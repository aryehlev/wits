//! Stacks on macOS come from Apple's /usr/bin/sample: it may examine the
//! user's own processes without root (it carries the entitlement wits
//! lacks), samples every thread whatever its state, and symbolizes with
//! file and line when debug information exists. wits runs it alongside its
//! own libproc sampling and reads its call tree.
//!
//! The leaf of each stack is what the thread was doing: a system call stub
//! in libsystem_kernel when blocked, its own code when running.

use crate::code::{Frame, Stacks};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct SThread {
    /// The unique thread id (PROC_PIDLISTTHREADIDS).
    pub id: u64,
    pub name: String,
    pub total: u32,
    /// Stacks (leaf first) and how many samples ended exactly there.
    pub stacks: Vec<(Vec<Frame>, u32)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleOut {
    pub interval_ms: f64,
    pub threads: Vec<SThread>,
    /// Paths of the binary images loaded in the process.
    pub images: Vec<String>,
}

impl SampleOut {
    pub fn thread(&self, id: u64) -> Option<&SThread> {
        self.threads.iter().find(|t| t.id == id)
    }
}

fn parse_frame(rest: &str) -> Frame {
    let (func, after) = match rest.find("  (in ") {
        Some(i) => (rest[..i].trim(), &rest[i + 6..]),
        None => match rest.find("  [") {
            Some(i) => (rest[..i].trim(), &rest[i..]),
            None => (rest.trim(), ""),
        },
    };
    let module = after.split(')').next().unwrap_or("").trim();
    let module = if after.is_empty() { "" } else { module };
    // file:line follows the last "]"
    let tail = after.rsplit_once(']').map(|x| x.1.trim()).unwrap_or("");
    let (file, line) = match tail.rsplit_once(':') {
        Some((f, l)) if !f.is_empty() => match l.trim().parse::<u32>() {
            Ok(n) => (Some(f.to_string()), Some(n)),
            Err(_) => (None, None),
        },
        _ => (None, None),
    };
    Frame {
        func: if func == "???" || func.is_empty() {
            "[unknown]".into()
        } else {
            func.to_string()
        },
        module: if module == "<unknown binary>" {
            String::new()
        } else {
            module.to_string()
        },
        file,
        line,
        kernel: false,
    }
}

/// Read sample's report: the "Call graph:" tree and the binary images.
pub fn parse(text: &str) -> SampleOut {
    let mut out = SampleOut::default();
    let mut in_graph = false;
    let mut in_images = false;
    // (depth, frame, count, children's counts)
    let mut path: Vec<(usize, Frame, u32, u32)> = Vec::new();
    let mut cur: Option<SThread> = None;

    fn flush(path: &mut Vec<(usize, Frame, u32, u32)>, to: usize, th: &mut Option<SThread>) {
        while path.last().map(|n| n.0 >= to).unwrap_or(false) {
            let (_, f, count, kids) = path.pop().unwrap();
            if let Some(last) = path.last_mut() {
                last.3 += count;
            }
            let own = count.saturating_sub(kids);
            if own > 0 {
                if let Some(t) = th.as_mut() {
                    let mut st: Vec<Frame> = vec![f];
                    st.extend(path.iter().rev().map(|n| n.1.clone()));
                    t.stacks.push((st, own));
                }
            }
        }
    }

    for line in text.lines() {
        if let Some(r) = line.strip_prefix("Analysis of sampling ") {
            if let Some(i) = r.find(" every ") {
                let n: String = r[i + 7..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                let ms = n.parse::<f64>().unwrap_or(1.0);
                out.interval_ms = if r.contains("microsecond") { ms / 1000.0 } else { ms };
            }
            continue;
        }
        if line.starts_with("Call graph:") {
            in_graph = true;
            continue;
        }
        if line.starts_with("Binary Images:") {
            in_images = true;
            continue;
        }
        if in_images {
            if let Some(i) = line.rfind("> ") {
                let p = line[i + 2..].trim();
                if !p.is_empty() {
                    out.images.push(p.to_string());
                }
            }
            continue;
        }
        if !in_graph {
            continue;
        }
        if line.trim().is_empty() || !line.starts_with(' ') {
            flush(&mut path, 0, &mut cur);
            out.threads.extend(cur.take());
            in_graph = !line.trim().is_empty() && line.starts_with(' ');
            if !line.trim().is_empty() {
                in_graph = false;
            }
            continue;
        }
        let Some(pos) = line.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let depth = pos.saturating_sub(4) / 2;
        let rest = &line[pos..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let count: u32 = num.parse().unwrap_or(0);
        let rest = rest[num.len()..].trim_start();
        if depth == 0 {
            flush(&mut path, 0, &mut cur);
            out.threads.extend(cur.take());
            if let Some(t) = rest.strip_prefix("Thread_") {
                let id: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                let after = &t[id.len()..];
                let name = if let Some(n) = after.strip_prefix(':') {
                    n.trim().to_string()
                } else if let Some(i) = after.find(": ") {
                    // "   DispatchQueue_1: com.apple.main-thread  (serial)"
                    after[i + 2..].split("  ").next().unwrap_or("").trim().to_string()
                } else {
                    String::new()
                };
                cur = Some(SThread {
                    id: id.parse().unwrap_or(0),
                    name,
                    total: count,
                    stacks: Vec::new(),
                });
            }
            continue;
        }
        flush(&mut path, depth, &mut cur);
        path.push((depth, parse_frame(rest), count, 0));
    }
    flush(&mut path, 0, &mut cur);
    out.threads.extend(cur.take());
    out
}

/// A `sample` run in the background.
pub struct Running {
    how: How,
    file: PathBuf,
    err: PathBuf,
}

enum How {
    Child(Child),
    /// Not our child (see `start_detached`): known by pid only.
    Orphan(i32),
}

fn tmpfile(pid: i32, ext: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("wits-{}-{pid}-{n}.{ext}", std::process::id()))
}

fn args(pid: i32, dur: Duration, interval_ms: u32, file: &std::path::Path) -> Vec<String> {
    vec![
        pid.to_string(),
        format!("{:.2}", dur.as_secs_f64().max(0.05)),
        interval_ms.to_string(),
        "-mayDie".into(),
        "-fullPaths".into(),
        "-file".into(),
        file.to_string_lossy().into_owned(),
    ]
}

/// Start sampling `pid` for `dur` every `interval_ms`.
pub fn start(pid: i32, dur: Duration, interval_ms: u32) -> Option<Running> {
    let file = tmpfile(pid, "sample.txt");
    let err = tmpfile(pid, "sample.err");
    let errf = std::fs::File::create(&err).ok()?;
    let child = Command::new("/usr/bin/sample")
        .args(args(pid, dur, interval_ms, &file))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .ok()?;
    Some(Running {
        how: How::Child(child),
        file,
        err,
    })
}

/// As `start`, but `sample` is not our child: inside `wits run` every
/// child is reaped, and a reaped child's CPU time would be counted as the
/// command's.
/// CPU time (ns) of the launcher shells `start_detached` reaped: it is in
/// our RUSAGE_CHILDREN, and is not the watched command's.
pub static LAUNCHER_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn children_cpu_ns() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

pub fn start_detached(pid: i32, dur: Duration, interval_ms: u32) -> Option<Running> {
    let file = tmpfile(pid, "sample.txt");
    let err = tmpfile(pid, "sample.err");
    let before = children_cpu_ns();
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"e=$1; shift; /usr/bin/sample "$@" </dev/null >/dev/null 2>"$e" & echo $!"#)
        .arg("sh")
        .arg(&err)
        .args(args(pid, dur, interval_ms, &file))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    LAUNCHER_NS.fetch_add(
        children_cpu_ns().saturating_sub(before),
        std::sync::atomic::Ordering::Relaxed,
    );
    let spid: i32 = String::from_utf8_lossy(&out.ok()?.stdout).trim().parse().ok()?;
    Some(Running {
        how: How::Orphan(spid),
        file,
        err,
    })
}

fn orphan_alive(pid: i32) -> bool {
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists
        && super::ffi::short(pid)
            .map(|s| s.status != 5 && s.comm == "sample")
            .unwrap_or(false)
}

impl Running {
    /// Wait for the report and read it. `Err` carries sample's complaint.
    pub fn finish(self) -> Result<SampleOut, String> {
        self.finish_within(Duration::from_secs(3600))
    }

    fn alive(&mut self) -> bool {
        match &mut self.how {
            How::Child(c) => matches!(c.try_wait(), Ok(None)),
            How::Orphan(p) => orphan_alive(*p),
        }
    }

    /// As `finish`, but give up on a report that takes longer than `limit`
    /// (symbolizing a big process the first time can take many seconds).
    pub fn finish_within(mut self, limit: Duration) -> Result<SampleOut, String> {
        let t0 = std::time::Instant::now();
        while self.alive() {
            if t0.elapsed() >= limit {
                match &mut self.how {
                    How::Child(c) => {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    How::Orphan(p) => unsafe {
                        libc::kill(*p, libc::SIGKILL);
                    },
                }
                let _ = std::fs::remove_file(&self.file);
                let _ = std::fs::remove_file(&self.err);
                return Err(format!(
                    "`sample` took over {:.0}s to report and was stopped",
                    limit.as_secs_f64()
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let How::Child(c) = &mut self.how {
            let _ = c.wait();
        }
        let err = std::fs::read_to_string(&self.err).unwrap_or_default();
        let text = std::fs::read_to_string(&self.file);
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_file(&self.err);
        match text {
            Ok(t) if t.contains("Call graph:") => Ok(parse(&t)),
            _ => Err(err
                .lines()
                .find(|l| l.contains("cannot") || l.contains("privilege") || l.contains("not"))
                .map(|l| l.split_once(": ").map(|x| x.1).unwrap_or(l).trim().to_string())
                .unwrap_or_else(|| "`sample` wrote no report".into())),
        }
    }

    /// Stop early (sample writes its report on SIGINT) and read it.
    pub fn stop(mut self) -> Result<SampleOut, String> {
        // Only while it runs: once gone, its pid could be someone else's.
        if self.alive() {
            let pid = match &self.how {
                How::Child(c) => c.id() as i32,
                How::Orphan(p) => *p,
            };
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
        self.finish()
    }
}

/// What a thread was doing at the leaf of a stack.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Leaf {
    /// Its own code, or a system call that was not blocking.
    Cpu,
    /// A contended lock: "mutex", "rwlock", "os_unfair_lock", "the GIL"...
    Lock(String),
    FileLock,
    /// fsync / F_FULLFSYNC
    Fsync,
    /// fcntl: F_FULLFSYNC or a file lock, told apart by thread state.
    Fcntl,
    /// Blocking read. `sock`: a socket call (recv*), not a plain read.
    Read { sys: String, sock: bool },
    Write { sys: String, sock: bool },
    Connect,
    Accept,
    Open,
    Child,
    Sleep,
    /// select / poll / kevent outside a dispatch or run loop
    Events(String),
    /// A run loop or dispatch waiting for messages or events.
    RunLoop,
    /// Waiting for the reply to a Mach or XPC message.
    Reply,
    /// An idle dispatch worker thread.
    Workq,
    /// A condition variable, semaphore or other park.
    Park(String),
    Signal,
    /// A file system call (stat, getattrlist, unlink...).
    FileOp(String),
    Other(String),
}

/// User functions that mean a thread is queued on a lock.
pub const LOCK_FUNCS: &[&str] = &[
    "pthread_mutex_lock",
    "_pthread_mutex_firstfit_lock_slow",
    "pthread_rwlock_rdlock",
    "pthread_rwlock_wrlock",
    "os_unfair_lock_lock",
    "_os_unfair_lock_lock_slow",
    "lock_contended",
    "Mutex::lock",
    "RwLock::read",
    "RwLock::write",
    "parking_lot::raw_mutex",
    "parking_lot::raw_rwlock",
    "take_gil",
    "PyThread_acquire_lock",
    "sync.(*Mutex).",
    "sync.(*RWMutex).",
    "runtime.lock2",
    "ObjectMonitor::enter",
    "objc_sync_enter",
    "std::mutex::lock",
    "std::__1::mutex::lock",
];

fn lock_name(frames: &[Frame]) -> Option<String> {
    for f in frames.iter().take(12) {
        let n = f.func.as_str();
        if n.contains("take_gil") || n.contains("PyThread_acquire_lock") {
            return Some(if n.contains("take_gil") {
                "the Python GIL".into()
            } else {
                "a Python lock".into()
            });
        }
        if n.contains("rwlock") || n.contains("RwLock") || n.contains("raw_rwlock") {
            return Some("a read-write lock".into());
        }
        if n.contains("os_unfair_lock") {
            return Some("an os_unfair_lock".into());
        }
        if n.contains("objc_sync_enter") {
            return Some("an @synchronized lock".into());
        }
        if LOCK_FUNCS.iter().any(|p| n.contains(p)) {
            return Some("a mutex".into());
        }
    }
    None
}

fn any(frames: &[Frame], pats: &[&str]) -> bool {
    frames
        .iter()
        .take(24)
        .any(|f| pats.iter().any(|p| f.func.contains(p)))
}

/// Classify one stack (leaf first) by its leaf system call stub.
pub fn classify(frames: &[Frame]) -> Leaf {
    let Some(leaf) = frames.first() else {
        return Leaf::Cpu;
    };
    if leaf.module != "libsystem_kernel.dylib" {
        return Leaf::Cpu;
    }
    let n = leaf.func.trim_start_matches('_');
    let n = n
        .strip_suffix("_nocancel")
        .unwrap_or(n)
        .trim_start_matches('_');
    let s = |x: &str| x.to_string();
    match n {
        "psynch_mutexwait" => Leaf::Lock(lock_name(frames).unwrap_or_else(|| s("a mutex"))),
        x if x.starts_with("psynch_rw_") => Leaf::Lock(s("a read-write lock")),
        "ulock_wait" | "ulock_wait2" => match lock_name(frames) {
            Some(l) => Leaf::Lock(l),
            None if any(frames, &["dispatch_once"]) => Leaf::Park(s("a dispatch_once")),
            None if any(frames, &["semaphore", "sema4"]) => Leaf::Park(s("a semaphore")),
            None if any(frames, &["dispatch_group"]) => Leaf::Park(s("a dispatch group")),
            None => Leaf::Park(s("a condition")),
        },
        "psynch_cvwait" => match lock_name(frames) {
            // The GIL is a condition variable underneath.
            Some(l) if l.contains("Python") => Leaf::Lock(l),
            _ => Leaf::Park(s("a condition variable")),
        },
        "semaphore_wait_trap" | "semaphore_timedwait_trap" => Leaf::Park(s("a semaphore")),
        "semwait_signal" => {
            if any(frames, &["nanosleep", "usleep", "sleep", "thread::sleep"]) {
                Leaf::Sleep
            } else {
                Leaf::Park(s("a condition variable"))
            }
        }
        "wait4" | "waitid" => Leaf::Child,
        "read" | "pread" | "readv" | "preadv" => Leaf::Read {
            sys: n.to_string(),
            sock: false,
        },
        "recvfrom" | "recvmsg" | "recvmsg_x" => Leaf::Read {
            sys: n.to_string(),
            sock: true,
        },
        "write" | "pwrite" | "writev" | "pwritev" => Leaf::Write {
            sys: n.to_string(),
            sock: false,
        },
        "sendto" | "sendmsg" | "sendmsg_x" => Leaf::Write {
            sys: n.to_string(),
            sock: true,
        },
        "fsync" | "fdatasync" => Leaf::Fsync,
        "fcntl" => Leaf::Fcntl,
        "flock" => Leaf::FileLock,
        "open" | "openat" | "open_dprotected_np" => Leaf::Open,
        "connect" => Leaf::Connect,
        "accept" => Leaf::Accept,
        "select" | "pselect" | "poll" | "kevent" | "kevent64" | "kevent_qos" | "kevent_id" => {
            if any(frames, &["_dispatch_", "CFRunLoop", "RunCurrentEventLoop", "uv_run", "uv__io_poll"]) {
                Leaf::RunLoop
            } else {
                Leaf::Events(n.to_string())
            }
        }
        "mach_msg2_trap" | "mach_msg_trap" | "mach_msg_overwrite_trap" => {
            if any(
                frames,
                &[
                    "xpc_connection_send_message_with_reply_sync",
                    "dispatch_mach_send_with_result_and_wait_for_reply",
                    "_xpc_pipe_",
                ],
            ) {
                Leaf::Reply
            } else {
                // Most Mach waits are a server or message pump receiving.
                Leaf::RunLoop
            }
        }
        "workq_kernreturn" => Leaf::Workq,
        "sigsuspend" | "sigwait" | "pause" => Leaf::Signal,
        "getattrlist" | "getattrlistbulk" | "stat64" | "lstat64" | "fstat64" | "fstatat64"
        | "stat" | "lstat" | "fstat" | "getdirentries64" | "unlink" | "unlinkat" | "rename"
        | "renameat" | "mkdir" | "rmdir" | "close" | "fgetattrlist" | "setattrlist"
        | "truncate" | "ftruncate" | "fchmod" | "chmod" | "access" | "fsgetpath" => {
            Leaf::FileOp(n.to_string())
        }
        // Quick system calls, sampled while running.
        "thread_switch" | "swtch_pri" | "getpid" | "mach_absolute_time" | "gettimeofday"
        | "bsdthread_ctl" | "thread_selfid" | "proc_info" | "sysctl" | "sysctlbyname"
        | "mach_vm_allocate" | "mmap" | "munmap" | "madvise" | "mprotect" => Leaf::Cpu,
        _ => Leaf::Other(leaf.func.clone()),
    }
}

/// On macOS the perf-map switches of Linux do not help: `sample` reads
/// only native symbols.
pub fn runtime_note(rt: crate::lang::Runtime) -> String {
    format!(
        "{} functions are hidden inside the interpreter or JIT: macOS `sample` shows only its native frames",
        rt.name()
    )
}

/// Stacks for code::summarize. `cat` decides each thread-stack's cause.
pub fn to_stacks(
    s: &SampleOut,
    pid: i32,
    comm: &str,
    mut weigh: impl FnMut(u64, &Leaf, u32) -> Option<(crate::answer::Cat, f64)>,
) -> Stacks {
    let mut st = Stacks::new("sample");
    st.comms.insert(pid, comm.to_string());
    for t in &s.threads {
        for (frames, count) in &t.stacks {
            let leaf = classify(frames);
            let Some((cat, ns)) = weigh(t.id, &leaf, *count) else {
                continue;
            };
            let ids: Vec<usize> = frames.iter().map(|f| st.intern(f.clone())).collect();
            st.add(pid, t.id as i32, cat, ids, ns);
        }
    }
    st.compact();
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const FIXTURE: &str = "\
Analysis of sampling lock (pid 11519) every 10 milliseconds
Process:         lock [11519]
Path:            /private/tmp/*/lock
Code Type:       ARM64

Call graph:
    92 Thread_63665   DispatchQueue_1: com.apple.main-thread  (serial)
    + 92 start  (in dyld) + 2360  [0x18d4f90e0]
    +   92 main  (in lock) + 48  [0x100bd3f2c]  lock.c:6
    +     92 _pthread_mutex_firstfit_lock_slow  (in libsystem_pthread.dylib) + 248  [0x18d874af8]
    +       92 _pthread_mutex_firstfit_lock_wait  (in libsystem_pthread.dylib) + 84  [0x18d877168]
    +         92 __psynch_mutexwait  (in libsystem_kernel.dylib) + 8  [0x18d83c524]
    92 Thread_63679: holder
      92 thread_start  (in libsystem_pthread.dylib) + 8  [0x18d874e3c]
        92 _pthread_start  (in libsystem_pthread.dylib) + 136  [0x18d87a034]
          92 holder  (in lock) + 40  [0x100bd3f64]  lock.c:4
            92 sleep  (in libsystem_c.dylib) + 52  [0x18d7250bc]
              92 nanosleep  (in libsystem_c.dylib) + 220  [0x18d71c2f8]
                92 __semwait_signal  (in libsystem_kernel.dylib) + 8  [0x18d83ce68]

Total number in stack (recursive counted multiple, when >=5):

Binary Images:
       0x100bd0000 -        0x100bd3fff +lock (0) <3A89B5E8-B277-3B66-BFEA-A50CF0AF1458> /tmp/*/lock
       0x18d838000 -        0x18d872fff  libsystem_kernel.dylib (10002.61.3) <CA94FC21-BC40-3B43-B65D-B87ECE9E1D48> /usr/lib/system/libsystem_kernel.dylib
";

    const BRANCHY: &str = "\
Analysis of sampling cpu (pid 1) every 10 milliseconds
Call graph:
    100 Thread_7   DispatchQueue_1: com.apple.main-thread  (serial)
      100 start  (in dyld) + 2360  [0x18d4f90e0]
        100 main  (in cpu) + 12  [0x102d8ff9c]  /src/cpu.c:6
          100 handle_request  (in cpu) + 20  [0x102d8ff78]  /src/cpu.c:5
            70 inner_math  (in cpu) + 40,56,...  [0x102d8ff20,0x102d8ff30,...]  /src/cpu.c:3
            25 parse_rows  (in cpu) + 60  [0x102d8ff50]  /src/cpu.c:4
            ! 25 _platform_memmove  (in libsystem_platform.dylib) + 96  [0x18d8a7f00]
            5 ???  (in <unknown binary>)  [0x1234]

Total number in stack (recursive counted multiple, when >=5):
";

    #[test]
    fn parses_threads_and_stacks() {
        let s = parse(FIXTURE);
        assert_eq!(s.interval_ms, 10.0);
        assert_eq!(s.threads.len(), 2);
        let main = &s.threads[0];
        assert_eq!(main.id, 63665);
        assert_eq!(main.name, "com.apple.main-thread");
        assert_eq!(main.total, 92);
        assert_eq!(main.stacks.len(), 1);
        let (st, n) = &main.stacks[0];
        assert_eq!(*n, 92);
        assert_eq!(st[0].func, "__psynch_mutexwait");
        assert_eq!(st[0].module, "libsystem_kernel.dylib");
        assert_eq!(st.len(), 5);
        assert_eq!(st[3].func, "main");
        assert_eq!(st[3].module, "lock");
        assert_eq!(st[3].file.as_deref(), Some("lock.c"));
        assert_eq!(st[3].line, Some(6));
        assert_eq!(st[4].func, "start");
        assert_eq!(s.threads[1].name, "holder");
        assert_eq!(s.images.len(), 2);
        assert!(s.images[1].ends_with("libsystem_kernel.dylib"));
    }

    #[test]
    fn self_counts_on_branches() {
        let s = parse(BRANCHY);
        let t = &s.threads[0];
        let total: u32 = t.stacks.iter().map(|x| x.1).sum();
        assert_eq!(total, 100);
        let inner = t.stacks.iter().find(|x| x.0[0].func == "inner_math").unwrap();
        assert_eq!(inner.1, 70);
        assert_eq!(inner.0[1].func, "handle_request");
        let mm = t.stacks.iter().find(|x| x.0[0].func == "_platform_memmove").unwrap();
        assert_eq!(mm.1, 25);
        assert_eq!(mm.0[1].func, "parse_rows");
        assert!(!t.stacks.iter().any(|x| x.0[0].func == "parse_rows"));
        let unk = t.stacks.iter().find(|x| x.0[0].func == "[unknown]").unwrap();
        assert_eq!(unk.1, 5);
        assert_eq!(inner.0[0].file.as_deref(), Some("/src/cpu.c"));
    }

    fn st(names: &[(&str, &str)]) -> Vec<Frame> {
        names
            .iter()
            .map(|(f, m)| Frame {
                func: f.to_string(),
                module: m.to_string(),
                ..Frame::default()
            })
            .collect()
    }
    const K: &str = "libsystem_kernel.dylib";

    #[test]
    fn leaves() {
        let s = parse(FIXTURE);
        assert_eq!(classify(&s.threads[0].stacks[0].0), Leaf::Lock("a mutex".into()));
        assert_eq!(classify(&s.threads[1].stacks[0].0), Leaf::Sleep);
        assert_eq!(classify(&st(&[("inner_math", "cpu")])), Leaf::Cpu);
        assert_eq!(
            classify(&st(&[("read", K), ("read_rows", "reader")])),
            Leaf::Read { sys: "read".into(), sock: false }
        );
        assert_eq!(
            classify(&st(&[("__recvfrom", K), ("sock_recv", "_socket.so")])),
            Leaf::Read { sys: "recvfrom".into(), sock: true }
        );
        assert_eq!(classify(&st(&[("__wait4", K), ("waitpid", "libsystem_c.dylib")])), Leaf::Child);
        assert_eq!(
            classify(&st(&[("__psynch_cvwait", K), ("_pthread_cond_wait", "libsystem_pthread.dylib"), ("take_gil", "Python")])),
            Leaf::Lock("the Python GIL".into())
        );
        assert_eq!(
            classify(&st(&[("__psynch_cvwait", K), ("_pthread_cond_wait", "libsystem_pthread.dylib")])),
            Leaf::Park("a condition variable".into())
        );
        assert_eq!(
            classify(&st(&[("mach_msg2_trap", K), ("mach_msg2_internal", K), ("__CFRunLoopServiceMachPort", "CoreFoundation")])),
            Leaf::RunLoop
        );
        assert_eq!(
            classify(&st(&[("mach_msg2_trap", K), ("xpc_connection_send_message_with_reply_sync", "libxpc.dylib")])),
            Leaf::Reply
        );
        assert_eq!(
            classify(&st(&[("mach_msg2_trap", K), ("mach_msg2_internal", K), ("ChromeMain", "Google Chrome Framework")])),
            Leaf::RunLoop
        );
        assert_eq!(classify(&st(&[("__workq_kernreturn", K)])), Leaf::Workq);
        assert_eq!(classify(&st(&[("fsync", K)])), Leaf::Fsync);
        assert_eq!(
            classify(&st(&[("__ulock_wait", K), ("_os_unfair_lock_lock_slow", "libsystem_platform.dylib")])),
            Leaf::Lock("an os_unfair_lock".into())
        );
        assert_eq!(classify(&st(&[("kevent", K), ("main", "server")])), Leaf::Events("kevent".into()));
        assert_eq!(
            classify(&st(&[("kevent_id", K), ("_dispatch_kq_poll", "libdispatch.dylib")])),
            Leaf::RunLoop
        );
    }
}
