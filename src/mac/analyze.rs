//! Turns what macOS shows about one process into the answer.
//!
//! - libproc, every 10 ms: each thread's state and exact CPU time; the
//!   process's exact time runnable but waiting for a CPU (rusage).
//! - Apple's `sample`, over the same window: every thread's stack, whose
//!   leaf says what a blocked thread is blocked in.
//! - Descriptors (libproc) name the pipe, socket or file, and other
//!   processes' descriptors name who holds the other end.

use super::fds::{self, Fd, Kind};
use super::ffi;
use super::stacks::{self, Leaf, SThread, SampleOut};
use crate::answer::{Answer, Blame, Cat, Opts, CAUSES};
use crate::code::{self, Stacks};
use crate::out::shorten;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum W {
    L(Leaf),
    /// Blocked, and `sample` could not show on what.
    Hidden,
    /// Uninterruptible, and `sample` could not show on what.
    Uninterruptible,
    /// Uninterruptible while in its own code: a page fault.
    Fault,
    Stopped,
    Brief,
}

#[derive(Default, Debug, Clone)]
pub struct Th {
    pub name: String,
    pub n: u32,
    pub running: u32,
    pub waiting: u32,
    pub uninter: u32,
    pub stopped: u32,
    pub cpu0: Option<(u64, u64)>,
    pub cpu1: (u64, u64),
    /// Samples at background priority (QoS background, taskpolicy -b).
    pub lowpri: u32,
}

impl Th {
    pub fn cpu_ns(&self) -> f64 {
        let (u0, s0) = self.cpu0.unwrap_or(self.cpu1);
        (self.cpu1.0.saturating_sub(u0) + self.cpu1.1.saturating_sub(s0)) as f64
    }
    fn sys_ns(&self) -> f64 {
        let (_, s0) = self.cpu0.unwrap_or(self.cpu1);
        self.cpu1.1.saturating_sub(s0) as f64
    }
}

pub struct Watch {
    pub window: Duration,
    pub ticks: u32,
    pub threads: HashMap<u64, Th>,
    pub usage0: Option<ffi::Usage>,
    pub usage1: Option<ffi::Usage>,
    pub gone: bool,
    /// Thread details were readable (own process, or root).
    pub readable: bool,
}

