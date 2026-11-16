//! The sampler: watches every thread of one process for a short window and
//! records, for each thread, how long it ran, how long it queued for a CPU,
//! and what it was blocked on the rest of the time.

use crate::proc::{self, Syscall};
use crate::sysno::{self, Kind};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::time::{Duration, Instant};

pub const EV_EPOLL: u8 = 0;
pub const EV_POLL: u8 = 1;
pub const EV_SELECT: u8 = 2;
/// Event wait whose watched set cannot be seen.
pub const EV_BLIND: u8 = 3;

/// What a blocked thread was waiting on at one sample.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Wait {
    Disk { sys: &'static str, fd: i32 },
    /// Uninterruptible wait outside a system call: a page being read in.
    Fault,
    Mem { why: String },
    Futex { addr: u64, lockish: bool, pi: bool },
    FileLock { fd: i32 },
    KLock { why: String },
    /// Blocking read or write on a descriptor.
    Io { fd: i32, write: bool, sys: &'static str },
    Connect { fd: i32 },
    /// Blocking open (a FIFO with no partner yet).
    Open { ptr: u64 },
    Events { kind: u8, a: u64, n: u64 },
    Ipc { sys: &'static str },
    Child,
    Sleep,
    Accept { fd: i32 },
    Stopped,
    Park { what: &'static str },
    /// Known only from the kernel wait symbol: "pipe" or "socket".
    Blind { what: &'static str },
    Unknown { why: String, d: bool },
}

fn has(w: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| w.contains(p))
}

const W_MEM: &[&str] = &["mem_cgroup", "reclaim", "swap", "throttle_direct", "compact", "oom_"];
const W_DISK: &[&str] = &[
    "io_schedule", "folio_wait", "wait_on_page", "bit_wait_io", "blk_", "bio_", "jbd2", "ext4",
    "xfs_", "btrfs", "wbt_", "rq_qos", "balance_dirty", "writeback", "wait_on_buffer",
    "inode_wait", "nfs", "fuse", "request_wait_answer", "submit_bio",
];
const W_KLOCK: &[&str] = &["rwsem", "mutex", "down_", "percpu_rwsem"];
const W_FLOCK: &[&str] = &["flock", "locks_lock", "lock_file", "posix_lock", "fcntl_setlk"];
const W_SLEEP: &[&str] = &["nanosleep", "nsleep"];
const W_EVENTS: &[&str] = &["ep_poll", "epoll", "do_sys_poll", "poll_schedule", "do_select", "select"];
const W_SOCK: &[&str] = &["sk_wait", "tcp_", "unix_", "skb_", "sock_", "inet_"];

fn futex_wait(args: &[u64; 6]) -> Wait {
    let cmd = args[1] & 0x7f;
    match cmd {
        // FUTEX_LOCK_PI, FUTEX_LOCK_PI2
        6 | 13 => Wait::Futex { addr: args[0], lockish: true, pi: true },
        // FUTEX_WAIT, FUTEX_WAIT_BITSET. glibc, Rust std and the Go runtime
        // all park contended-mutex waiters with an expected value of 2.
        0 | 9 => Wait::Futex { addr: args[0], lockish: args[2] as u32 == 2, pi: false },
        _ => Wait::Park { what: "futex" },
    }
}

fn by_wchan(w: Option<&str>, d: bool) -> Wait {
    let Some(w) = w else {
        return Wait::Unknown { why: String::new(), d };
    };
    if has(w, W_MEM) {
        Wait::Mem { why: w.to_string() }
    } else if w.contains("futex") {
        let pi = w.contains("lock_pi");
        Wait::Futex { addr: 0, lockish: pi, pi }
    } else if has(w, W_FLOCK) {
        Wait::FileLock { fd: -1 }
    } else if has(w, W_DISK) {
        Wait::Disk { sys: "", fd: -1 }
    } else if has(w, W_KLOCK) {
        Wait::KLock { why: w.to_string() }
    } else if w.contains("do_wait") || w.contains("vfork") {
        Wait::Child
    } else if has(w, W_SLEEP) {
        Wait::Sleep
    } else if has(w, W_EVENTS) {
        Wait::Events { kind: EV_BLIND, a: 0, n: 0 }
    } else if w.contains("accept") {
        Wait::Accept { fd: -1 }
    } else if w.contains("partner") || w.contains("fifo_open") {
        Wait::Open { ptr: 0 }
    } else if w.contains("pipe") {
        Wait::Blind { what: "pipe" }
    } else if w.contains("tty") {
        Wait::Park { what: "terminal input" }
    } else if has(w, W_SOCK) {
        Wait::Blind { what: "socket" }
    } else if w.contains("sigsuspend") || w.contains("sigtimedwait") || w.contains("pause") {
        Wait::Park { what: "a signal" }
    } else {
        Wait::Unknown { why: w.to_string(), d }
    }
}

/// Decide what one sample of one thread means. `None` means the thread was
/// running or runnable (or is dead); the scheduler counters cover that time.
pub fn classify(state: char, sc: Option<Syscall>, wchan: Option<&str>) -> Option<Wait> {
    match state {
        'R' | 'Z' | 'X' => return None,
        'T' | 't' => return Some(Wait::Stopped),
        _ => {}
    }
    let d = state == 'D';
    if let Some(w) = wchan {
        if has(w, W_MEM) {
            return Some(Wait::Mem { why: w.to_string() });
        }
    }
    let (nr, args) = match sc {
        Some(Syscall::Running) => return None,
        Some(Syscall::Outside) => {
            return Some(if d {
                match by_wchan(wchan, d) {
                    w @ (Wait::KLock { .. } | Wait::Mem { .. }) => w,
                    _ => Wait::Fault,
                }
            } else {
                by_wchan(wchan, d)
            });
        }
        Some(Syscall::In { nr, args }) => (nr, args),
        None => return Some(by_wchan(wchan, d)),
    };
    let (kind, name) = sysno::lookup(nr);
    let fd = args[0] as i32;
    Some(match kind {
        Kind::Read | Kind::Write if d => Wait::Disk { sys: name, fd },
        Kind::Read => Wait::Io { fd, write: false, sys: name },
        Kind::Write => Wait::Io { fd, write: true, sys: name },
        Kind::Sync => Wait::Disk { sys: name, fd },
        Kind::SyncAll | Kind::FileMeta => Wait::Disk { sys: name, fd: -1 },
        Kind::Open0 | Kind::Open1 if d => Wait::Disk { sys: name, fd: -1 },
        Kind::Open0 => Wait::Open { ptr: args[0] },
        Kind::Open1 => Wait::Open { ptr: args[1] },
        Kind::Connect => Wait::Connect { fd },
        Kind::Accept => Wait::Accept { fd },
        Kind::Poll if args[1] == 0 => Wait::Sleep,
        Kind::Poll => Wait::Events { kind: EV_POLL, a: args[0], n: args[1] },
        Kind::Select if args[0] == 0 => Wait::Sleep,
        Kind::Select => Wait::Events {
            kind: EV_SELECT,
            a: if args[1] != 0 { args[1] } else { args[2] },
            n: args[0],
        },
        Kind::Epoll => Wait::Events { kind: EV_EPOLL, a: args[0], n: 0 },
        Kind::Futex => futex_wait(&args),
        Kind::Flock => Wait::FileLock { fd },
        // F_SETLKW, F_OFD_SETLKW
        Kind::Fcntl if args[1] == 7 || args[1] == 38 => Wait::FileLock { fd },
        Kind::Fcntl => Wait::Park { what: "fcntl" },
        Kind::Wait | Kind::Fork => Wait::Child,
        Kind::Sleep => Wait::Sleep,
        Kind::SigWait => Wait::Park { what: "a signal" },
        Kind::Uring => Wait::Events { kind: EV_BLIND, a: 0, n: 0 },
        Kind::Ipc => Wait::Ipc { sys: name },
        Kind::Other => match by_wchan(wchan, d) {
            Wait::Unknown { why, d } if !d && why.is_empty() => Wait::Park { what: "a system call" },
            w => w,
        },
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum FdKind {
    Pipe,
    Fifo,
    Socket(u64),
    Tty,
    File,
    BlockDev,
    CharDev,
    /// eventfd, timerfd, signalfd, inotify, epoll...
    Anon,
    Other,
}

#[derive(Debug, Clone)]
pub struct FdInfo {
    pub link: String,
    pub kind: FdKind,
    pub dev: u64,
    pub ino: u64,
}

pub fn fd_info(pid: i32, fd: i32) -> Option<FdInfo> {
    let p = format!("/proc/{pid}/fd/{fd}");
    let link = fs::read_link(&p).ok()?.to_string_lossy().into_owned();
    let md = fs::metadata(&p).ok();
    let (dev, ino) = md.as_ref().map(|m| (m.dev(), m.ino())).unwrap_or((0, 0));
    let kind = if let Some(r) = link.strip_prefix("socket:[") {
        FdKind::Socket(r.trim_end_matches(']').parse().unwrap_or(0))
    } else if link.starts_with("pipe:[") {
        FdKind::Pipe
    } else if link.starts_with("anon_inode:") {
        FdKind::Anon
    } else if let Some(m) = &md {
        let ft = m.file_type();
        if ft.is_fifo() {
            FdKind::Fifo
        } else if ft.is_char_device() {
            if link.starts_with("/dev/pts/") || link.starts_with("/dev/tty") || link == "/dev/console" {
                FdKind::Tty
            } else {
                FdKind::CharDev
            }
        } else if ft.is_block_device() {
            FdKind::BlockDev
        } else if ft.is_file() {
            FdKind::File
        } else {
            FdKind::Other
        }
    } else {
        FdKind::Other
    };
    Some(FdInfo { link, kind, dev, ino })
}

#[derive(Debug, Default)]
pub struct Th {
    pub comm: String,
    pub sched0: Option<(u64, u64)>,
    pub sched1: Option<(u64, u64)>,
    pub utime: (u64, u64),
    pub stime: (u64, u64),
    pub majflt: (u64, u64),
    /// Samples taken while the thread existed.
    pub n: u32,
    pub running: u32,
    pub waits: HashMap<Wait, u32>,
}

pub struct Profile {
    pub pid: i32,
    pub comm: String,
    pub window: Duration,
    pub ticks: u32,
    pub threads: HashMap<i32, Th>,
    pub fds: HashMap<i32, FdInfo>,
    pub fds_readable: bool,
    pub evsets: HashMap<(u8, u64, u64), Option<Vec<i32>>>,
    pub strs: HashMap<u64, String>,
    /// futex address -> owner thread id -> times seen
    pub owners: HashMap<u64, HashMap<i32, u32>>,
    /// At least one thread's system call was readable.
    pub saw_syscall: bool,
    /// The kernel refused to show a system call.
    pub denied: bool,
    pub gone: bool,
    pub capped: bool,
    pub locks: Option<String>,
    pub nice: i64,
    mem: Option<Option<fs::File>>,
}

const MAX_THREADS: usize = 512;

impl Profile {
    fn mem_read(&mut self, addr: u64, len: usize) -> Option<Vec<u8>> {
        if addr == 0 || len == 0 {
            return None;
        }
        let pid = self.pid;
        let f = self
            .mem
            .get_or_insert_with(|| fs::File::open(format!("/proc/{pid}/mem")).ok())
            .as_ref()?;
        let mut buf = vec![0u8; len];
        let n = f.read_at(&mut buf, addr).ok()?;
        if n == 0 {
            return None;
        }
        buf.truncate(n);
        Some(buf)
    }

    fn need_fd(&mut self, fd: i32) {
        if fd >= 0 && !self.fds.contains_key(&fd) {
            if let Some(i) = fd_info(self.pid, fd) {
                self.fds.insert(fd, i);
            }
        }
    }

    fn load_fds(&mut self) {
        let Ok(rd) = fs::read_dir(format!("/proc/{}/fd", self.pid)) else {
            return;
        };
        self.fds_readable = true;
        for e in rd.flatten().take(4096) {
            if let Some(fd) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                self.need_fd(fd);
            }
        }
    }

    fn resolve_events(&mut self, kind: u8, a: u64, n: u64) -> Option<Vec<i32>> {
        match kind {
            EV_EPOLL => {
                let s = proc::slurp(&format!("/proc/{}/fdinfo/{}", self.pid, a))?;
                Some(
                    s.lines()
                        .filter_map(|l| l.strip_prefix("tfd:"))
                        .filter_map(|r| r.split_ascii_whitespace().next()?.parse().ok())
                        .collect(),
                )
            }
            EV_POLL => {
                let n = n.min(256) as usize;
                let b = self.mem_read(a, n * 8)?;
                Some(
                    b.chunks_exact(8)
                        .map(|c| i32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                        .filter(|&fd| fd >= 0)
                        .collect(),
                )
            }
            EV_SELECT => {
                let n = n.min(1024) as usize;
                let b = self.mem_read(a, n.div_ceil(8))?;
                Some(
                    (0..n)
                        .filter(|i| b.get(i / 8).map(|x| x >> (i % 8) & 1 == 1).unwrap_or(false))
                        .map(|i| i as i32)
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// First-sight bookkeeping for a wait: resolve descriptors, strings and
    /// lock owners while the process is still in that state.
    fn note(&mut self, w: &Wait, tid: i32, tids: &[i32]) {
        match *w {
            Wait::Disk { fd, .. } | Wait::Io { fd, .. } | Wait::Connect { fd } | Wait::Accept { fd } => {
                self.need_fd(fd)
            }
            Wait::FileLock { fd } => {
                self.need_fd(fd);
                if self.locks.is_none() {
                    self.locks = proc::slurp("/proc/locks");
                }
            }
            Wait::Events { kind, a, n } => {
                if !self.evsets.contains_key(&(kind, a, n)) {
                    let set = self.resolve_events(kind, a, n);
                    if let Some(v) = &set {
                        for &fd in v.clone().iter() {
                            self.need_fd(fd);
                        }
                    }
                    self.evsets.insert((kind, a, n), set);
                }
            }
            Wait::Open { ptr } => {
                if ptr != 0 && !self.strs.contains_key(&ptr) {
                    if let Some(b) = self.mem_read(ptr, 256) {
                        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                        self.strs.insert(ptr, String::from_utf8_lossy(&b[..end]).into_owned());
                    }
                }
            }
            Wait::Futex { addr, lockish: true, pi } => {
                let owner = if pi {
                    self.mem_read(addr, 4).map(|b| u32_at(&b, 0) & 0x3fff_ffff)
                } else {
                    // glibc pthread_mutex_t: { int lock; unsigned count; int owner; ... }
                    self.mem_read(addr + 8, 4).map(|b| u32_at(&b, 0))
                };
                if let Some(o) = owner {
                    let o = o as i32;
                    if o != tid && tids.contains(&o) {
                        *self.owners.entry(addr).or_default().entry(o).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    if b.len() < i + 4 {
        return 0;
    }
    u32::from_ne_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn list_tids(pid: i32, capped: &mut bool) -> Vec<i32> {
    let mut t = proc::tids(pid);
    t.sort_unstable();
    if t.len() > MAX_THREADS {
        t.truncate(MAX_THREADS);
        *capped = true;
    }
    t
}

/// Sample `pid` for `dur`. Returns `None` if the process does not exist.
pub fn sample(pid: i32, dur: Duration) -> Option<Profile> {
    let st = proc::stat(pid)?;
    let mut p = Profile {
        pid,
        comm: st.comm.clone(),
        window: Duration::ZERO,
        ticks: 0,
        threads: HashMap::new(),
        fds: HashMap::new(),
        fds_readable: false,
        evsets: HashMap::new(),
        strs: HashMap::new(),
        owners: HashMap::new(),
        saw_syscall: false,
        denied: false,
        gone: false,
        capped: false,
        locks: None,
        nice: st.nice,
        mem: None,
    };
    p.load_fds();
    let mut tids = list_tids(pid, &mut p.capped);
    let tick = Duration::from_millis(match tids.len() {
        0..=16 => 10,
        17..=128 => 20,
        _ => 50,
    });
    let start = Instant::now();
    let deadline = start + dur;
    let mut i: u32 = 0;
    loop {
        if i > 0 && i % 8 == 0 {
            tids = list_tids(pid, &mut p.capped);
        }
        let mut alive = false;
        for &tid in &tids {
            let Some(s) = proc::task_stat(pid, tid) else {
                continue;
            };
            if s.state == 'Z' || s.state == 'X' {
                continue;
            }
            alive = true;
            let sched = proc::task_schedstat(pid, tid);
            let wait = if s.state == 'R' {
                None
            } else {
                let base = format!("/proc/{pid}/task/{tid}");
                let sc = match proc::slurp_perm(&format!("{base}/syscall")) {
                    Ok(t) => {
                        p.saw_syscall = true;
                        proc::parse_syscall(&t)
                    }
                    Err(denied) => {
                        p.denied |= denied;
                        None
                    }
                };
                let wchan = proc::slurp(&format!("{base}/wchan"))
                    .map(|w| w.trim().to_string())
                    .filter(|w| !w.is_empty() && w != "0");
                classify(s.state, sc, wchan.as_deref())
            };
            if let Some(w) = &wait {
                p.note(w, tid, &tids);
            }
            let th = p.threads.entry(tid).or_insert_with(|| Th {
                comm: s.comm.clone(),
                sched0: sched,
                utime: (s.utime, s.utime),
                stime: (s.stime, s.stime),
                majflt: (s.majflt, s.majflt),
                ..Th::default()
            });
            if sched.is_some() {
                th.sched1 = sched;
            }
            th.utime.1 = s.utime;
            th.stime.1 = s.stime;
            th.majflt.1 = s.majflt;
            th.n += 1;
            match wait {
                None => th.running += 1,
                Some(w) => *th.waits.entry(w).or_default() += 1,
            }
        }
        i += 1;
        if !alive {
            p.gone = true;
            break;
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
    p.ticks = i;
    p.window = start.elapsed();
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(nr: libc::c_long, args: [u64; 6]) -> Option<Syscall> {
        Some(Syscall::In { nr: nr as i64, args })
    }

    #[test]
    fn running_and_dead_are_not_waits() {
        assert_eq!(classify('R', None, None), None);
        assert_eq!(classify('Z', None, None), None);
        assert_eq!(classify('S', Some(Syscall::Running), None), None);
    }

    #[test]
    fn stopped() {
        assert_eq!(classify('T', None, None), Some(Wait::Stopped));
    }

    #[test]
    fn blocking_read_is_io_but_uninterruptible_read_is_disk() {
        let a = [5, 0, 0, 0, 0, 0];
        assert_eq!(
            classify('S', sc(libc::SYS_read, a), Some("anon_pipe_read")),
            Some(Wait::Io { fd: 5, write: false, sys: "read" })
        );
        assert_eq!(
            classify('D', sc(libc::SYS_read, a), Some("folio_wait_bit_common")),
            Some(Wait::Disk { sys: "read", fd: 5 })
        );
    }

    #[test]
    fn fsync_is_disk() {
        assert_eq!(
            classify('D', sc(libc::SYS_fsync, [9, 0, 0, 0, 0, 0]), None),
            Some(Wait::Disk { sys: "fsync", fd: 9 })
        );
    }

    #[test]
    fn futex_mutex_versus_park() {
        // FUTEX_WAIT_PRIVATE, expected value 2: a contended mutex
        assert_eq!(
            classify('S', sc(libc::SYS_futex, [0x1000, 128, 2, 0, 0, 0]), None),
            Some(Wait::Futex { addr: 0x1000, lockish: true, pi: false })
        );
        // FUTEX_WAIT_BITSET_PRIVATE on a condition variable
        assert_eq!(
            classify('S', sc(libc::SYS_futex, [0x1000, 137, 0, 0, 0, 0]), None),
            Some(Wait::Futex { addr: 0x1000, lockish: false, pi: false })
        );
        // FUTEX_LOCK_PI
        assert_eq!(
            classify('S', sc(libc::SYS_futex, [0x1000, 6, 0, 0, 0, 0]), None),
            Some(Wait::Futex { addr: 0x1000, lockish: true, pi: true })
        );
    }

    #[test]
    fn file_locks() {
        assert_eq!(
            classify('S', sc(libc::SYS_flock, [4, 2, 0, 0, 0, 0]), None),
            Some(Wait::FileLock { fd: 4 })
        );
        assert_eq!(
            classify('S', sc(libc::SYS_fcntl, [4, 7, 0, 0, 0, 0]), None),
            Some(Wait::FileLock { fd: 4 })
        );
        assert_eq!(classify('S', None, Some("flock_lock_inode_wait")), Some(Wait::FileLock { fd: -1 }));
    }

    #[test]
    fn page_fault_and_memory() {
        assert_eq!(classify('D', Some(Syscall::Outside), Some("folio_wait_bit_common")), Some(Wait::Fault));
        assert!(matches!(
            classify('D', sc(libc::SYS_read, [3, 0, 0, 0, 0, 0]), Some("mem_cgroup_handle_over_high")),
            Some(Wait::Mem { .. })
        ));
    }

    #[test]
    fn without_syscall_falls_back_to_wait_symbol() {
        assert_eq!(classify('S', None, Some("anon_pipe_read")), Some(Wait::Blind { what: "pipe" }));
        assert_eq!(classify('S', None, Some("hrtimer_nanosleep")), Some(Wait::Sleep));
        assert_eq!(classify('S', None, Some("do_wait")), Some(Wait::Child));
        assert_eq!(classify('S', None, Some("ep_poll")), Some(Wait::Events { kind: EV_BLIND, a: 0, n: 0 }));
        assert_eq!(classify('D', None, Some("io_schedule")), Some(Wait::Disk { sys: "", fd: -1 }));
        assert_eq!(classify('S', None, None), Some(Wait::Unknown { why: String::new(), d: false }));
    }

    #[test]
    fn sleeps_and_children() {
        assert_eq!(classify('S', sc(libc::SYS_clock_nanosleep, [0; 6]), None), Some(Wait::Sleep));
        assert_eq!(classify('S', sc(libc::SYS_wait4, [0; 6]), None), Some(Wait::Child));
        assert_eq!(classify('S', sc(libc::SYS_ppoll, [0x10, 0, 0, 0, 0, 0]), None), Some(Wait::Sleep));
        assert_eq!(
            classify('S', sc(libc::SYS_ppoll, [0x10, 2, 0, 0, 0, 0]), None),
            Some(Wait::Events { kind: EV_POLL, a: 0x10, n: 2 })
        );
    }
}
