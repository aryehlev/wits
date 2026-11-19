//! Assemble the stacks wits collected on Linux (eBPF, perf, stack scanning)
//! into one `Stacks`, named and weighted by time, with notes on what kept
//! them from being complete.

use crate::answer::Cat;
use crate::code::{is_runtime, python_event_wait, Frame, Stacks};
use crate::ebpf::RawStack;
use crate::lang;
use crate::symbolize::Symbolizer;
use std::collections::{HashMap, HashSet};

const MEM: &[&str] = &["mem_cgroup", "reclaim", "swap", "throttle_direct", "compact", "oom_"];
const DISK: &[&str] = &[
    "io_schedule", "folio_wait", "wait_on_page", "bit_wait_io", "blk_", "bio_", "jbd2",
    "ext4", "xfs_", "btrfs", "wbt_", "rq_qos", "balance_dirty", "writeback", "wait_on_buffer",
    "nfs", "fuse", "request_wait_answer", "submit_bio", "filemap_fault",
];
const KLOCK: &[&str] = &["rwsem", "mutex_lock", "__mutex", "down_read", "down_write", "percpu_rwsem"];
const FLOCK: &[&str] = &["flock", "locks_lock", "posix_lock", "fcntl_setlk"];
const PEER: &[&str] = &[
    "pipe_read", "pipe_write", "pipe_wait", "unix_stream", "unix_dgram", "tcp_", "sk_wait",
    "inet_", "sock_", "skb_", "wait_woken", "do_wait", "kernel_wait", "fifo_open", "wait_for_partner",
];
/// User functions that mean a thread is queued on a lock.
pub const LOCK_FUNCS: &[&str] = &[
    "pthread_mutex_lock",
    "pthread_mutex_timedlock",
    "__pthread_mutex_lock",
    "__lll_lock_wait",
    "pthread_rwlock_rdlock",
    "pthread_rwlock_wrlock",
    "lock_contended",
    "Mutex<",
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
    "os_unfair_lock_lock",
    "_os_unfair_lock_lock_slow",
    "_pthread_mutex_firstfit_lock_slow",
];

pub fn is_lock_frame(f: &Frame) -> bool {
    LOCK_FUNCS.iter().any(|p| f.func.contains(p))
}

/// Whether a user stack (leaf first) is queued on a lock.
fn lock_wait(u: &[Frame]) -> bool {
    match u.iter().take(8).find(|f| is_lock_frame(f)) {
        // Python waits for events through its locks; the GIL is real.
        Some(f) if f.func.contains("PyThread_acquire_lock") => !python_event_wait(u),
        Some(_) => true,
        None => false,
    }
}

/// What an off-CPU span was, from the task state it left the CPU in and
/// the kernel and user functions it was in.
pub fn classify_off(state: u32, k: &[Frame], u: &[Frame]) -> Cat {
    let s = state & 0xff;
    if s == 0 {
        return Cat::CpuWait;
    }
    // stopped, traced, dead
    if s & (0x04 | 0x08 | 0x10 | 0x20) != 0 {
        return Cat::Idle;
    }
    if k.is_empty() {
        return classify_by_user(s, u);
    }
    let has = |pats: &[&str]| k.iter().any(|f| pats.iter().any(|p| f.func.contains(p)));
    if has(MEM) {
        return Cat::Memory;
    }
    if has(FLOCK) {
        return Cat::Lock;
    }
    if has(&["futex"]) {
        return if lock_wait(u) {
            Cat::Lock
        } else {
            Cat::Idle
        };
    }
    let d = s & 0x02 != 0 && s & 0x80 == 0;
    if d && has(KLOCK) {
        return Cat::Lock;
    }
    if d || has(DISK) {
        return Cat::Disk;
    }
    if has(PEER) {
        return Cat::Peer;
    }
    Cat::Idle
}

