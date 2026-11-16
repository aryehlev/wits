//! Turns a sampled profile into one answer: the cause, its share of the
//! process's time, the specific object, and who is to blame.

use crate::net::{self, Sock};
use crate::proc;
use crate::sample::{self, FdKind, Profile, Wait, EV_BLIND};
use crate::sys::{self, SysDelta, SysSnap};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cat {
    Cpu,
    CpuWait,
    Disk,
    Memory,
    Lock,
    Peer,
    Idle,
}

pub const CAUSES: [Cat; 6] = [
    Cat::Cpu,
    Cat::CpuWait,
    Cat::Disk,
    Cat::Memory,
    Cat::Lock,
    Cat::Peer,
];

impl Cat {
    pub fn key(self) -> &'static str {
        match self {
            Cat::Cpu => "cpu",
            Cat::CpuWait => "cpu-wait",
            Cat::Disk => "disk",
            Cat::Memory => "memory",
            Cat::Lock => "lock",
            Cat::Peer => "peer",
            Cat::Idle => "idle",
        }
    }

    /// "<name> is ..."
    pub fn headline(self) -> &'static str {
        match self {
            Cat::Cpu => "busy on CPU, not blocked",
            Cat::CpuWait => "waiting for a CPU",
            Cat::Disk => "waiting for disk",
            Cat::Memory => "short of memory",
            Cat::Lock => "waiting for a lock",
            Cat::Peer => "waiting for someone else",
            Cat::Idle => "idle",
        }
    }

    fn state(self) -> &'static str {
        match self {
            Cat::Cpu => "working",
            Cat::Peer => "waiting",
            Cat::Idle => "idle",
            _ => "stalled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Blame {
    /// "self" | "process" | "thread" | "cgroup" | "remote" | "device" | "unknown"
    pub kind: &'static str,
    pub pid: Option<i32>,
    pub name: Option<String>,
    pub text: String,
}