pub fn watch(pid: i32, dur: Duration) -> Option<Watch> {
    ffi::short(pid)?;
    let mut w = Watch {
        window: Duration::ZERO,
        ticks: 0,
        threads: HashMap::new(),
        usage0: ffi::usage(pid),
        usage1: None,
        gone: false,
        readable: false,
    };
    let mut ids = ffi::thread_ids(pid);
    let tick = Duration::from_millis(match ids.len() {
        0..=16 => 10,
        17..=128 => 20,
        _ => 50,
    });
    let start = Instant::now();
    let deadline = start + dur;
    let mut i: u32 = 0;
    loop {
        if i > 0 && i.is_multiple_of(8) {
            ids = ffi::thread_ids(pid);
        }
        let mut alive = false;
        for &id in ids.iter().take(512) {
            let Some(t) = ffi::thread(pid, id) else { continue };
            alive = true;
            w.readable = true;
            sample_thread(w.threads.entry(id).or_default(), &t);
        }
        i += 1;
        if !alive && ffi::short(pid).map(|s| s.status == 5).unwrap_or(true) {
            w.gone = true;
            break;
        }
        if !alive {
            // Not ours to read: keep the window, there is nothing per thread.
            if ffi::short(pid).is_none() {
                w.gone = true;
                break;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let next = (start + tick * i).min(deadline);
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    w.usage1 = ffi::usage(pid);
    w.ticks = i;
    w.window = start.elapsed();
    Some(w)
}

pub fn sample_thread(th: &mut Th, t: &ffi::Thread) {
    if th.cpu0.is_none() {
        th.cpu0 = Some((t.user_ns, t.system_ns));
        th.name = t.name.clone();
    }
    th.cpu1 = (t.user_ns, t.system_ns);
    th.n += 1;
    match t.state {
        1 => th.running += 1,
        2 | 5 => th.stopped += 1,
        4 => th.uninter += 1,
        _ => th.waiting += 1,
    }
    if t.curpri > 0 && t.curpri <= 4 {
        th.lowpri += 1;
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum Key {
    Cpu,
    Rq,
    W(W),
}

fn disky(l: &Leaf) -> bool {
    matches!(
        l,
        Leaf::Fsync | Leaf::Fcntl | Leaf::FileOp(_) | Leaf::Open | Leaf::Read { .. } | Leaf::Write { .. }
    )
}

/// Split one thread's share of the window into CPU, runqueue and waits.
pub fn split(th: &Th, wall: f64, rq: f64, st: Option<&SThread>) -> Vec<(Key, f64)> {
    let mut cpu = th.cpu_ns().min(wall);
    let mut rq = rq;
    if cpu + rq > wall && cpu + rq > 0.0 {
        let k = wall / (cpu + rq);
        cpu *= k;
        rq *= k;
    }
    let mut rest = (wall - cpu - rq).max(0.0);
    let mut out = vec![(Key::Cpu, cpu), (Key::Rq, rq)];
    let mut counts: HashMap<W, f64> = HashMap::new();
    match st.filter(|s| s.total > 0) {
        Some(s) => {
            for (frames, c) in &s.stacks {
                let l = stacks::classify(frames);
                if l != Leaf::Cpu {
                    *counts.entry(W::L(l)).or_default() += *c as f64;
                }
            }
            // Uninterruptible while `sample` shows its own code: faulting.
            if th.uninter > 0 && !counts.keys().any(|w| matches!(w, W::L(l) if disky(l))) {
                let f = (wall * th.uninter as f64 / th.n.max(1) as f64).min(rest);
                out.push((Key::W(W::Fault), f));
                rest -= f;
            }
        }
        None => {
            if th.uninter > 0 {
                counts.insert(W::Uninterruptible, th.uninter as f64);
            }
            if th.waiting > 0 {
                counts.insert(W::Hidden, th.waiting as f64);
            }
        }
    }
    if th.stopped > 0 {
        counts.insert(W::Stopped, th.stopped as f64 * 1000.0);
    }
    let total: f64 = counts.values().sum();
    if total > 0.0 {
        for (w, c) in counts {
            out.push((Key::W(w), rest * c / total));
        }
    } else if rest > 0.0 {
        out.push((Key::W(W::Brief), rest));
    }
    out
}

/// Each thread's share of the process's measured runnable time.
pub fn runqueue(threads: &HashMap<u64, Th>, walls: &HashMap<u64, f64>, total: Option<f64>) -> HashMap<u64, f64> {
    let est: HashMap<u64, f64> = threads
        .iter()
        .map(|(id, t)| {
            let w = walls.get(id).copied().unwrap_or(0.0);
            (*id, (w * t.running as f64 / t.n.max(1) as f64 - t.cpu_ns()).max(0.0))
        })
        .collect();
    let Some(total) = total else {
        return est;
    };
    let sum: f64 = est.values().sum();
    let cpu: f64 = threads.values().map(|t| t.cpu_ns()).sum();
    threads
        .iter()
        .map(|(id, t)| {
            let v = if sum > 0.0 {
                total * est[id] / sum
            } else if cpu > 0.0 {
                total * t.cpu_ns() / cpu
            } else {
                0.0
            };
            (*id, v)
        })
        .collect()
}

pub fn comm(pid: i32) -> String {
    ffi::short(pid).map(|s| s.comm).unwrap_or_else(|| "?".into())
}

/// "name" plus a shortened command line.
pub fn label(pid: i32) -> String {
    let c = comm(pid);
    let Some(cl) = ffi::cmdline(pid) else { return c };
    if cl.is_empty() || cl == c {
        return c;
    }
    let words: Vec<String> = cl
        .split(' ')
        .enumerate()
        .map(|(i, w)| {
            let base = w.rsplit('/').next().unwrap_or(w);
            if i == 0 {
                base.to_string()
            } else if w.starts_with('/') && w.matches('/').count() > 2 && !base.is_empty() {
                format!("…/{base}")
            } else {
                w.to_string()
            }
        })
        .collect();
    shorten(&words.join(" "), 60)
}

pub fn children(pid: i32) -> Vec<i32> {
    ffi::pids()
        .into_iter()
        .filter(|&p| ffi::short(p).map(|s| s.ppid == pid && s.status != 5).unwrap_or(false))
        .collect()
}

pub fn newest_child(pid: i32) -> Option<(i32, usize)> {
    let mut kids: Vec<((u64, u64), i32)> = children(pid)
        .into_iter()
        .map(|c| (ffi::start_time(c).unwrap_or((0, c as u64)), c))
        .collect();
    kids.sort_unstable();
    kids.last().map(|k| (k.1, kids.len() - 1))
}

fn ancestors(pid: i32) -> HashSet<i32> {
    let mut s = HashSet::new();
    let mut p = pid;
    for _ in 0..64 {
        let Some(sh) = ffi::short(p) else { break };
        if sh.ppid <= 1 || !s.insert(sh.ppid) {
            break;
        }
        p = sh.ppid;
    }
    s
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

/// Is the process only waiting for a child? A quick look with `sample`.
fn waiting_on_child(pid: i32) -> bool {
    if children(pid).is_empty() {
        return false;
    }
    let t0 = ffi::task(pid);
    let Some(run) = stacks::start(pid, Duration::from_millis(150), 10) else {
        return false;
    };
    let Ok(s) = run.finish() else { return false };
    let busy = match (t0, ffi::task(pid)) {
        (Some(a), Some(b)) => (b.user_ns + b.system_ns).saturating_sub(a.user_ns + a.system_ns) > 30_000_000,
        _ => false,
    };
    if busy {
        return false;
    }
    let mut child = 0;
    let mut other = 0;
    for t in &s.threads {
        for (f, c) in &t.stacks {
            match stacks::classify(f) {
                Leaf::Child => child += c,
                Leaf::Workq | Leaf::RunLoop | Leaf::Signal | Leaf::Park(_) => {}
                _ => other += c,
            }
        }
    }
    child > 0 && child >= other
}

fn doing(pid: i32) -> Option<String> {
    if waiting_on_child(pid) {
        if let Some((c, _)) = newest_child(pid) {
            return Some(format!("only waiting for its child pid {c} ({})", comm(c)));
        }
    }
    let (a, _) = analyze_full(
        pid,
        Opts {
            dur: Duration::from_millis(300),
            follow: false,
            light: true,
            stacks: false,
        },
    )?;
    Some(match a.cause {
        Some(Cat::Cpu) => "busy on CPU".into(),
        Some(Cat::Peer) => "itself waiting for someone else".into(),
        Some(Cat::Memory) => "itself short of memory".into(),
        Some(c) => format!("itself {}", c.headline()),
        None if a.state == "stopped" => "stopped by a signal".into(),
        None if a.state == "unknown" => "blocked on something wits cannot see".into(),
        None => a.idle_what.clone(),
    })
}

fn process_blame(pid: i32, more: usize, lead: &str, light: bool) -> Blame {
    let name = label(pid);
    let d = if light {
        String::new()
    } else {
        doing(pid).map(|d| format!(", which is {d}")).unwrap_or_default()
    };
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
        ..Blame::default()
    }
}

/// What other visible processes did during the window.
struct Others {
    before: HashMap<i32, ffi::Usage>,
    after: HashMap<i32, ffi::Usage>,
}

impl Others {
    fn take(exclude: i32) -> HashMap<i32, ffi::Usage> {
        fds::visible_pids(exclude)
            .into_iter()
            .filter_map(|p| ffi::usage(p).map(|u| (p, u)))
            .collect()
    }
    fn delta(&self, f: impl Fn(&ffi::Usage) -> u64) -> Vec<(i32, u64)> {
        let mut v: Vec<(i32, u64)> = self
            .after
            .iter()
            .filter_map(|(p, b)| self.before.get(p).map(|a| (*p, f(b).saturating_sub(f(a)))))
            .filter(|x| x.1 > 0)
            .collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        v
    }
}

#[derive(Clone)]
struct Res {
    cat: Cat,
    label: String,
    fd: Option<i32>,
}

struct Ctx<'a> {
    pid: i32,
    fds: &'a [Fd],
    fds_readable: bool,
    swap_in: bool,
    pressure: i32,
    wrote: bool,
    /// A blocked read or write had several candidate descriptors.
    unsure: std::cell::Cell<bool>,
    /// ... or too many to pick one: how many.
    many: std::cell::Cell<usize>,
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{b:.0} B")
    }
}

impl Ctx<'_> {
    fn fd(&self, fd: i32) -> Option<&Fd> {
        self.fds.iter().find(|f| f.fd == fd)
    }

    /// `sample` does not show the descriptor: guess it from what the
    /// process holds, as the Linux limited view does.
    fn guess(&self, write: bool, sock_only: bool) -> Option<i32> {
        let ok = |f: &&Fd| (if write { fds::writable(f) } else { fds::readable(f) }) || matches!(f.kind, Kind::Socket(_));
        let conn = |f: &&Fd| match &f.kind {
            Kind::Socket(s) => fds::connected(s),
            Kind::Pipe(_) | Kind::Fifo(_) | Kind::Tty(_) => !sock_only,
            _ => false,
        };
        let c: Vec<&Fd> = self.fds.iter().filter(ok).filter(conn).collect();
        let std = if write { 1 } else { 0 };
        let non_tty: Vec<&&Fd> = c.iter().filter(|f| !matches!(f.kind, Kind::Tty(_))).collect();
        if !sock_only {
            if let Some(f) = non_tty.iter().find(|f| f.fd == std) {
                self.unsure.set(non_tty.len() > 1);
                return Some(f.fd);
            }
        }
        match non_tty.len() {
            1 => return Some(non_tty[0].fd),
            0 => {}
            n if n > 3 => {
                self.many.set(n);
                return None;
            }
            _ => {
                self.unsure.set(true);
                // The one with nothing to read (or something unsent).
                let quiet = non_tty.iter().find(|f| match &f.kind {
                    Kind::Socket(s) => if write { s.snd > 0 } else { s.rcv == 0 },
                    Kind::Pipe(p) => if write { p.queued > 0 } else { p.queued == 0 },
                    _ => false,
                });
                return quiet.or(non_tty.first()).map(|f| f.fd);
            }
        }
        if !sock_only {
            if let Some(f) = c.iter().find(|f| f.fd == std) {
                return Some(f.fd);
            }
        }
        // A regular file: the I/O is disk I/O.
        let files: Vec<&Fd> = self
            .fds
            .iter()
            .filter(|f| matches!(f.kind, Kind::File(_)) && (if write { fds::writable(f) } else { fds::readable(f) }))
            .collect();
        if files.len() == 1 {
            return Some(files[0].fd);
        }
        // Standard output redirected to a file is rarely where it blocks.
        let own: Vec<&&Fd> = files.iter().filter(|f| f.fd > 2).collect();
        (own.len() == 1).then(|| own[0].fd)
    }

    fn written_file(&self) -> Option<&Fd> {
        let v: Vec<&Fd> = self
            .fds
            .iter()
            .filter(|f| matches!(f.kind, Kind::File(_)) && fds::writable(f))
            .collect();
        v.last().copied()
    }

    fn resolve(&self, w: &W) -> Res {
        let r = |cat, label: String, fd| Res { cat, label, fd };
        match w {
            W::L(l) => match l {
                Leaf::Cpu => r(Cat::Cpu, String::new(), None),
                Leaf::Lock(what) => r(Cat::Lock, format!("{what} inside the process"), None),
                Leaf::FileLock => {
                    let f = self.guess_file();
                    r(
                        Cat::Lock,
                        match f.and_then(|fd| self.fd(fd)) {
                            Some(f) => format!("the file lock on {}", fds::desc(f)),
                            None => "a file lock".into(),
                        },
                        f,
                    )
                }
                Leaf::Fsync => match self.written_file() {
                    Some(f) => r(Cat::Disk, format!("fsync() on {}", fds::desc(f)), Some(f.fd)),
                    None => r(Cat::Disk, "fsync()".into(), None),
                },
                Leaf::Fcntl => {
                    if self.wrote {
                        match self.written_file() {
                            Some(f) => r(Cat::Disk, format!("fcntl(F_FULLFSYNC) on {}", fds::desc(f)), Some(f.fd)),
                            None => r(Cat::Disk, "fcntl(F_FULLFSYNC)".into(), None),
                        }
                    } else {
                        let f = self.guess_file();
                        r(
                            Cat::Lock,
                            match f.and_then(|fd| self.fd(fd)) {
                                Some(f) => format!("the file lock on {} (fcntl)", fds::desc(f)),
                                None => "a file lock (fcntl)".into(),
                            },
                            f,
                        )
                    }
                }
                Leaf::Read { sys, sock } => self.io(sys, false, *sock),
                Leaf::Write { sys, sock } => self.io(sys, true, *sock),
                Leaf::Connect => {
                    let fd = self.fds.iter().find(|f| matches!(&f.kind, Kind::Socket(s) if s.kind == 2 && s.tcp_state == 2));
                    match fd {
                        Some(f) => r(Cat::Peer, format!("connecting: {}", fds::desc(f)), Some(f.fd)),
                        None => r(Cat::Peer, "connecting to a server".into(), None),
                    }
                }
                Leaf::Accept => {
                    let l = self
                        .fds
                        .iter()
                        .find(|f| matches!(&f.kind, Kind::Socket(s) if s.listening()))
                        .map(|f| format!(" ({})", fds::desc(f)))
                        .unwrap_or_default();
                    r(Cat::Idle, format!("waiting for new connections{l}"), None)
                }
                Leaf::Open => r(
                    Cat::Peer,
                    "opening a FIFO: nobody has opened the other end".into(),
                    None,
                ),
                Leaf::Child => r(Cat::Peer, "waiting for its child processes to finish".into(), None),
                Leaf::Sleep => r(Cat::Idle, "asleep on its own timer".into(), None),
                // An event loop (Node, Python's asyncio, a run loop) while one
                // of its connections is still being opened: it waits on that.
                Leaf::Events(_) | Leaf::RunLoop if self.connecting().is_some() && !self.listens() => {
                    let f = self.connecting().unwrap();
                    r(
                        Cat::Peer,
                        format!("connecting: {}, no answer yet (waiting in its event loop)", fds::desc(f)),
                        Some(f.fd),
                    )
                }
                Leaf::Events(sys) => {
                    // One connection and no listener: it is waiting on that peer.
                    let conns: Vec<&Fd> = self
                        .fds
                        .iter()
                        .filter(|f| matches!(&f.kind, Kind::Socket(s) if fds::connected(s) && s.kind != 1))
                        .collect();
                    let listens = self.fds.iter().any(|f| matches!(&f.kind, Kind::Socket(s) if s.listening()));
                    if conns.len() == 1 && !listens {
                        let f = conns[0];
                        r(
                            Cat::Peer,
                            format!("waiting in {sys}() on fd {}, {}", f.fd, fds::desc(f)),
                            Some(f.fd),
                        )
                    } else {
                        r(Cat::Idle, format!("waiting for events in {sys}()"), None)
                    }
                }
                Leaf::RunLoop => r(Cat::Idle, "waiting in its run loop for events".into(), None),
                Leaf::Reply => r(
                    Cat::Peer,
                    "waiting for the reply to a Mach/XPC message from another process".into(),
                    None,
                ),
                Leaf::Workq => r(Cat::Idle, "idle worker threads".into(), None),
                Leaf::Park(what) => r(Cat::Idle, format!("waiting on {what}"), None),
                Leaf::Signal => r(Cat::Idle, "waiting for a signal".into(), None),
                Leaf::FileOp(sys) => r(Cat::Disk, format!("{sys}() on the file system"), None),
                // `sample` now and then fails to name a system call stub.
                Leaf::Other(f) if f == "[unknown]" => {
                    r(Cat::Idle, "blocked, but wits cannot see on what".into(), None)
                }
                Leaf::Other(f) => r(Cat::Idle, format!("waiting in {f}()"), None),
            },
            W::Hidden => r(Cat::Idle, "blocked, but wits cannot see on what".into(), None),
            W::Uninterruptible => r(Cat::Disk, "uninterruptible waits (most likely disk I/O)".into(), None),
            W::Fault => {
                if self.swap_in || self.pressure >= 2 {
                    r(
                        Cat::Memory,
                        if self.swap_in {
                            "pages being read back in from swap".into()
                        } else {
                            "page faults while memory is short".into()
                        },
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
            W::Stopped => r(
                Cat::Idle,
                "stopped by a signal (Ctrl-Z, SIGSTOP or a debugger)".into(),
                None,
            ),
            W::Brief => r(Cat::Idle, "waiting for brief sleeps".into(), None),
        }
    }

    /// A TCP socket whose connect has not been answered yet (SYN_SENT).
    fn connecting(&self) -> Option<&Fd> {
        self.fds
            .iter()
            .find(|f| matches!(&f.kind, Kind::Socket(s) if s.kind == 2 && s.tcp_state == 2))
    }

    fn listens(&self) -> bool {
        self.fds.iter().any(|f| matches!(&f.kind, Kind::Socket(s) if s.listening()))
    }

    fn guess_file(&self) -> Option<i32> {
        let v: Vec<&Fd> = self.fds.iter().filter(|f| matches!(f.kind, Kind::File(_))).collect();
        (v.len() == 1).then(|| v[0].fd)
    }

    fn io(&self, sys: &str, write: bool, sock: bool) -> Res {
        let r = |cat, label: String, fd| Res { cat, label, fd };
        let Some(fd) = self.guess(write, sock) else {
            let n = self.many.get();
            if n > 0 {
                let dir = if write { "to" } else { "on" };
                return r(
                    Cat::Peer,
                    format!("blocked in {sys}() {dir} one of its {n} pipes and sockets (macOS does not say which)"),
                    None,
                );
            }
            let why = if self.fds_readable { "wits cannot tell apart" } else { "wits cannot see" };
            return r(Cat::Idle, format!("blocked in {sys}() on a descriptor {why}"), None);
        };
        let Some(f) = self.fd(fd) else {
            return r(Cat::Idle, format!("blocked in {sys}() on fd {fd}"), None);
        };
        let desc = fds::desc(f);
        match (&f.kind, write) {
            (Kind::Tty(_), false) => r(Cat::Idle, "waiting for terminal input".into(), None),
            (Kind::Socket(s), _) if s.listening() => r(Cat::Idle, format!("waiting for new connections ({desc})"), None),
            (Kind::Socket(s), false) if s.kind == 1 && !fds::connected(s) => {
                r(Cat::Idle, format!("waiting for datagrams on {desc}"), None)
            }
            (Kind::Pipe(_) | Kind::Fifo(_) | Kind::Socket(_), false) => {
                r(Cat::Peer, format!("blocked in {sys}() on fd {fd}, {desc}"), Some(fd))
            }
            (Kind::Pipe(_) | Kind::Fifo(_) | Kind::Socket(_) | Kind::Tty(_), true) => r(
                Cat::Peer,
                format!("blocked in {sys}() to fd {fd}, {desc}: the other end is not reading"),
                Some(fd),
            ),
            (Kind::File(_), _) => r(Cat::Disk, format!("{sys}() on {desc}"), Some(fd)),
            _ => r(Cat::Idle, format!("waiting on device {desc}"), None),
        }
    }
}

fn peer_blame(cx: &Ctx, res: &Res, w: &W, light: bool) -> Blame {
    let pid = cx.pid;
    if matches!(w, W::L(Leaf::Child)) {
        return match newest_child(pid) {
            Some((c, more)) => process_blame(c, more, "its child ", light),
            None => Blame::new("unknown", "a child process that has already gone".into()),
        };
    }
    if matches!(w, W::L(Leaf::Reply)) {
        return Blame::new(
            "unknown",
            "another process over Mach/XPC; macOS does not say which (sudo spindump names it)".into(),
        );
    }
    let unseen = || {
        Blame::new(
            "unknown",
            "the other end is held by a process wits cannot see (another user's? sudo wits sees it)".into(),
        )
    };
    let Some(f) = res.fd.and_then(|fd| cx.fd(fd)) else {
        return Blame::new("unknown", "wits cannot tell which process is at the other end".into());
    };
    let writing = matches!(w, W::L(Leaf::Write { .. }));
    match &f.kind {
        Kind::Pipe(p) => match pick(fds::pipe_holders(p.peer, pid, Some(!writing)), pid) {
            Some((o, more)) => process_blame(o, more, "", light),
            None => unseen(),
        },
        Kind::Fifo(v) => match pick(fds::file_holders(v.dev, v.ino, pid), pid) {
            Some((o, more)) => process_blame(o, more, "", light),
            None => unseen(),
        },
        Kind::Tty(_) => Blame::new(
            "unknown",
            "the terminal (or whoever reads it) is not consuming output".into(),
        ),
        Kind::Socket(s) if s.kind == 2 => {
            if !fds::is_local(&s.remote) {
                let svc = match &s.remote {
                    ffi::SockAddr::In { port, .. } => fds::port_name(*port).map(|n| format!(" ({n})")).unwrap_or_default(),
                    _ => String::new(),
                };
                return Blame::new(
                    "remote",
                    format!("remote host {}{svc}: not a process on this machine", s.remote),
                );
            }
            match pick(fds::socket_peer(s, pid), pid) {
                Some((o, more)) => process_blame(o, more, "", light),
                None => Blame::new(
                    "unknown",
                    match &s.remote {
                        ffi::SockAddr::In { port, .. } => format!(
                            "a local process on port {port} that wits cannot see (another user's?)"
                        ),
                        _ => "a local process wits cannot see".into(),
                    },
                ),
            }
        }
        Kind::Socket(s) if s.kind == 3 => match pick(fds::socket_peer(s, pid), pid) {
            Some((o, more)) => process_blame(o, more, "", light),
            None => unseen(),
        },
        Kind::Socket(s) => Blame::new("remote", format!("whoever answers at {}", s.remote)),
        _ => unseen(),
    }
}

fn activity(keys: &[(Key, f64)], cx: &Ctx) -> String {
    let mut by: HashMap<String, f64> = HashMap::new();
    for (k, ns) in keys {
        let l = match k {
            Key::Cpu => "busy on CPU".to_string(),
            Key::Rq => "waiting for a CPU".to_string(),
            Key::W(w) => cx.resolve(w).label,
        };
        *by.entry(l).or_default() += ns;
    }
    by.into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|x| x.0)
        .unwrap_or_else(|| "idle".into())
}

pub struct Sys {
    pub swapins: u64,
    pub pressure: i32,
    /// Fraction of all CPUs busy during the window.
    pub busy: Option<f64>,
}

fn pct(x: f64) -> String {
    format!("{:.0}%", (x * 100.0).round())
}

pub fn analyze_full(pid: i32, opts: Opts) -> Option<(Answer, Option<Stacks>)> {
    let mut pid = pid;
    let mut followed = Vec::new();
    if opts.follow {
        for _ in 0..8 {
            if !waiting_on_child(pid) {
                break;
            }
            let Some((child, _)) = newest_child(pid) else { break };
            followed.push((pid, comm(pid)));
            pid = child;
        }
    }
    let short = ffi::short(pid)?;
    if ffi::thread_ids(pid).is_empty() && ffi::usage(pid).is_none() {
        return limited(pid, &short, followed, opts.dur).map(|a| (a, None));
    }
    let me = unsafe { libc::getuid() };
    let others0 = (!opts.light).then(|| Others::take(pid));
    let vm0 = ffi::vm_swapins_pageins();
    let cpu0 = ffi::cpu_ticks();
    let fdl = fds::list(pid);
    let fds_readable = fdl.is_some();
    let fdl = fdl.unwrap_or_default();
    let run = stacks::start(pid, opts.dur, 10);
    let w = watch(pid, opts.dur)?;
    let sres: Result<SampleOut, String> = match run {
        // Within the overview several run at once: keep to its budget.
        Some(r) => r.finish_within(if opts.light || !opts.stacks {
            Duration::from_millis(2500)
        } else {
            Duration::from_secs(30)
        }),
        None => Err("could not run /usr/bin/sample".into()),
    };
    let vm1 = ffi::vm_swapins_pageins();
    let cpu1 = ffi::cpu_ticks();
    let others = others0.map(|b| Others {
        before: b,
        after: Others::take(pid),
    });
    let sys = Sys {
        swapins: match (vm0, vm1) {
            (Some(a), Some(b)) => b.0.saturating_sub(a.0),
            _ => 0,
        },
        pressure: ffi::memory_pressure().unwrap_or(1),
        busy: match (cpu0, cpu1) {
            (Some(a), Some(b)) => {
                let busy = b.0.saturating_sub(a.0) as f64;
                let idle = b.1.saturating_sub(a.1) as f64;
                (busy + idle > 0.0).then(|| busy / (busy + idle))
            }
            _ => None,
        },
    };

    let samp = sres.as_ref().ok();
    let mut notes = Vec::new();
    let own = short.uid == me || me == 0;
    if w.gone {
        notes.push("the process exited while wits was watching".to_string());
    }
    // How many samples did `sample` get, against how many it should have?
    let expect = (w.window.as_secs_f64() * 1000.0 / 10.0).max(1.0);
    let got = samp.map(|s| s.threads.iter().map(|t| t.total).max().unwrap_or(0)).unwrap_or(0);
    let snapshot_only = samp.is_some() && (got as f64) < expect / 8.0 && !w.gone;
    match &sres {
        Err(e) if !own => notes.push(format!(
            "limited view: macOS shows another user's processes only to root ({e}); sudo wits sees them"
        )),
        Err(e) => notes.push(format!("no stacks: {e}")),
        Ok(_) if snapshot_only => notes.push(
            "macOS lets `sample` take only a snapshot of this protected system binary; what it waits on rests on that".into(),
        ),
        _ => {}
    }
    let blind = samp.is_none();

    let window_ns = w.window.as_nanos() as f64;
    let ticks = w.ticks.max(1) as f64;
    let walls: HashMap<u64, f64> = w
        .threads
        .iter()
        .map(|(id, t)| (*id, window_ns * t.n as f64 / ticks))
        .collect();
    let rq_total = match (&w.usage0, &w.usage1) {
        (Some(a), Some(b)) => Some(b.runnable_ns.saturating_sub(a.runnable_ns) as f64),
        _ => None,
    };
    let rqs = runqueue(&w.threads, &walls, rq_total);
    let wrote = match (&w.usage0, &w.usage1) {
        (Some(a), Some(b)) => b.disk_written > a.disk_written,
        _ => false,
    };
    let cx = Ctx {
        pid,
        fds: &fdl,
        fds_readable,
        swap_in: sys.swapins > 0,
        pressure: sys.pressure,
        wrote,
        unsure: std::cell::Cell::new(false),
        many: std::cell::Cell::new(0),
    };

    // Threads not readable through libproc (another user's process):
    // nothing to weigh, the answer is what `sample` alone shows.
    let mut threads = w.threads.clone();
    // Unnamed threads: the dispatch queue `sample` saw them on.
    if let Some(s) = samp {
        for (id, t) in threads.iter_mut() {
            if t.name.is_empty() {
                if let Some(st) = s.thread(*id) {
                    t.name = if st.name == "com.apple.main-thread" {
                        "main thread".into()
                    } else {
                        st.name.clone()
                    };
                }
            }
        }
    }
    if threads.is_empty() {
        if let Some(s) = samp {
            for t in &s.threads {
                threads.insert(
                    t.id,
                    Th {
                        name: t.name.clone(),
                        n: w.ticks,
                        waiting: w.ticks,
                        ..Th::default()
                    },
                );
            }
        }
    }
    let walls: HashMap<u64, f64> = threads
        .iter()
        .map(|(id, t)| (*id, window_ns * t.n as f64 / ticks))
        .collect();

    let mut per_thread: HashMap<u64, Vec<(Key, f64)>> = HashMap::new();
    let mut resolved: HashMap<W, Res> = HashMap::new();
    let mut cat_ns: HashMap<Cat, f64> = HashMap::new();
    let mut wait_ns: HashMap<W, f64> = HashMap::new();
    let mut tid_w: HashMap<(u64, W), f64> = HashMap::new();
    let mut total = 0.0;
    let mut hottest: Option<(u64, f64)> = None;
    let (mut cpu_all, mut sys_all) = (0.0, 0.0);
    let mut lowpri = (0u32, 0u32);
    for (id, th) in &threads {
        let wall = walls[id];
        total += wall;
        cpu_all += th.cpu_ns();
        sys_all += th.sys_ns();
        lowpri.0 += th.lowpri;
        lowpri.1 += th.n;
        let parts = split(th, wall, rqs.get(id).copied().unwrap_or(0.0), samp.and_then(|s| s.thread(*id)));
        for (k, ns) in &parts {
            let cat = match k {
                Key::Cpu => {
                    if hottest.map(|h| *ns > h.1).unwrap_or(true) {
                        hottest = Some((*id, *ns));
                    }
                    Cat::Cpu
                }
                Key::Rq => Cat::CpuWait,
                Key::W(wt) => {
                    let r = resolved.entry(wt.clone()).or_insert_with(|| cx.resolve(wt));
                    *wait_ns.entry(wt.clone()).or_default() += ns;
                    *tid_w.entry((*id, wt.clone())).or_default() += ns;
                    r.cat
                }
            };
            *cat_ns.entry(cat).or_default() += ns;
        }
        per_thread.insert(*id, parts);
    }

    // Stacks from `sample`, weighted by the time each thread spent there.
    let code_stacks = samp.map(|s| {
        let mut counts: HashMap<(u64, W), f64> = HashMap::new();
        let mut cpu_counts: HashMap<u64, f64> = HashMap::new();
        for t in &s.threads {
            for (f, c) in &t.stacks {
                match stacks::classify(f) {
                    Leaf::Cpu => *cpu_counts.entry(t.id).or_default() += *c as f64,
                    l => *counts.entry((t.id, W::L(l))).or_default() += *c as f64,
                }
            }
        }
        let mut st = stacks::to_stacks(s, pid, &short.comm, |tid, leaf, c| {
            let c = c as f64;
            match leaf {
                Leaf::Cpu => {
                    let cpu = threads.get(&tid).map(|t| t.cpu_ns()).unwrap_or(0.0);
                    let n = cpu_counts.get(&tid).copied().unwrap_or(0.0);
                    // A thread sampled in its own code but measured off CPU
                    // was faulting or queued: leave it to the measured split.
                    (n > 0.0 && cpu > 0.0).then(|| (Cat::Cpu, cpu * c / n))
                }
                l => {
                    let key = (tid, W::L(l.clone()));
                    let ns = tid_w.get(&key).copied().unwrap_or(0.0);
                    let n = counts.get(&key).copied().unwrap_or(0.0);
                    let cat = resolved.get(&W::L(l.clone())).map(|r| r.cat).unwrap_or(Cat::Idle);
                    (n > 0.0 && ns > 0.0).then(|| (cat, ns * c / n))
                }
            }
        });
        if !w.readable {
            // No libproc times: weigh by samples alone.
            st = stacks::to_stacks(s, pid, &short.comm, |_, leaf, c| {
                let cat = match leaf {
                    Leaf::Cpu => Cat::Cpu,
                    l => resolved.get(&W::L(l.clone())).map(|r| r.cat).unwrap_or(Cat::Idle),
                };
                Some((cat, c as f64 * s.interval_ms.max(1.0) * 1e6))
            });
        }
        let exe = ffi::path(pid).unwrap_or_default();
        if let Some(rt) = crate::lang::detect(&exe, s.images.iter().map(|p| p.as_str())) {
            st.notes.push(super::stacks::runtime_note(rt));
        }
        st
    });

    // `sample` runs regardless (it says what each thread is blocked in);
    // --no-stacks only leaves out the code view.
    let code_stacks = code_stacks.filter(|_| opts.stacks);
    let idle_ns = cat_ns.get(&Cat::Idle).copied().unwrap_or(0.0);
    let active_ns = (total - idle_ns).max(0.0);
    let top_wait = |cat: Cat| -> Option<(W, Res)> {
        let mut by_label: HashMap<&str, (f64, &W, f64)> = HashMap::new();
        for (wt, ns) in &wait_ns {
            let r = &resolved[wt];
            if r.cat == cat {
                let e = by_label.entry(r.label.as_str()).or_insert((0.0, wt, 0.0));
                e.0 += ns;
                if *ns > e.2 {
                    e.1 = wt;
                    e.2 = *ns;
                }
            }
        }
        by_label
            .into_values()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, wt, _)| (wt.clone(), resolved[wt].clone()))
    };
    // What the main thread waits on says what the program is doing; a pool of
    // parked worker threads (Node's, a JVM's) would otherwise outweigh it.
    let main_idle = threads
        .iter()
        .find(|(_, t)| t.name == "main thread")
        .and_then(|(id, _)| per_thread.get(id))
        .and_then(|parts| {
            parts
                .iter()
                .filter_map(|(k, ns)| match k {
                    Key::W(wt) if resolved[wt].cat == Cat::Idle => Some((wt, *ns)),
                    _ => None,
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
        })
        .map(|(wt, _)| resolved[wt].label.clone());
    let idle_what = main_idle
        .or_else(|| top_wait(Cat::Idle).map(|x| x.1.label))
        .unwrap_or_else(|| "idle".into());
    let mut breakdown: Vec<(Cat, f64)> = CAUSES
        .iter()
        .map(|c| (*c, cat_ns.get(c).copied().unwrap_or(0.0) / active_ns.max(1.0)))
        .collect();
    breakdown.sort_by(|a, b| b.1.total_cmp(&a.1));
    let stopped_ns = wait_ns.get(&W::Stopped).copied().unwrap_or(0.0);
    let samples = samp.map(|s| s.threads.iter().map(|t| t.total).sum()).unwrap_or(w.ticks);

    let mut ans = Answer {
        pid,
        name: short.comm.clone(),
        cmdline: ffi::cmdline(pid).unwrap_or_default(),
        followed,
        window_ms: w.window.as_millis() as u64,
        threads: threads.len(),
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
        code: code_stacks
            .as_ref()
            .map(|s| code::summarize(s, 3)),
        pod: None,
    };
    if stopped_ns > 0.5 * total || active_ns < 0.05 * window_ns {
        ans.breakdown.iter_mut().for_each(|b| b.1 = 0.0);
    }
    if stopped_ns > 0.5 * total {
        ans.state = "stopped";
        ans.idle_what = resolved[&W::Stopped].label.clone();
        return Some((ans, code_stacks));
    }
    if active_ns < 0.05 * window_ns {
        let hidden = matches!(
            top_wait(Cat::Idle),
            Some((W::Hidden, _)) | Some((W::L(Leaf::Other(_)), _))
        ) && ans.idle_what == "blocked, but wits cannot see on what";
        if blind || hidden {
            ans.confidence = "low";
            if hidden {
                ans.state = "unknown";
                ans.idle_what = "blocked in the kernel".into();
            }
        }
        return Some((ans, code_stacks));
    }
    let (cause, share) = ans.breakdown[0];
    ans.cause = Some(cause);
    ans.share = share;
    ans.state = cause.state();
    let (what, blame) = match cause {
        Cat::Cpu => {
            let cpus = cat_ns[&Cat::Cpu] / window_ns;
            let kern = if cpu_all > 0.0 { sys_all / cpu_all } else { 0.0 };
            let mut what = if kern >= 0.5 {
                format!("{} of that CPU time is inside the kernel (system calls), the rest its own code", pct(kern))
            } else {
                format!("running its own code ({} user, {} kernel)", pct(1.0 - kern), pct(kern))
            };
            if threads.len() > 1 {
                if let Some((id, ns)) = hottest {
                    let n = threads.get(&id).map(|t| t.name.as_str()).unwrap_or("");
                    let n = if n.is_empty() { String::new() } else { format!(" ({n})") };
                    what.push_str(&format!("; hottest thread {id}{n} at {}", pct(ns / window_ns)));
                }
            }
            (
                what,
                Blame::new("self", format!("itself: nothing is holding it back; it is using {cpus:.2} CPUs")),
            )
        }
        Cat::CpuWait => {
            let bg = lowpri.1 > 0 && lowpri.0 * 2 > lowpri.1;
            let what = if bg {
                "runnable but queued: it runs at background priority, which macOS keeps on the efficiency cores".to_string()
            } else {
                match sys.busy {
                    Some(b) => format!("runnable but queued: all {} CPUs were {} busy", ffi::ncpu(), pct(b)),
                    None => "runnable but queued for a CPU".to_string(),
                }
            };
            let hogs: Vec<(i32, u64)> = others
                .as_ref()
                .map(|o| o.delta(|u| u.user_ns + u.system_ns))
                .unwrap_or_default();
            let win = window_ns.max(1.0);
            let blame = if bg {
                Blame::new("self", "its own priority: it was started as background work (QoS background or taskpolicy -b)".into())
            } else if let Some(&(p, ns)) = hogs.first().filter(|h| h.1 as f64 / win >= 0.1) {
                let mut b = process_blame(p, 0, "", true);
                b.text = format!("{} used {:.1} CPUs in the same window", b.text, ns as f64 / win);
                if let Some(&(p2, ns2)) = hogs.get(1).filter(|h| h.1 as f64 / win >= 0.1) {
                    b.text.push_str(&format!("; next pid {p2} ({}) {:.1}", comm(p2), ns2 as f64 / win));
                }
                b
            } else if sys.busy.map(|b| b > 0.8).unwrap_or(false) {
                Blame::new("unknown", "processes wits cannot see (other users' or the system's) are keeping the CPUs busy; sudo wits sees them".into())
            } else {
                Blame::new("unknown", "no visible process is hogging the CPUs".into())
            };
            (what, blame)
        }
        Cat::Disk | Cat::Memory | Cat::Lock | Cat::Peer => {
            let (wt, res) = top_wait(cause)?;
            match cause {
                Cat::Disk => {
                    let (r, wr) = match (&w.usage0, &w.usage1) {
                        (Some(a), Some(b)) => (
                            b.disk_read.saturating_sub(a.disk_read),
                            b.disk_written.saturating_sub(a.disk_written),
                        ),
                        _ => (0, 0),
                    };
                    let io = others
                        .as_ref()
                        .map(|o| o.delta(|u| u.disk_read + u.disk_written))
                        .unwrap_or_default();
                    let blame = match io.first() {
                        Some(&(p, b)) if b > (r + wr) * 2 && b > 1_000_000 => {
                            let mut bl = process_blame(p, 0, "", true);
                            bl.text = format!(
                                "{}, which did {} of I/O in the same window (this process: {})",
                                bl.text,
                                fmt_bytes(b),
                                fmt_bytes(r + wr)
                            );
                            bl
                        }
                        _ => Blame::new(
                            "self",
                            format!(
                                "its own I/O: it read {}, wrote {}; no other visible process is doing more",
                                fmt_bytes(r),
                                fmt_bytes(wr)
                            ),
                        ),
                    };
                    (res.label.clone(), blame)
                }
                Cat::Memory => {
                    let lvl = match sys.pressure {
                        4 => "critical",
                        2 => "warning",
                        _ => "normal",
                    };
                    let big = others
                        .as_ref()
                        .and_then(|o| o.after.iter().max_by_key(|x| x.1.footprint).map(|(p, u)| (*p, u.footprint)));
                    let own_fp = w.usage1.map(|u| u.footprint).unwrap_or(0);
                    let blame = match big {
                        Some((p, fp)) if fp > own_fp => {
                            let mut b = process_blame(p, 0, "", true);
                            b.text = format!(
                                "the machine is short of memory (pressure {lvl}); the biggest visible process is {} at {}",
                                b.text,
                                fmt_bytes(fp)
                            );
                            b
                        }
                        _ => Blame::new(
                            "self",
                            format!("itself, at {} (memory pressure {lvl})", fmt_bytes(own_fp)),
                        ),
                    };
                    let what = format!("{}; {} pages swapped in during the window", res.label, sys.swapins);
                    (what, blame)
                }
                Cat::Lock => {
                    let b = if opts.light {
                        Blame::new("unknown", String::new())
                    } else {
                        lock_blame(&cx, &res, &wt, &per_thread, &threads)
                    };
                    (res.label.clone(), b)
                }
                _ => {
                    let b = if opts.light {
                        Blame::new("unknown", String::new())
                    } else {
                        peer_blame(&cx, &res, &wt, false)
                    };
                    (res.label.clone(), b)
                }
            }
        }
        Cat::Idle => unreachable!(),
    };
    let mut what = what;
    // A wait that is all one thread's: name the thread.
    if threads.len() > 1 && matches!(cause, Cat::Lock | Cat::Peer | Cat::Disk) {
        if let Some((wt, _)) = top_wait(cause) {
            let all = wait_ns.get(&wt).copied().unwrap_or(0.0);
            let one = tid_w
                .iter()
                .filter(|((_, w2), _)| *w2 == wt)
                .max_by(|a, b| a.1.total_cmp(b.1));
            if let Some(((tid, _), ns)) = one {
                if all > 0.0 && *ns >= 0.8 * all {
                    let n = threads.get(tid).map(|t| t.name.as_str()).unwrap_or("");
                    if n.is_empty() {
                        what.push_str(&format!(", in thread {tid}"));
                    } else {
                        what.push_str(&format!(", in thread {tid} ({n})"));
                    }
                }
            }
        }
    }
    ans.what = what;
    ans.blame = Some(blame);
    ans.confidence = if blind && matches!(cause, Cat::Lock | Cat::Peer | Cat::Disk | Cat::Memory) {
        "low"
    } else if samples < 30 || share < 0.6 || blind || snapshot_only {
        "medium"
    } else {
        "high"
    };
    // The descriptor was guessed.
    if matches!(cause, Cat::Peer) && cx.many.get() > 0 {
        ans.confidence = "low";
    } else if matches!(cause, Cat::Peer) && cx.unsure.get() && ans.confidence == "high" {
        ans.confidence = "medium";
        ans.notes.push("macOS does not show which descriptor a blocked read or write is on; wits picked the likely one".into());
    }
    Some((ans, code_stacks))
}

/// CPU time and run state of any process, from the setuid /bin/ps: the
/// only view macOS gives of another user's process without root.
fn ps(pid: i32) -> Option<(f64, char)> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "time=,state=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let t = it.next()?;
    let st = it.next()?.chars().next()?;
    // [[h:]m]m:ss.cc
    let secs = t
        .split(':')
        .try_fold(0.0, |acc, p| p.parse::<f64>().ok().map(|v| acc * 60.0 + v))?;
    Some((secs * 1e9, st))
}

/// Another user's process: what `ps` shows over the window.
fn limited(pid: i32, short: &ffi::Short, followed: Vec<(i32, String)>, dur: Duration) -> Option<Answer> {
    let start = Instant::now();
    let (t0, _) = ps(pid)?;
    let mut states: HashMap<char, u32> = HashMap::new();
    let mut n = 0u32;
    let step = Duration::from_millis(100);
    let mut t1 = t0;
    while start.elapsed() < dur {
        std::thread::sleep(step.min(dur.saturating_sub(start.elapsed())));
        let Some((t, st)) = ps(pid) else { break };
        t1 = t;
        *states.entry(st).or_default() += 1;
        n += 1;
    }
    let window = start.elapsed();
    let win = window.as_nanos() as f64;
    let frac = |c: char| states.get(&c).copied().unwrap_or(0) as f64 / n.max(1) as f64;
    let cpu = ((t1 - t0) / win).max(0.0);
    let d = frac('U');
    let stopped = frac('T');
    let mut a = Answer {
        pid,
        name: short.comm.clone(),
        cmdline: ffi::cmdline(pid).or_else(|| ffi::path(pid)).unwrap_or_default(),
        followed,
        window_ms: window.as_millis() as u64,
        threads: 1,
        samples: n,
        state: "unknown",
        cause: None,
        share: 0.0,
        active: cpu + d,
        what: String::new(),
        blame: None,
        breakdown: CAUSES.iter().map(|c| (*c, 0.0)).collect(),
        idle: (1.0 - cpu - d).max(0.0),
        idle_what: "asleep or blocked; macOS shows no more of another user's process".into(),
        confidence: "low",
        notes: vec![format!(
            "limited view: macOS shows only CPU time and run state of another user's process (owner uid {}); sudo wits sees what it waits on",
            short.uid
        )],
        code: None,
        pod: None,
    };
    if stopped > 0.5 {
        a.state = "stopped";
        a.idle_what = "stopped by a signal (Ctrl-Z, SIGSTOP or a debugger)".into();
    } else if cpu >= 0.05 && cpu >= d {
        a.state = Cat::Cpu.state();
        a.cause = Some(Cat::Cpu);
        a.share = cpu / (cpu + d);
        a.what = format!("on CPU ({cpu:.2} CPUs); macOS does not show another user's stacks or waits");
        a.blame = Some(Blame::new(
            "self",
            format!("itself, as far as macOS shows: it is using {cpu:.2} CPUs"),
        ));
    } else if d >= 0.05 {
        a.state = Cat::Disk.state();
        a.cause = Some(Cat::Disk);
        a.share = d / (cpu + d);
        a.what = "uninterruptible waits (most likely disk I/O)".into();
        a.blame = Some(Blame::new("unknown", "macOS does not say on what without root".into()));
    }
    if a.cause.is_some() {
        a.breakdown = CAUSES
            .iter()
            .map(|c| {
                let v = match c {
                    Cat::Cpu => cpu,
                    Cat::Disk => d,
                    _ => 0.0,
                };
                (*c, v / (cpu + d))
            })
            .collect();
        a.breakdown.sort_by(|x, y| y.1.total_cmp(&x.1));
    }
    Some(a)
}

fn lock_blame(
    cx: &Ctx,
    res: &Res,
    w: &W,
    per_thread: &HashMap<u64, Vec<(Key, f64)>>,
    threads: &HashMap<u64, Th>,
) -> Blame {
    if !matches!(w, W::L(Leaf::Lock(_))) {
        // A file lock: who else has the file open?
        if let Some(Kind::File(v)) = res.fd.and_then(|fd| cx.fd(fd)).map(|f| &f.kind) {
            return match pick(fds::file_holders(v.dev, v.ino, cx.pid), cx.pid) {
                Some((p, more)) => {
                    let mut b = process_blame(p, more, "probably held by ", false);
                    b.text.push_str(&format!("; it has {} open too", v.path));
                    b
                }
                None => Blame::new("unknown", "held by a process wits cannot see".into()),
            };
        }
        return Blame::new("unknown", "held by a process wits cannot see".into());
    }
    // Threads that are not themselves queued on a lock could hold it.
    let mut cands: Vec<(u64, f64, String)> = per_thread
        .iter()
        .filter(|(_, parts)| {
            let lock: f64 = parts
                .iter()
                .filter(|(k, _)| matches!(k, Key::W(W::L(Leaf::Lock(_)))))
                .map(|x| x.1)
                .sum();
            let all: f64 = parts.iter().map(|x| x.1).sum();
            lock < 0.5 * all
        })
        .map(|(id, parts)| {
            let busy: f64 = parts
                .iter()
                .filter(|(k, _)| !matches!(k, Key::W(w) if cx.resolve(w).cat == Cat::Idle))
                .map(|x| x.1)
                .sum();
            (*id, busy, activity(parts, cx))
        })
        .collect();
    cands.sort_by(|a, b| b.1.total_cmp(&a.1));
    let name = |id: u64| {
        let n = threads.get(&id).map(|t| t.name.clone()).unwrap_or_default();
        if n.is_empty() {
            format!("thread {id}")
        } else {
            format!("thread {id} ({n})")
        }
    };
    match cands.as_slice() {
        [] => Blame::new("unknown", "another thread of the process; macOS does not show which holds it".into()),
        [(id, _, act)] => Blame {
            kind: "thread",
            pid: None,
            name: Some(name(*id)),
            text: format!("probably {}, the only thread not queued for it, which is {act}", name(*id)),
            ..Blame::default()
        },
        [(id, _, act), rest @ ..] => Blame {
            kind: "thread",
            pid: None,
            name: Some(name(*id)),
            text: format!(
                "another thread; the likeliest is {}, which is {act} ({} other candidates; macOS does not show the owner)",
                name(*id),
                rest.len()
            ),
            ..Blame::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::Frame;

    fn fr(f: &str, m: &str) -> Frame {
        Frame {
            func: f.into(),
            module: m.into(),
            ..Frame::default()
        }
    }

    fn tcp(fd: i32, state: i32, port: u16) -> Fd {
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 255, 255, 1));
        Fd {
            fd,
            kind: Kind::Socket(ffi::Socket {
                flags: 0,
                kind: 2,
                pcb: 0,
                local: ffi::SockAddr::In { ip, port: 50000 },
                remote: ffi::SockAddr::In { ip, port },
                tcp_state: state,
                rcv: 0,
                snd: 0,
                unix_peer_pcb: 0,
                so_options: 0,
            }),
        }
    }

    fn cx(fds: &[Fd]) -> Ctx<'_> {
        Ctx {
            pid: 1,
            fds,
            fds_readable: true,
            swap_in: false,
            pressure: 0,
            wrote: false,
            unsure: std::cell::Cell::new(false),
            many: std::cell::Cell::new(0),
        }
    }

    #[test]
    fn event_loop_waits_on_a_connect_in_progress() {
        // Node's loop (uv__io_poll -> kevent) while a connect has no answer.
        let fds = [tcp(15, 2, 8080)];
        let r = cx(&fds).resolve(&W::L(Leaf::RunLoop));
        assert_eq!(r.cat, Cat::Peer);
        assert_eq!(r.fd, Some(15));
        assert!(r.label.starts_with("connecting: "), "{}", r.label);
        assert_eq!(cx(&fds).resolve(&W::L(Leaf::Events("kevent".into()))).cat, Cat::Peer);
        // Connected, or a server with a listener: still an idle loop.
        let fds = [tcp(15, 4, 8080)];
        assert_eq!(cx(&fds).resolve(&W::L(Leaf::RunLoop)).cat, Cat::Idle);
        let fds = [tcp(15, 2, 8080), tcp(16, 1, 0)];
        assert_eq!(cx(&fds).resolve(&W::L(Leaf::RunLoop)).cat, Cat::Idle);
        assert_eq!(cx(&[]).resolve(&W::L(Leaf::RunLoop)).cat, Cat::Idle);
    }

    #[test]
    fn split_uses_measured_cpu_and_sampled_waits() {
        let th = Th {
            n: 100,
            running: 30,
            waiting: 70,
            cpu0: Some((0, 0)),
            cpu1: (300_000_000, 0),
            ..Th::default()
        };
        let st = SThread {
            id: 1,
            name: String::new(),
            total: 100,
            stacks: vec![
                (vec![fr("work", "app")], 30),
                (vec![fr("__psynch_mutexwait", "libsystem_kernel.dylib"), fr("pthread_mutex_lock", "libsystem_pthread.dylib")], 70),
            ],
        };
        let parts = split(&th, 1e9, 0.0, Some(&st));
        let get = |k: Key| parts.iter().filter(|x| x.0 == k).map(|x| x.1).sum::<f64>();
        assert!((get(Key::Cpu) - 3e8).abs() < 1.0);
        assert!((get(Key::W(W::L(Leaf::Lock("a mutex".into())))) - 7e8).abs() < 1.0);
    }

    #[test]
    fn runqueue_scales_to_measured_total() {
        let mut t = HashMap::new();
        t.insert(1, Th { n: 10, running: 10, cpu0: Some((0, 0)), cpu1: (5e8 as u64, 0), ..Th::default() });
        t.insert(2, Th { n: 10, waiting: 10, cpu0: Some((0, 0)), cpu1: (0, 0), ..Th::default() });
        let walls: HashMap<u64, f64> = [(1, 1e9), (2, 1e9)].into_iter().collect();
        let r = runqueue(&t, &walls, Some(2e8));
        assert!((r[&1] - 2e8).abs() < 1.0);
        assert_eq!(r[&2], 0.0);
    }
}