/// Without kernel frames (some kernels unwind only a frame or two from the
/// scheduler tracepoint), the system call wrapper on top of the user stack
/// says what the thread waits for.
fn classify_by_user(s: u32, u: &[Frame]) -> Cat {
    if lock_wait(u) {
        return Cat::Lock;
    }
    let d = s & 0x02 != 0 && s & 0x80 == 0;
    let leaf = u
        .iter()
        .find(|f| !f.func.starts_with('[') && f.func != "[unknown]")
        .map(|f| f.func.trim_start_matches('_').trim_start_matches("libc_"))
        .unwrap_or("");
    const DISK: &[&str] = &["fsync", "fdatasync", "sync_file_range", "msync", "syncfs", "sync"];
    const LOCK: &[&str] = &["flock", "lockf"];
    const PEER: &[&str] = &[
        "read", "pread64", "readv", "recv", "recvfrom", "recvmsg", "write", "pwrite64", "writev",
        "send", "sendto", "sendmsg", "connect", "splice", "sendfile",
    ];
    if DISK.contains(&leaf) || d {
        Cat::Disk
    } else if LOCK.contains(&leaf) {
        Cat::Lock
    } else if PEER.contains(&leaf) {
        Cat::Peer
    } else {
        // sleeps, polls, condition variables, waits for children
        Cat::Idle
    }
}

/// A stack captured from a blocked thread by the sampler, already
/// classified by wits, with the time it stands for.
pub struct Scan<'a> {
    pub pid: i32,
    pub tid: i32,
    pub cat: Cat,
    pub ns: f64,
    pub stack: &'a [u64],
    /// The system call the thread was in, or "" when that is not known.
    pub sys: &'static str,
}

pub struct Inputs<'a> {
    /// The processes of interest, most important first: their names label
    /// folded stacks, and their runtimes are checked for hidden functions.
    pub pids: Vec<(i32, String)>,
    pub scans: Vec<Scan<'a>>,
    pub raw: Option<&'a [RawStack]>,
    /// Measured on-CPU and runqueue time, to weight sampled stacks.
    pub cpu_ns: f64,
    pub rq_ns: f64,
    /// Why there are no on-CPU stacks, when there are none.
    pub why_no_cpu: Option<String>,
    /// Measured time per (process, cause). An eBPF off-CPU stack whose
    /// cause the measurements never saw for that process (a pipe wait that
    /// is internal to a process tree, say) is not shown as that cause.
    pub measured: Option<HashMap<(i32, Cat), f64>>,
    /// Inside `wits run`, which already asks runtimes for their symbols.
    pub in_run: bool,
}

const ROOTS: &[&str] = &[
    "main",
    "_start",
    "start_thread",
    "thread_start",
    "clone",
    "clone3",
    "__libc_start_main",
    "__libc_start_call_main",
    "runtime.goexit",
    "runtime.main",
    "runtime.mstart",
    "_dl_start_user",
];

fn complete(frames: &[&Frame]) -> bool {
    frames
        .last()
        .map(|f| ROOTS.iter().any(|r| f.func == *r || f.func.ends_with(&format!("::{r}"))))
        .unwrap_or(false)
        || frames.len() > 3
}

/// Drop the frames of the tracing machinery and the scheduler itself from
/// the top of a kernel stack: they are how the stack was taken, not why.
fn strip_tracing(k: &mut Vec<Frame>) {
    let noise = |f: &Frame| {
        let n = f.func.as_str();
        n.starts_with("perf_")
            || n.starts_with("__perf")
            || n.starts_with("trace_")
            || n.starts_with("__traceiter")
            || n.starts_with("bpf_")
            || n.starts_with("__bpf")
            || n.starts_with("0x")
            || matches!(n, "__schedule" | "schedule" | "preempt_schedule" | "preempt_schedule_common")
    };
    let n = k.iter().take_while(|f| noise(f)).count();
    k.drain(..n);
}

/// The system call a kernel stack is in: "epoll_wait" for one that came in
/// through `__x64_sys_epoll_wait`.
fn syscall_of(k: &[Frame]) -> Option<&str> {
    const ENTRY: &[&str] = &["__x64_sys_", "__arm64_sys_", "__ia32_sys_", "__riscv_sys_", "__s390x_sys_", "__se_sys_", "__do_sys_", "SyS_"];
    // From the root: the way in comes before what it calls.
    k.iter().rev().find_map(|f| ENTRY.iter().find_map(|p| f.func.strip_prefix(p))).filter(|n| !n.is_empty())
}