impl Blame {
    fn new(kind: &'static str, text: String) -> Blame {
        Blame {
            kind,
            pid: None,
            name: None,
            text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub pid: i32,
    pub name: String,
    pub cmdline: String,
    /// Parents wits walked through because they were only waiting for a child.
    pub followed: Vec<(i32, String)>,
    pub window_ms: u64,
    pub threads: usize,
    pub samples: u32,
    /// "working" | "stalled" | "waiting" | "idle" | "stopped" | "unknown"
    pub state: &'static str,
    pub cause: Option<Cat>,
    /// Share of the process's non-idle time taken by the cause (0..1).
    pub share: f64,
    /// Non-idle thread time divided by the window: 1.0 is one busy thread.
    pub active: f64,
    pub what: String,
    pub blame: Option<Blame>,
    /// Share of non-idle time for each of the six causes.
    pub breakdown: Vec<(Cat, f64)>,
    /// Fraction of all thread time that was idle, and what the idle time was.
    pub idle: f64,
    pub idle_what: String,
    pub confidence: &'static str,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy)]
pub struct Opts {
    pub dur: Duration,
    pub follow: bool,
    /// Skip system-wide scans and blame lookups (used when describing a
    /// blamed process in passing).
    pub light: bool,
}

struct Res {
    cat: Cat,
    label: String,
    fd: Option<i32>,
}

#[derive(PartialEq, Clone, Copy)]
enum Role {
    Conn,
    Listener,
    Tty,
    Wake,
    Dgram,
    File,
    Dev,
    Unknown,
}

struct Ctx<'a> {
    p: &'a Profile,
    socks: HashMap<u64, Sock>,
    same_ns: bool,
    link_count: HashMap<&'a str, usize>,
    /// The system swapped pages in during the window.
    swap_in: bool,
    /// The process's cgroup hit its memory limit during the window.
    limit_hit: bool,
}

fn fmt_ms(ms: u32) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

fn pct(x: f64) -> String {
    format!("{:.0}%", (x * 100.0).round())
}

impl<'a> Ctx<'a> {
    fn new(p: &'a Profile, swap_in: bool, limit_hit: bool) -> Ctx<'a> {
        let mut link_count: HashMap<&str, usize> = HashMap::new();
        for i in p.fds.values() {
            *link_count.entry(i.link.as_str()).or_default() += 1;
        }
        let any_sock = p.fds.values().any(|i| matches!(i.kind, FdKind::Socket(_)));
        Ctx {
            p,
            socks: if any_sock {
                net::sockets(p.pid)
            } else {
                HashMap::new()
            },
            same_ns: net::same_netns(p.pid),
            link_count,
            swap_in,
            limit_hit,
        }
    }

    fn role(&self, fd: i32) -> Role {
        let Some(i) = self.p.fds.get(&fd) else {
            return Role::Unknown;
        };
        match &i.kind {
            FdKind::Pipe | FdKind::Fifo => {
                if self.link_count.get(i.link.as_str()).copied().unwrap_or(0) >= 2 {
                    Role::Wake
                } else {
                    Role::Conn
                }
            }
            FdKind::Socket(ino) => match self.socks.get(ino) {
                Some(Sock::Tcp {
                    listening: true, ..
                }) => Role::Listener,
                Some(Sock::Tcp { .. }) => Role::Conn,
                Some(Sock::Udp { remote, .. }) => {
                    if remote.is_unset() {
                        Role::Dgram
                    } else {
                        Role::Conn
                    }
                }
                Some(Sock::Unix {
                    listening: true, ..
                }) => Role::Listener,
                Some(Sock::Unix {
                    stream: true,
                    connected: true,
                    ..
                }) => Role::Conn,
                _ => Role::Dgram,
            },
            FdKind::Tty => Role::Tty,
            FdKind::Anon => Role::Wake,
            FdKind::File | FdKind::BlockDev => Role::File,
            FdKind::CharDev | FdKind::Other => Role::Dev,
        }
    }

    fn sock_desc(&self, ino: u64) -> String {
        match self.socks.get(&ino) {
            Some(Sock::Tcp {
                local,
                listening: true,
                ..
            }) => format!("TCP listener on port {}", local.port),
            Some(Sock::Tcp { local, remote, .. }) => {
                let svc = net::port_name(remote.port)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                format!("TCP {local} → {remote}{svc}")
            }
            Some(Sock::Udp { local, remote }) => {
                if remote.is_unset() {
                    format!("UDP port {}", local.port)
                } else {
                    format!("UDP {local} → {remote}")
                }
            }
            Some(Sock::Unix { path, .. }) => {
                if !path.is_empty() {
                    return format!("Unix socket {path}");
                }
                let peer_path = if self.same_ns {
                    net::unix_peer(ino)
                } else {
                    None
                }
                .and_then(|pi| match self.socks.get(&pi) {
                    Some(Sock::Unix { path, .. }) if !path.is_empty() => Some(path.clone()),
                    _ => None,
                });
                match peer_path {
                    Some(p) => format!("Unix socket to {p}"),
                    None => "a Unix socket".into(),
                }
            }
            None => "a socket".into(),
        }
    }

    fn fd_desc(&self, fd: i32) -> String {
        let Some(i) = self.p.fds.get(&fd) else {
            return "a descriptor wits cannot see".into();
        };
        match &i.kind {
            FdKind::Pipe => "a pipe".into(),
            FdKind::Fifo => format!("FIFO {}", i.link),
            FdKind::Tty => format!("terminal {}", i.link),
            FdKind::Socket(ino) => self.sock_desc(*ino),
            _ => i.link.clone(),
        }
    }

    fn tcp_timing(&self, fd: i32) -> Option<net::TcpInfo> {
        if !self.same_ns {
            return None;
        }
        let FdKind::Socket(ino) = self.p.fds.get(&fd)?.kind else {
            return None;
        };
        match self.socks.get(&ino)? {
            Sock::Tcp {
                local,
                remote,
                listening: false,
                ..
            } => net::tcp_info(local, remote),
            _ => None,
        }
    }

    fn timing_suffix(&self, fd: i32) -> String {
        match self.tcp_timing(fd) {
            Some(t) if t.retransmits > 0 => "; retransmitting (packet loss or a dead peer)".into(),
            Some(t) if t.last_sent_ms <= t.last_recv_ms => {
                format!("; last sent {} ago, no reply since", fmt_ms(t.last_sent_ms))
            }
            Some(t) => format!("; nothing received for {}", fmt_ms(t.last_recv_ms)),
            None => String::new(),
        }
    }

    fn events(&self, kind: u8, a: u64, n: u64) -> Res {
        let idle = |label: &str| Res {
            cat: Cat::Idle,
            label: label.to_string(),
            fd: None,
        };
        let watched: Vec<i32> = match self.p.evsets.get(&(kind, a, n)) {
            Some(Some(v)) => v.clone(),
            // The watched set is not visible: fall back to every descriptor.
            _ if self.p.fds_readable => {
                let mut v: Vec<i32> = self.p.fds.keys().copied().collect();
                v.sort_unstable();
                v
            }
            _ => return idle("waiting for events wits cannot see"),
        };
        let exact = matches!(self.p.evsets.get(&(kind, a, n)), Some(Some(_)));
        let roles: Vec<(i32, Role)> = watched
            .iter()
            .map(|&fd| (fd, self.role(fd)))
            .filter(|(_, r)| !matches!(r, Role::Wake | Role::Unknown | Role::File | Role::Dev))
            .collect();
        if roles.is_empty() {
            return idle("waiting on its own timers and events");
        }
        if roles.iter().any(|(_, r)| *r == Role::Tty) {
            return idle("waiting for terminal input");
        }
        let conns: Vec<i32> = roles
            .iter()
            .filter(|(_, r)| *r == Role::Conn)
            .map(|x| x.0)
            .collect();
        let listener = roles
            .iter()
            .find(|(_, r)| *r == Role::Listener)
            .map(|x| x.0);
        if conns.is_empty() {
            return match listener {
                Some(fd) => idle(&format!(
                    "waiting for new connections ({})",
                    self.fd_desc(fd)
                )),
                None => idle("waiting for datagrams or events"),
            };
        }
        let verb = if exact {
            "polling"
        } else {
            "waiting for events; it holds"
        };
        if listener.is_none() && roles.len() <= 2 {
            let fd = conns[0];
            return Res {
                cat: Cat::Peer,
                label: format!("{verb} {}{}", self.fd_desc(fd), self.timing_suffix(fd)),
                fd: Some(fd),
            };
        }
        // An event loop with many connections: only call it waiting if some
        // connection has a request out with no reply.
        for &fd in conns.iter().take(32) {
            if let Some(t) = self.tcp_timing(fd) {
                if t.last_sent_ms <= t.last_recv_ms && t.last_sent_ms < 120_000 {
                    return Res {
                        cat: Cat::Peer,
                        label: format!("{verb} {}{}", self.fd_desc(fd), self.timing_suffix(fd)),
                        fd: Some(fd),
                    };
                }
            }
        }
        idle(&format!(
            "an event loop watching {} connections, none with an unanswered request",
            conns.len()
        ))
    }

    fn resolve(&self, w: &Wait) -> Res {
        let r = |cat, label: String, fd| Res { cat, label, fd };
        match w {
            Wait::Disk { sys, fd } => {
                let on = match self.p.fds.get(fd) {
                    Some(i) => format!(" on {}", i.link),
                    None => String::new(),
                };
                let what = if sys.is_empty() {
                    "uninterruptible I/O".to_string()
                } else {
                    format!("{sys}(){on}")
                };
                r(Cat::Disk, what, Some(*fd).filter(|f| *f >= 0))
            }
            Wait::Fault => {
                if self.swap_in {
                    r(
                        Cat::Memory,
                        "pages being read back in from swap".into(),
                        None,
                    )
                } else if self.limit_hit {
                    r(
                        Cat::Memory,
                        "its pages keep being evicted and read back from disk".into(),
                        None,
                    )
                } else {
                    r(
                        Cat::Disk,
                        "page faults reading its own code or mapped files from disk".into(),
                        None,
                    )
                }
            }
            Wait::Mem { why } => r(
                Cat::Memory,
                format!("reclaiming memory in the kernel ({why})"),
                None,
            ),
            Wait::Futex {
                lockish: true,
                addr,
                pi,
            } => {
                let k = if *pi {
                    "priority-inheritance mutex"
                } else {
                    "mutex"
                };
                let at = if *addr != 0 {
                    format!(" at {addr:#x}")
                } else {
                    String::new()
                };
                r(Cat::Lock, format!("a {k}{at} inside the process"), None)
            }
            Wait::Futex { .. } => r(
                Cat::Idle,
                "parked threads (idle or waiting on a condition)".into(),
                None,
            ),
            Wait::FileLock { fd } => {
                let on = match self.p.fds.get(fd) {
                    Some(i) => format!("the file lock on {}", i.link),
                    None => "a file lock".into(),
                };
                r(Cat::Lock, on, Some(*fd).filter(|f| *f >= 0))
            }
            Wait::KLock { why } => r(Cat::Lock, format!("a lock inside the kernel ({why})"), None),
            Wait::Io { fd, write, sys } => {
                let desc = self.fd_desc(*fd);
                match (self.role(*fd), *write) {
                    (Role::Conn, false) => r(
                        Cat::Peer,
                        format!(
                            "blocked in {sys}() on fd {fd}, {desc}{}",
                            self.timing_suffix(*fd)
                        ),
                        Some(*fd),
                    ),
                    (Role::Conn, true) | (Role::Tty, true) => r(
                        Cat::Peer,
                        format!(
                            "blocked in {sys}() to fd {fd}, {desc}: the other end is not reading"
                        ),
                        Some(*fd),
                    ),
                    (Role::Tty, false) => r(Cat::Idle, "waiting for terminal input".into(), None),
                    (Role::Listener, _) => r(
                        Cat::Idle,
                        format!("waiting for new connections ({desc})"),
                        None,
                    ),
                    (Role::Wake, _) => r(
                        Cat::Idle,
                        "waiting on its own timers and events".into(),
                        None,
                    ),
                    (Role::Dgram, _) => {
                        r(Cat::Idle, format!("waiting for datagrams on {desc}"), None)
                    }
                    (Role::File, _) => r(Cat::Disk, format!("{sys}() on {desc}"), Some(*fd)),
                    (Role::Dev, _) => r(Cat::Idle, format!("waiting on device {desc}"), None),
                    (Role::Unknown, _) => r(
                        Cat::Idle,
                        format!("blocked in {sys}() on fd {fd}, which wits cannot see"),
                        None,
                    ),
                }
            }
            Wait::Connect { fd } => r(
                Cat::Peer,
                format!("connecting: {}", self.fd_desc(*fd)),
                Some(*fd),
            ),
            Wait::Open { ptr } => {
                let path = self
                    .p
                    .strs
                    .get(ptr)
                    .cloned()
                    .unwrap_or_else(|| "a FIFO".into());
                r(
                    Cat::Peer,
                    format!("opening {path}: nobody has opened the other end"),
                    None,
                )
            }
            Wait::Events { kind, a, n } => self.events(*kind, *a, *n),
            Wait::Ipc { sys } => r(
                Cat::Peer,
                format!("blocked in {sys}() on a System V or POSIX IPC object"),
                None,
            ),
            Wait::Child => r(
                Cat::Peer,
                "waiting for its child processes to finish".into(),
                None,
            ),
            Wait::Sleep => r(Cat::Idle, "asleep on its own timer".into(), None),
            Wait::Accept { fd } => {
                let d = if *fd >= 0 {
                    format!(" ({})", self.fd_desc(*fd))
                } else {
                    String::new()
                };
                r(Cat::Idle, format!("waiting for new connections{d}"), None)
            }
            Wait::Stopped => r(
                Cat::Idle,
                "stopped by a signal (Ctrl-Z, SIGSTOP or a debugger)".into(),
                None,
            ),
            Wait::Park { what } => r(Cat::Idle, format!("waiting for {what}"), None),
            Wait::Blind { what } => {
                // Guess the descriptor when there is only one candidate.
                let mut c: Vec<i32> = self
                    .p
                    .fds
                    .iter()
                    .filter(|(fd, i)| match (*what, &i.kind) {
                        ("pipe", FdKind::Pipe | FdKind::Fifo) => true,
                        ("socket", FdKind::Socket(_)) => self.role(**fd) == Role::Conn,
                        _ => false,
                    })
                    .map(|(fd, _)| *fd)
                    .collect();
                c.sort_unstable();
                let has_tty = self.p.fds.values().any(|i| i.kind == FdKind::Tty);
                if c.is_empty() && *what == "socket" && has_tty {
                    r(Cat::Idle, "waiting for terminal input".into(), None)
                } else if c.len() == 1 {
                    r(
                        Cat::Peer,
                        format!("blocked on fd {}, {}", c[0], self.fd_desc(c[0])),
                        Some(c[0]),
                    )
                } else {
                    r(Cat::Peer, format!("blocked on a {what}"), None)
                }
            }
            Wait::Unknown { d: true, why } => {
                let w = if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({why})")
                };
                r(
                    Cat::Disk,
                    format!("uninterruptible wait in the kernel{w}"),
                    None,
                )
            }
            Wait::Unknown { .. } => r(
                Cat::Idle,
                "blocked on something wits cannot see".into(),
                None,
            ),
        }
    }
}

// ---- finding other processes ----------------------------------------------

fn ancestors(pid: i32) -> HashSet<i32> {
    let mut s = HashSet::new();
    let mut cur = pid;
    for _ in 0..64 {
        match proc::stat(cur) {
            Some(st) if st.ppid > 0 => {
                s.insert(st.ppid);
                cur = st.ppid;
            }
            _ => break,
        }
    }
    s
}

/// Other processes holding a descriptor whose /proc link is `link`.
/// `want_writers`: Some(true) keeps only write-capable holders, Some(false)
/// only read-capable ones.
fn holders(link: &str, exclude: i32, want_writers: Option<bool>) -> Vec<i32> {
    let mut out = Vec::new();
    for pid in proc::pids() {
        if pid == exclude {
            continue;
        }
        let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(l) = fs::read_link(e.path()) else {
                continue;
            };
            if l.to_string_lossy() != link {
                continue;
            }
            if let Some(w) = want_writers {
                let name = e.file_name();
                let acc = proc::slurp(&format!("/proc/{pid}/fdinfo/{}", name.to_string_lossy()))
                    .and_then(|s| {
                        s.lines()
                            .find_map(|l| l.strip_prefix("flags:"))
                            .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
                    })
                    .map(|f| f & 3);
                let ok = match acc {
                    Some(0) => !w,
                    Some(1) => w,
                    _ => true,
                };
                if !ok {
                    continue;
                }
            }
            out.push(pid);
            break;
        }
    }
    out
}

fn pick(mut c: Vec<i32>, target: i32) -> Option<(i32, usize)> {
    if c.is_empty() {
        return None;
    }
    let anc = ancestors(target);
    c.sort_unstable();
    let n = c.len();
    let best = c.iter().copied().find(|p| !anc.contains(p)).unwrap_or(c[0]);
    Some((best, n - 1))
}

/// One short phrase on what another process is doing right now.
fn doing(pid: i32) -> Option<String> {
    if waiting_on_child(pid) {
        if let Some((c, _)) = newest_child(pid) {
            return Some(format!(
                "only waiting for its child pid {c} ({})",
                proc::comm(c).unwrap_or_default()
            ));
        }
    }
    let a = analyze(
        pid,
        Opts {
            dur: Duration::from_millis(200),
            follow: false,
            light: true,
        },
    )?;
    Some(match a.cause {
        Some(Cat::Cpu) => "busy on CPU".into(),
        Some(Cat::Peer) => "itself waiting for someone else".into(),
        Some(Cat::Memory) => "itself short of memory".into(),
        Some(c) => format!("itself {}", c.headline()),
        None if a.state == "stopped" => "stopped by a signal".into(),
        None => a.idle_what.clone(),
    })
}

fn process_blame(pid: i32, more: usize, lead: &str) -> Blame {
    let name = proc::label(pid);
    let d = doing(pid)
        .map(|d| format!(", which is {d}"))
        .unwrap_or_default();
    let extra = if more > 0 {
        format!(" (and {more} more)")
    } else {
        String::new()
    };
    Blame {
        kind: "process",
        pid: Some(pid),
        name: Some(name.clone()),
        text: format!("{lead}pid {pid} ({name}){d}{extra}"),
    }
}

fn newest_child(pid: i32) -> Option<(i32, usize)> {
    let mut kids: Vec<(u64, i32)> = proc::children(pid)
        .into_iter()
        .filter_map(|c| {
            let s = proc::stat(c)?;
            (s.state != 'Z').then_some((s.starttime, c))
        })
        .collect();
    kids.sort_unstable();
    kids.last().map(|k| (k.1, kids.len() - 1))
}

fn peer_blame(cx: &Ctx, res: &Res, wait: &Wait) -> Blame {
    let pid = cx.p.pid;
    if matches!(wait, Wait::Child) {
        return match newest_child(pid) {
            Some((c, more)) => process_blame(c, more, "its child "),
            None => Blame::new("unknown", "a child process that has already gone".into()),
        };
    }
    let unseen = || {
        Blame::new(
            "unknown",
            "the other end is held by a process wits cannot see (another user's?)".into(),
        )
    };
    let Some(fd) = res.fd else {
        return Blame::new(
            "unknown",
            "wits cannot tell which process is at the other end".into(),
        );
    };
    let Some(info) = cx.p.fds.get(&fd) else {
        return unseen();
    };
    let writing = matches!(wait, Wait::Io { write: true, .. });
    match &info.kind {
        FdKind::Pipe | FdKind::Fifo => match pick(holders(&info.link, pid, Some(!writing)), pid) {
            Some((p, more)) => process_blame(p, more, ""),
            None => unseen(),
        },
        FdKind::Tty => Blame::new(
            "unknown",
            "the terminal (or whoever reads it) is not consuming output".into(),
        ),
        FdKind::Socket(ino) => match cx.socks.get(ino) {
            Some(Sock::Tcp { local, remote, .. }) => {
                let local_ips: HashSet<_> = cx
                    .socks
                    .values()
                    .filter_map(|s| match s {
                        Sock::Tcp { local, .. } => Some(local.ip()),
                        _ => None,
                    })
                    .collect();
                let is_local = remote.ip().is_loopback() || local_ips.contains(&remote.ip());
                if !is_local {
                    let svc = net::port_name(remote.port)
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default();
                    return Blame::new(
                        "remote",
                        format!("remote host {remote}{svc}: not a process on this machine"),
                    );
                }
                let mut peer_ino = None;
                let mut listen_ino = None;
                for (i, s) in &cx.socks {
                    if let Sock::Tcp {
                        local: l,
                        remote: r,
                        listening,
                        ..
                    } = s
                    {
                        if !listening
                            && l.port == remote.port
                            && r.port == local.port
                            && l.ip() == remote.ip()
                        {
                            peer_ino = Some(*i);
                        } else if *listening && l.port == remote.port {
                            listen_ino = Some(*i);
                        }
                    }
                }
                for i in [peer_ino, listen_ino].into_iter().flatten() {
                    if let Some((p, more)) = pick(holders(&format!("socket:[{i}]"), pid, None), pid)
                    {
                        return process_blame(p, more, "");
                    }
                }
                Blame::new(
                    "unknown",
                    format!(
                        "a local process on port {} that wits cannot see (another user's?)",
                        remote.port
                    ),
                )
            }
            Some(Sock::Unix { .. }) => {
                let peer = if cx.same_ns {
                    net::unix_peer(*ino)
                } else {
                    None
                };
                match peer.and_then(|pi| pick(holders(&format!("socket:[{pi}]"), pid, None), pid)) {
                    Some((p, more)) => process_blame(p, more, ""),
                    None => unseen(),
                }
            }
            Some(Sock::Udp { remote, .. }) => {
                Blame::new("remote", format!("whoever answers at {remote}"))
            }
            None => unseen(),
        },
        _ => unseen(),
    }
}

fn lock_blame(
    cx: &Ctx,
    res: &Res,
    wait: &Wait,
    per_thread: &HashMap<i32, Vec<(Cat, String, f64)>>,
) -> Blame {
    let p = cx.p;
    match wait {
        Wait::FileLock { .. } => {
            let locks = sys::parse_locks(p.locks.as_deref().unwrap_or(""));
            let mine: HashSet<i32> = std::iter::once(p.pid)
                .chain(p.threads.keys().copied())
                .collect();
            // Which inode are we queued on? Prefer the descriptor we saw;
            // otherwise the kernel's own list of blocked waiters.
            let ino = res
                .fd
                .and_then(|fd| p.fds.get(&fd))
                .map(|i| i.ino)
                .filter(|i| *i != 0)
                .or_else(|| {
                    locks
                        .iter()
                        .find(|l| l.waiting && mine.contains(&l.pid))
                        .map(|l| l.ino)
                });
            let holder = ino.and_then(|ino| {
                locks
                    .iter()
                    .filter(|l| !l.waiting && l.ino == ino && !mine.contains(&l.pid))
                    .map(|l| l.pid)
                    .next()
            });
            match holder {
                Some(h) if h > 0 && proc::stat(h).is_some() => process_blame(h, 0, "held by "),
                Some(_) => Blame::new(
                    "unknown",
                    "held through a descriptor whose owner is not visible from here".into(),
                ),
                None => Blame::new("unknown", "the holder is not listed in /proc/locks".into()),
            }
        }
        Wait::Futex { addr, .. } => {
            let waiters = p
                .threads
                .values()
                .filter(|t| t.waits.contains_key(wait))
                .count();
            let w = if waiters > 1 {
                format!("{waiters} threads queue for it; ")
            } else {
                String::new()
            };
            let owner = p
                .owners
                .get(addr)
                .and_then(|m| m.iter().max_by_key(|x| x.1))
                .map(|x| *x.0);
            match owner {
                Some(tid) => {
                    let tname = p.threads.get(&tid).map(|t| t.comm.clone()).unwrap_or_default();
                    let busy = per_thread
                        .get(&tid)
                        .and_then(|v| v.iter().max_by(|a, b| a.2.total_cmp(&b.2)))
                        .map(|(cat, label, _)| match cat {
                            Cat::Cpu => ", which is busy on CPU".to_string(),
                            Cat::Idle => format!(", which is itself {label}"),
                            c => format!(", which is itself {} ({label})", c.headline()),
                        })
                        .unwrap_or_default();
                    Blame {
                        kind: "thread",
                        pid: Some(tid),
                        name: Some(tname.clone()),
                        text: format!("{w}held by its own thread {tid} ({tname}){busy}"),
                    }
                }
                None => Blame::new(
                    "thread",
                    format!("{w}held by another thread of the same process (owner not recorded in the lock)"),
                ),
            }
        }
        _ => Blame::new(
            "unknown",
            "kernel locks do not record an owner that wits can read".into(),
        ),
    }
}

fn cpu_wait_answer(
    p: &Profile,
    delta: Option<&SysDelta>,
    throttle: Option<(sys::CgLevel, f64)>,
    rq_ns: f64,
) -> (String, Blame) {
    if let Some((cg, ns)) = throttle {
        if ns >= 0.2 * rq_ns {
            let lim = sys::cpu_limit(&cg)
                .map(|l| format!("{l:.2} CPUs"))
                .unwrap_or_else(|| "a CPU quota".into());
            return (
                format!("the CPU limit of its cgroup {}: {lim}", cg.name),
                Blame::new(
                    "cgroup",
                    format!(
                        "the limit itself, not another process: the cgroup was throttled for {:.1}s of the window",
                        ns / 1e9
                    ),
                ),
            );
        }
    }
    let (runnable, cpus) = sys::load_now();
    let allowed = proc::status_field(p.pid, "Cpus_allowed_list")
        .map(|s| proc::parse_cpu_list(&s))
        .unwrap_or_default();
    let pinned = !allowed.is_empty() && (allowed.len() as u32) < cpus;
    let mut what = if pinned {
        let list = proc::status_field(p.pid, "Cpus_allowed_list").unwrap_or_default();
        format!("runnable but queued: it may only use CPU {list}, which is taken")
    } else {
        format!("runnable but queued: {runnable} tasks want {cpus} CPUs")
    };
    if p.nice > 0 {
        what.push_str(&format!("; it runs at nice +{}", p.nice));
    }
    let Some(d) = delta else {
        return (
            what,
            Blame::new("unknown", "other runnable processes".into()),
        );
    };
    let mine: HashSet<u32> = allowed.iter().copied().collect();
    let mut hogs = d
        .cpu
        .iter()
        .filter(|(pid, _)| *pid != p.pid && *pid != std::process::id() as i32)
        .filter(|(pid, _)| {
            if !pinned {
                return true;
            }
            // Pinned: only processes running on our CPUs compete with us.
            proc::stat(*pid)
                .map(|s| mine.contains(&s.processor))
                .unwrap_or(false)
        });
    let blame = match hogs.next() {
        Some(&(hp, secs)) if secs / d.secs >= 0.1 => {
            let name = proc::label(hp);
            let second = hogs
                .next()
                .filter(|(_, s)| s / d.secs >= 0.1)
                .map(|(p2, s2)| {
                    format!("; next pid {p2} ({}) {:.1}", proc::label(*p2), s2 / d.secs)
                })
                .unwrap_or_default();
            Blame {
                kind: "process",
                pid: Some(hp),
                name: Some(name.clone()),
                text: format!(
                    "pid {hp} ({name}) used {:.1} CPUs in the same window{second}",
                    secs / d.secs
                ),
            }
        }
        _ => Blame::new(
            "unknown",
            "no single process: many small ones are sharing the CPUs".into(),
        ),
    };
    (what, blame)
}

fn disk_answer(p: &Profile, res: &Res, delta: Option<&SysDelta>) -> (String, Blame) {
    let Some(d) = delta else {
        return (
            res.label.clone(),
            Blame::new("unknown", "the storage device".into()),
        );
    };
    let dev = res
        .fd
        .and_then(|fd| p.fds.get(&fd))
        .and_then(|i| {
            let k = (libc::major(i.dev), libc::minor(i.dev));
            d.disk_util.get(&k).map(|(n, u)| (n.clone(), *u))
        })
        .or_else(|| d.busiest_disk().map(|(n, u)| (n.to_string(), u)));
    let devtxt = dev.as_ref().map(|(n, u)| format!("{n} ({} busy)", pct(*u)));
    let what = match &devtxt {
        Some(t) => format!("{}, device {t}", res.label),
        None => res.label.clone(),
    };
    let own = d.io.get(&p.pid).copied().unwrap_or((0, 0));
    let other =
        d.io.iter()
            .filter(|(pid, _)| **pid != p.pid && **pid != std::process::id() as i32)
            .max_by_key(|(_, v)| v.0 + v.1)
            .map(|(pid, v)| (*pid, *v));
    let rw = |v: (u64, u64)| {
        format!(
            "read {}, wrote {}",
            proc::fmt_bytes(v.0),
            proc::fmt_bytes(v.1)
        )
    };
    let blame = match other {
        Some((op, v)) if v.0 + v.1 >= (own.0 + own.1).max(1 << 20) => {
            let name = proc::label(op);
            Blame {
                kind: "process",
                pid: Some(op),
                name: Some(name.clone()),
                text: format!("pid {op} ({name}) did the most I/O in the same window: {}", rw(v)),
            }
        }
        _ if own.0 + own.1 > 0 => Blame::new(
            "self",
            format!("its own I/O: it {}; no other visible process is doing more", rw(own)),
        ),
        _ => Blame::new(
            "device",
            "no visible culprit: the device is slow or busy with I/O wits cannot see (other users' needs sudo)".into(),
        ),
    };
    (what, blame)
}

fn memory_answer(
    p: &Profile,
    res: &Res,
    delta: Option<&SysDelta>,
    hit: Option<(sys::CgLevel, u64)>,
    majflt: u64,
) -> (String, Blame) {
    let page = proc::page_size();
    let mut what = res.label.clone();
    if majflt > 0 {
        what.push_str(&format!("; {majflt} major page faults"));
    }
    if let Some(d) = delta {
        if d.swapped_in_pages + d.swapped_out_pages > 0 {
            what.push_str(&format!(
                "; system swapped in {}, out {}",
                proc::fmt_bytes(d.swapped_in_pages * page),
                proc::fmt_bytes(d.swapped_out_pages * page)
            ));
        }
    }
    if let Some((cg, hits)) = hit {
        let lim = sys::mem_limit(&cg)
            .map(|(cur, lim)| {
                format!(
                    ": using {} of a {} limit",
                    proc::fmt_bytes(cur),
                    proc::fmt_bytes(lim)
                )
            })
            .unwrap_or_default();
        return (
            what,
            Blame::new(
                "cgroup",
                format!(
                    "the memory limit of its cgroup {}{lim}; hit {hits} times in the window",
                    cg.name
                ),
            ),
        );
    }
    let Some(d) = delta else {
        return (
            what,
            Blame::new("unknown", "memory pressure on the machine".into()),
        );
    };
    let total = sys::mem_total_bytes();
    let blame = match d.rss_pages.iter().max_by_key(|x| x.1) {
        Some((&big, &pages)) if big == p.pid => Blame::new(
            "self",
            format!(
                "itself: it holds {} of {} RAM",
                proc::fmt_bytes(pages * page),
                proc::fmt_bytes(total)
            ),
        ),
        Some((&big, &pages)) => {
            let name = proc::label(big);
            Blame {
                kind: "process",
                pid: Some(big),
                name: Some(name.clone()),
                text: format!(
                    "pid {big} ({name}) holds the most memory: {} of {} RAM",
                    proc::fmt_bytes(pages * page),
                    proc::fmt_bytes(total)
                ),
            }
        }
        None => Blame::new("unknown", "memory pressure on the machine".into()),
    };
    (what, blame)
}

fn waiting_on_child(pid: i32) -> bool {
    let Some(st) = proc::stat(pid) else {
        return false;
    };
    if st.num_threads != 1 {
        return false;
    }
    let sc = proc::slurp(&format!("/proc/{pid}/syscall")).and_then(|s| proc::parse_syscall(&s));
    let wchan = proc::slurp(&format!("/proc/{pid}/wchan"))
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty() && w != "0");
    sample::classify(st.state, sc, wchan.as_deref()) == Some(Wait::Child)
}