/// A thread in a system call whose innermost user function is one of the C
/// library's with no name (hidden, in a libc without symbols) is in the
/// library's code for that call: the system call names it. Unnamed code
/// anywhere else stays unknown: a name would make it look like the
/// program's own function.
fn name_leaf(u: &mut [Frame], sys: &str) {
    let Some(f) = u.first_mut() else { return };
    if !sys.is_empty() && f.func == "[unknown]" && crate::code::origin(f).0 == crate::code::Kind::Sys {
        f.func = sys.to_string();
    }
}

pub fn stacks(inp: Inputs, sym: &mut Symbolizer) -> Stacks {
    for (p, _) in &inp.pids {
        sym.snapshot(*p);
    }
    let source = match (inp.raw.is_some(), inp.scans.is_empty()) {
        (true, false) => "ebpf+stack-scan",
        (true, true) => "ebpf",
        (false, _) => "stack-scan",
    };
    let mut st = Stacks::new(source);
    for (p, c) in &inp.pids {
        st.comms.insert(*p, c.clone());
    }
    let scanned: HashSet<i32> = inp.scans.iter().map(|s| s.tid).collect();
    let mut truncated = (0.0, 0.0, String::new());
    let mut note_depth = |st: &Stacks, ids: &[usize], ns: f64| {
        let user: Vec<&Frame> = ids.iter().map(|&i| &st.frames[i]).filter(|f| !f.kernel).collect();
        truncated.1 += ns;
        if !complete(&user) && !user.is_empty() {
            truncated.0 += ns;
            if truncated.2.is_empty() {
                truncated.2 = user.last().map(|f| f.module.clone()).unwrap_or_default();
            }
        }
    };
    for s in &inp.scans {
        let mut frames = sym.user(s.pid, s.stack);
        name_leaf(&mut frames, s.sys);
        let ids: Vec<usize> = frames.into_iter().map(|f| st.intern(f)).collect();
        if s.cat != Cat::Idle {
            note_depth(&st, &ids, s.ns);
        }
        st.add(s.pid, s.tid, s.cat, ids, s.ns);
    }
    if let Some(raw) = inp.raw {
        let mut seen_pids = HashSet::new();
        for r in raw {
            if seen_pids.insert(r.tgid) && !st.comms.contains_key(&r.tgid) {
                sym.snapshot(r.tgid);
                st.comms
                    .insert(r.tgid, crate::proc::comm(r.tgid).unwrap_or_default());
            }
            if r.ns == 0 {
                continue;
            }
            let mut u = sym.user(r.tgid, &r.user);
            // The link register names the caller of a leaf function, unless
            // it is already in the stack or points back into the leaf.
            if r.lr != 0 && r.lr < 1 << 48 && !r.user.is_empty() && r.user.get(1) != Some(&r.lr) {
                let leaf = sym.user(r.tgid, &r.user[..1]);
                let caller = sym.user(r.tgid, &[0, r.lr]);
                let caller = &caller[1..];
                let (Some(l), Some(c)) = (leaf.last(), caller.last()) else {
                    unreachable!()
                };
                if c.func != l.func && c.func != "[unknown]" && u.get(leaf.len()) != Some(c) {
                    let at = leaf.len();
                    for (i, f) in caller.iter().enumerate() {
                        u.insert(at + i, f.clone());
                    }
                }
            }
            let mut k = sym.kernel(&r.kernel);
            strip_tracing(&mut k);
            let cat = match r.off_state {
                None => Cat::Cpu,
                Some(state) => classify_off(state, &k, &u),
            };
            // Blocked time of a thread the sampler captured is already in.
            if !matches!(cat, Cat::Cpu | Cat::CpuWait) && scanned.contains(&r.tid) {
                continue;
            }
            let cat = match &inp.measured {
                Some(m) if !matches!(cat, Cat::Cpu | Cat::CpuWait | Cat::Idle)
                    && m.get(&(r.tgid, cat)).copied().unwrap_or(0.0) <= 0.0 =>
                {
                    Cat::Idle
                }
                _ => cat,
            };
            // After the cause is decided: a name must not change it.
            if let Some(sys) = syscall_of(&k) {
                name_leaf(&mut u, sys);
            }
            let ids: Vec<usize> = k.into_iter().chain(u).map(|f| st.intern(f)).collect();
            if cat != Cat::Idle {
                note_depth(&st, &ids, r.ns as f64);
            }
            st.add(r.tgid, r.tid, cat, ids, r.ns as f64);
        }
    }
    st.compact();
    if inp.raw.is_some() {
        st.scale_to(Cat::Cpu, inp.cpu_ns);
        st.scale_to(Cat::CpuWait, inp.rq_ns);
        // The measurements say how long each process waited for each cause
        // and the stacks say where: where the two tell the cause apart
        // differently (the GIL is a condition variable to the kernel), the
        // stacks follow the totals the report shows.
        if let Some(m) = &inp.measured {
            let mut have: HashMap<(i32, Cat), f64> = HashMap::new();
            for s in st.samples.iter().filter(|s| !matches!(s.cat, Cat::Cpu | Cat::CpuWait | Cat::Idle)) {
                *have.entry((s.pid, s.cat)).or_default() += s.ns;
            }
            for s in st.samples.iter_mut() {
                let key = (s.pid, s.cat);
                if let (Some(&h), Some(&want)) = (have.get(&key), m.get(&key)) {
                    if h > 0.0 && want > 0.0 {
                        s.ns *= want / h;
                    }
                }
            }
        }
    }
    if let Some(why) = &inp.why_no_cpu {
        if inp.cpu_ns > 0.0 {
            st.notes.push(format!("no stacks for time on CPU: {why}"));
        }
    }
    if truncated.1 > 0.0 && truncated.0 / truncated.1 > 0.4 {
        let m = if truncated.2.is_empty() {
            "the program".to_string()
        } else {
            truncated.2.clone()
        };
        st.notes.push(format!(
            "stacks stop early in {m}: it was built without frame pointers (C/C++: -fno-omit-frame-pointer; Rust: RUSTFLAGS=\"-C force-frame-pointers=yes\")"
        ));
    }
    // A runtime whose own functions we cannot see.
    for (pid, _) in inp.pids.iter().take(3) {
        let pid = *pid;
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let maps = sym.mappings(pid);
        if let Some(rt) = lang::detect(&exe, maps.iter().map(|m| m.path.as_str())) {
            let app_frames = st.samples.iter().filter(|s| s.pid == pid).any(|s| {
                s.frames.iter().any(|&i| {
                    let f = &st.frames[i];
                    f.module.starts_with('[') && !is_runtime(f) && !f.kernel
                })
            });
            let h = lang::hint(rt, pid, inp.in_run);
            if !sym.has_jit(pid) && !app_frames && !st.notes.contains(&h) {
                st.notes.push(h);
            }
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(names: &[&str]) -> Vec<Frame> {
        names
            .iter()
            .map(|n| Frame {
                func: n.to_string(),
                module: "[kernel]".into(),
                kernel: true,
                ..Frame::default()
            })
            .collect()
    }
    fn u(names: &[&str]) -> Vec<Frame> {
        names
            .iter()
            .map(|n| Frame {
                func: n.to_string(),
                module: "app".into(),
                ..Frame::default()
            })
            .collect()
    }

    #[test]
    fn a_nameless_leaf_takes_the_system_calls_name() {
        let entry = k(&["__schedule", "schedule_hrtimeout_range", "ep_poll", "do_epoll_wait", "__x64_sys_epoll_wait", "do_syscall_64", "entry_SYSCALL_64_after_hwframe"]);
        assert_eq!(syscall_of(&entry), Some("epoll_wait"));
        assert_eq!(syscall_of(&k(&["__schedule", "futex_wait", "do_futex", "__arm64_sys_futex", "invoke_syscall"])), Some("futex"));
        // A page fault, or a stack that does not reach the way in.
        assert_eq!(syscall_of(&k(&["__schedule", "io_schedule", "filemap_fault", "handle_mm_fault", "asm_exc_page_fault"])), None);
        assert_eq!(syscall_of(&[]), None);

        let libc = |func: &str| Frame { func: func.into(), module: "libc.so.6".into(), ..Frame::default() };
        let mut stack = vec![libc("[unknown]"), u(&["mio::sys::unix::selector::Selector::select"]).remove(0)];
        name_leaf(&mut stack, "epoll_wait");
        assert_eq!((stack[0].func.as_str(), stack[0].module.as_str()), ("epoll_wait", "libc.so.6"));
        assert_eq!(crate::code::origin(&stack[0]).0, crate::code::Kind::Sys);
        // A function with a name keeps it; so does an unnamed one that is not the leaf.
        let mut stack = vec![libc("__GI___poll"), libc("[unknown]")];
        name_leaf(&mut stack, "poll");
        assert_eq!((stack[0].func.as_str(), stack[1].func.as_str()), ("__GI___poll", "[unknown]"));
        // No system call known: nothing to name it after.
        let mut stack = vec![libc("[unknown]")];
        name_leaf(&mut stack, "");
        assert_eq!(stack[0].func, "[unknown]");
        // Not the C library's: it stays unknown, and is not taken for the program's.
        let mut stack = vec![Frame { func: "[unknown]".into(), module: "svc".into(), ..Frame::default() }];
        name_leaf(&mut stack, "read");
        assert_eq!(stack[0].func, "[unknown]");
        name_leaf(&mut [], "read");
    }

    #[test]
    fn off_cpu_classes() {
        assert_eq!(classify_off(0x100, &[], &[]), Cat::CpuWait);
        assert_eq!(classify_off(0, &[], &[]), Cat::CpuWait);
        assert_eq!(
            classify_off(1, &k(&["__schedule", "futex_wait_queue", "futex_wait"]), &u(&["__lll_lock_wait", "pthread_mutex_lock"])),
            Cat::Lock
        );
        assert_eq!(
            classify_off(1, &k(&["__schedule", "futex_wait_queue"]), &u(&["pthread_cond_wait"])),
            Cat::Idle
        );
        assert_eq!(classify_off(2, &k(&["__schedule", "io_schedule", "folio_wait_bit_common"]), &[]), Cat::Disk);
        assert_eq!(classify_off(1, &k(&["__schedule", "pipe_read"]), &[]), Cat::Peer);
        assert_eq!(classify_off(1, &k(&["__schedule", "do_nanosleep"]), &[]), Cat::Idle);
        assert_eq!(classify_off(2, &k(&["__schedule", "rwsem_down_read_slowpath"]), &[]), Cat::Lock);
        assert_eq!(classify_off(1, &k(&["__schedule", "mem_cgroup_handle_over_high"]), &[]), Cat::Memory);
        // no kernel frames: the user stack decides
        assert_eq!(classify_off(1, &[], &u(&["__lll_lock_wait", "pthread_mutex_lock", "main"])), Cat::Lock);
        assert_eq!(classify_off(1, &[], &u(&["read", "main"])), Cat::Peer);
        assert_eq!(classify_off(2, &[], &u(&["fsync", "main"])), Cat::Disk);
        assert_eq!(classify_off(1, &[], &u(&["clock_nanosleep", "main"])), Cat::Idle);
        assert_eq!(classify_off(1, &[], &u(&["pthread_cond_wait", "main"])), Cat::Idle);
        // Python: Event.wait goes through a lock, but waits for a signal;
        // the GIL is a lock even though it is a condition variable.
        let py = |n: &str| Frame { func: n.into(), module: "[python]".into(), ..Frame::default() };
        let mut ev = u(&["_PySemaphore_Wait", "_PyParkingLot_Park", "_PyMutex_LockTimed", "lock_PyThread_acquire_lock"]);
        ev.extend([py("Condition.wait"), py("Event.wait"), py("Server.run")]);
        assert_eq!(classify_off(1, &k(&["__schedule", "futex_wait_queue"]), &ev), Cat::Idle);
        assert_eq!(classify_off(1, &[], &ev), Cat::Idle);
        let mut held = u(&["_PySemaphore_Wait", "_PyParkingLot_Park", "_PyMutex_LockTimed", "lock_PyThread_acquire_lock"]);
        held.push(py("Pool.get_conn"));
        assert_eq!(classify_off(1, &k(&["__schedule", "futex_wait_queue"]), &held), Cat::Lock);
        let mut gil = u(&["pthread_cond_timedwait", "take_gil", "PyEval_RestoreThread", "lock_PyThread_acquire_lock"]);
        gil.extend([py("Condition.wait"), py("Event.wait")]);
        assert_eq!(classify_off(1, &k(&["__schedule", "futex_wait_queue"]), &gil), Cat::Lock);
    }
}