#[derive(Hash, PartialEq, Eq, Clone)]
enum Key {
    Cpu,
    Rq,
    W(Wait),
}

/// Split one thread's wall time into on-CPU, queued, and each observed wait.
fn split_thread(th: &sample::Th, wall: f64, tck: f64) -> Vec<(Key, f64)> {
    let (mut cpu, mut rq) = match (th.sched0, th.sched1) {
        (Some(a), Some(b)) => (
            b.0.saturating_sub(a.0) as f64,
            b.1.saturating_sub(a.1) as f64,
        ),
        _ => {
            // No scheduler statistics: estimate from CPU ticks and samples.
            let cpu = ((th.utime.1 + th.stime.1).saturating_sub(th.utime.0 + th.stime.0)) as f64
                / tck
                * 1e9;
            let run = wall * th.running as f64 / th.n.max(1) as f64;
            (cpu, (run - cpu).max(0.0))
        }
    };
    if cpu + rq > wall && cpu + rq > 0.0 {
        let k = wall / (cpu + rq);
        cpu *= k;
        rq *= k;
    }
    let rest = wall - cpu - rq;
    let blocked: f64 = th.waits.values().map(|c| *c as f64).sum();
    let mut out = vec![(Key::Cpu, cpu)];
    if blocked > 0.0 {
        for (w, c) in &th.waits {
            out.push((Key::W(w.clone()), rest * *c as f64 / blocked));
        }
    } else if th.running * 2 > th.n {
        // Runnable throughout, yet the counters leave a gap: time the kernel
        // does not credit to the task (a hypervisor took the CPU away).
        rq += rest;
    } else if rest > 0.0 {
        out.push((
            Key::W(Wait::Park {
                what: "brief sleeps",
            }),
            rest,
        ));
    }
    out.push((Key::Rq, rq));
    out
}

pub fn analyze(pid: i32, opts: Opts) -> Option<Answer> {
    let mut pid = pid;
    let mut followed = Vec::new();
    if opts.follow {
        for _ in 0..8 {
            if !waiting_on_child(pid) {
                break;
            }
            let Some((child, _)) = newest_child(pid) else {
                break;
            };
            followed.push((pid, proc::comm(pid).unwrap_or_default()));
            pid = child;
        }
    }

    let cpu_chain = if opts.light {
        Vec::new()
    } else {
        sys::cgroup_chain(pid, "cpu")
    };
    let mem_chain = if opts.light {
        Vec::new()
    } else {
        sys::cgroup_chain(pid, "memory")
    };
    let thr0 = sys::throttled_ns(&cpu_chain);
    let hits0 = sys::mem_limit_hits(&mem_chain);
    let snap0 = (!opts.light).then(SysSnap::take);

    let p = sample::sample(pid, opts.dur)?;

    let delta = snap0.map(|a| SysDelta::between(&a, &SysSnap::take()));
    let thr1 = sys::throttled_ns(&cpu_chain);
    let hits1 = sys::mem_limit_hits(&mem_chain);
    let throttle = cpu_chain
        .iter()
        .zip(thr0.iter().zip(thr1.iter()))
        .map(|(c, (a, b))| (c.clone(), b.saturating_sub(*a) as f64))
        .filter(|x| x.1 > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1));
    let mem_hit = mem_chain
        .iter()
        .zip(hits0.iter().zip(hits1.iter()))
        .map(|(c, (a, b))| (c.clone(), b.saturating_sub(*a)))
        .filter(|x| x.1 > 0)
        .max_by_key(|x| x.1);

    if p.ticks == 0 || p.threads.is_empty() {
        return None;
    }
    let window_ns = p.window.as_nanos() as f64;
    let tck = proc::clk_tck() as f64;
    let swap_in = delta
        .as_ref()
        .map(|d| d.swapped_in_pages > 0)
        .unwrap_or(false);
    let cx = Ctx::new(&p, swap_in, mem_hit.is_some());

    // Resolve each distinct wait once.
    let mut resolved: HashMap<Wait, Res> = HashMap::new();
    for th in p.threads.values() {
        for w in th.waits.keys() {
            if !resolved.contains_key(w) {
                resolved.insert(w.clone(), cx.resolve(w));
            }
        }
    }
    let park = Wait::Park {
        what: "brief sleeps",
    };
    resolved.insert(park.clone(), cx.resolve(&park));

    let mut cat_ns: HashMap<Cat, f64> = HashMap::new();
    let mut wait_ns: HashMap<Wait, f64> = HashMap::new();
    let mut per_thread: HashMap<i32, Vec<(Cat, String, f64)>> = HashMap::new();
    let mut total = 0.0;
    let mut samples = 0;
    let mut hottest: Option<(i32, f64)> = None;
    let mut majflt = 0;
    let (mut ut, mut st) = (0u64, 0u64);
    for (tid, th) in &p.threads {
        samples += th.n;
        let wall = window_ns * th.n as f64 / p.ticks as f64;
        total += wall;
        majflt += th.majflt.1.saturating_sub(th.majflt.0);
        ut += th.utime.1.saturating_sub(th.utime.0);
        st += th.stime.1.saturating_sub(th.stime.0);
        let mut mine = Vec::new();
        let mut parts = split_thread(th, wall, tck);
        // While the cgroup is at its memory limit, the kernel CPU time of a
        // thread seen faulting is the kernel reclaiming memory on its behalf.
        let faulting = th
            .waits
            .keys()
            .find(|w| matches!(w, Wait::Fault | Wait::Mem { .. }))
            .cloned();
        if let (Some(w), true) = (faulting, mem_hit.is_some()) {
            let sys_ns = th.stime.1.saturating_sub(th.stime.0) as f64 / tck * 1e9;
            let mut moved = 0.0;
            for part in parts.iter_mut().filter(|x| x.0 == Key::Cpu) {
                moved = part.1.min(sys_ns);
                part.1 -= moved;
            }
            parts.push((Key::W(w), moved));
        }
        for (k, ns) in parts {
            let (cat, label) = match &k {
                Key::Cpu => {
                    if hottest.map(|h| ns > h.1).unwrap_or(true) {
                        hottest = Some((*tid, ns));
                    }
                    (Cat::Cpu, String::new())
                }
                Key::Rq => (Cat::CpuWait, String::new()),
                Key::W(w) => {
                    *wait_ns.entry(w.clone()).or_default() += ns;
                    let r = &resolved[w];
                    (r.cat, r.label.clone())
                }
            };
            *cat_ns.entry(cat).or_default() += ns;
            mine.push((cat, label, ns));
        }
        per_thread.insert(*tid, mine);
    }

    let idle_ns = cat_ns.get(&Cat::Idle).copied().unwrap_or(0.0);
    let active_ns = (total - idle_ns).max(0.0);
    let top_wait = |cat: Cat| -> Option<(&Wait, &Res)> {
        // Group by label so the same object seen through several waits adds up.
        let mut by_label: HashMap<&str, (f64, &Wait, f64)> = HashMap::new();
        for (w, ns) in &wait_ns {
            let r = &resolved[w];
            if r.cat == cat {
                let e = by_label.entry(r.label.as_str()).or_insert((0.0, w, 0.0));
                e.0 += ns;
                if *ns > e.2 {
                    e.1 = w;
                    e.2 = *ns;
                }
            }
        }
        by_label
            .into_values()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, w, _)| (w, &resolved[w]))
    };
    let idle_what = top_wait(Cat::Idle)
        .map(|x| x.1.label.clone())
        .unwrap_or_else(|| "idle".into());

    let mut breakdown: Vec<(Cat, f64)> = CAUSES
        .iter()
        .map(|c| {
            (
                *c,
                cat_ns.get(c).copied().unwrap_or(0.0) / active_ns.max(1.0),
            )
        })
        .collect();
    breakdown.sort_by(|a, b| b.1.total_cmp(&a.1));

    let stopped_ns = wait_ns.get(&Wait::Stopped).copied().unwrap_or(0.0);
    let mut notes = Vec::new();
    if p.gone {
        notes.push("the process exited while wits was watching".to_string());
    }
    if p.capped {
        notes.push("only the first 512 threads were sampled".to_string());
    }
    let blind = !p.saw_syscall && p.threads.values().any(|t| !t.waits.is_empty());
    if blind {
        notes.push(format!(
            "limited view: the kernel would not show what pid {pid} is blocked in (another user's process, or ptrace_scope); try sudo"
        ));
    }

    let mut ans = Answer {
        pid,
        name: p.comm.clone(),
        cmdline: proc::cmdline(pid),
        followed,
        window_ms: p.window.as_millis() as u64,
        threads: p.threads.len(),
        samples,
        state: "idle",
        cause: None,
        share: 0.0,
        active: active_ns / window_ns.max(1.0),
        what: String::new(),
        blame: None,
        breakdown,
        idle: idle_ns / total.max(1.0),
        idle_what,
        confidence: "high",
        notes,
    };

    if stopped_ns > 0.5 * total {
        ans.state = "stopped";
        ans.idle_what = resolved[&Wait::Stopped].label.clone();
        return Some(ans);
    }
    // Less than 5% of one thread's time doing or waiting for anything.
    if active_ns < 0.05 * window_ns {
        // Blocked on something hidden from us, or parked on a futex we
        // cannot classify as lock or idle: do not call it idle.
        let hidden = matches!(
            top_wait(Cat::Idle),
            Some((
                Wait::Unknown { .. } | Wait::Futex { .. } | Wait::Events { kind: EV_BLIND, .. },
                _
            ))
        );
        if blind {
            ans.confidence = "low";
            if hidden {
                ans.state = "unknown";
            }
        }
        return Some(ans);
    }

    let (cause, share) = ans.breakdown[0];
    ans.cause = Some(cause);
    ans.share = share;
    ans.state = cause.state();
    let d = delta.as_ref();
    let (what, blame) = match cause {
        Cat::Cpu => {
            let cpus = cat_ns[&Cat::Cpu] / window_ns;
            let kern = if ut + st > 0 {
                st as f64 / (ut + st) as f64
            } else {
                0.0
            };
            let mut what = if kern >= 0.5 {
                format!("{} of that CPU time is inside the kernel (system calls), the rest its own code", pct(kern))
            } else {
                format!(
                    "running its own code ({} user, {} kernel)",
                    pct(1.0 - kern),
                    pct(kern)
                )
            };
            if p.threads.len() > 1 {
                if let Some((tid, ns)) = hottest {
                    let n = p.threads.get(&tid).map(|t| t.comm.as_str()).unwrap_or("?");
                    what.push_str(&format!(
                        "; hottest thread {tid} ({n}) at {}",
                        pct(ns / window_ns)
                    ));
                }
            }
            (
                what,
                Blame::new(
                    "self",
                    format!("itself: nothing is holding it back; it is using {cpus:.2} CPUs"),
                ),
            )
        }
        Cat::CpuWait => cpu_wait_answer(&p, d, throttle, cat_ns[&Cat::CpuWait]),
        Cat::Disk | Cat::Memory | Cat::Lock | Cat::Peer => {
            let (wait, res) = top_wait(cause)?;
            match cause {
                Cat::Disk => disk_answer(&p, res, d),
                Cat::Memory => memory_answer(&p, res, d, mem_hit, majflt),
                Cat::Lock => {
                    let b = if opts.light {
                        Blame::new("unknown", String::new())
                    } else {
                        lock_blame(&cx, res, wait, &per_thread)
                    };
                    (res.label.clone(), b)
                }
                _ => {
                    let b = if opts.light {
                        Blame::new("unknown", String::new())
                    } else {
                        peer_blame(&cx, res, wait)
                    };
                    (res.label.clone(), b)
                }
            }
        }
        Cat::Idle => unreachable!(),
    };
    ans.what = what;
    ans.blame = Some(blame);
    ans.confidence = if blind && matches!(cause, Cat::Lock | Cat::Peer | Cat::Disk | Cat::Memory) {
        "low"
    } else if samples < 30 || share < 0.6 || blind {
        "medium"
    } else {
        "high"
    };
    // EV_BLIND waits could not be inspected.
    if matches!(
        top_wait(cause),
        Some((Wait::Events { kind: EV_BLIND, .. }, _))
    ) {
        ans.confidence = "low";
    }
    Some(ans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::Th;

    fn th(sched: (u64, u64), n: u32, running: u32, waits: &[(Wait, u32)]) -> Th {
        Th {
            sched0: Some((0, 0)),
            sched1: Some(sched),
            n,
            running,
            waits: waits.iter().cloned().collect(),
            ..Th::default()
        }
    }

    fn get(v: &[(Key, f64)], k: &Key) -> f64 {
        v.iter().filter(|x| &x.0 == k).map(|x| x.1).sum()
    }

    #[test]
    fn split_adds_up_to_wall() {
        let t = th((300, 200), 100, 50, &[(Wait::Sleep, 30), (Wait::Child, 20)]);
        let v = split_thread(&t, 1000.0, 100.0);
        let sum: f64 = v.iter().map(|x| x.1).sum();
        assert!((sum - 1000.0).abs() < 1e-6);
        assert_eq!(get(&v, &Key::Cpu), 300.0);
        assert_eq!(get(&v, &Key::Rq), 200.0);
        assert_eq!(get(&v, &Key::W(Wait::Sleep)), 300.0);
        assert_eq!(get(&v, &Key::W(Wait::Child)), 200.0);
    }

    #[test]
    fn counters_larger_than_wall_are_scaled() {
        let t = th((900, 600), 10, 10, &[]);
        let v = split_thread(&t, 1000.0, 100.0);
        assert!((get(&v, &Key::Cpu) - 600.0).abs() < 1e-6);
        assert!((get(&v, &Key::Rq) - 400.0).abs() < 1e-6);
    }

    #[test]
    fn runnable_but_unaccounted_time_counts_as_cpu_wait() {
        // Always seen runnable, yet the scheduler only accounts for 20%.
        let t = th((200, 0), 100, 100, &[]);
        let v = split_thread(&t, 1000.0, 100.0);
        assert_eq!(get(&v, &Key::Cpu), 200.0);
        assert!((get(&v, &Key::Rq) - 800.0).abs() < 1e-6);
    }

    #[test]
    fn unsampled_sleep_is_idle() {
        let t = th((100, 0), 10, 1, &[]);
        let v = split_thread(&t, 1000.0, 100.0);
        assert_eq!(
            get(
                &v,
                &Key::W(Wait::Park {
                    what: "brief sleeps"
                })
            ),
            900.0
        );
        let sum: f64 = v.iter().map(|x| x.1).sum();
        assert!((sum - 1000.0).abs() < 1e-6);
    }
}
