//! Watching a whole process tree on Linux, for `wits run`: every thread of
//! every descendant, from the moment the command starts until it exits.
//!
//! Time on CPU and queued for a CPU comes from schedstat and is exact; the
//! rest of each thread's time goes to what it was blocked on at each tick.
//! Waiting for a child, or on a pipe whose other end is in the tree, is
//! internal: the other process's own time is counted instead.

use crate::answer::{Cat, Tree, TreeProc};
use crate::ebpf;
use crate::proc;
use crate::prof;
use crate::sample::{classify, fd_info, FdKind, Wait};
use crate::stackscan::{sp_pc, Scanner};
use crate::symbolize::{parse_maps, Mapping, Symbolizer};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Th {
    sched: Option<(u64, u64)>,
    jiffies: u64,
    last: Option<Instant>,
    last_cap: u32,
    ncap: u32,
    /// Index into `captures` of this thread's latest stack per cause.
    cap_of: HashMap<Cat, usize>,
}

struct Pr {
    name: String,
    threads: HashMap<i32, Th>,
    cpu_ns: f64,
    by_cat: HashMap<Cat, f64>,
    mem: Option<File>,
    maps: Vec<Mapping>,
    /// fd -> pipe inode, refreshed now and then.
    pipes: HashMap<i32, u64>,
    seen_tick: u32,
}

struct Capture {
    pid: i32,
    tid: i32,
    cat: Cat,
    stack: Vec<u64>,
    ns: f64,
    /// The system call the thread was in.
    sys: &'static str,
}

fn pipe_ino(link: &str) -> Option<u64> {
    link.strip_prefix("pipe:[")?.trim_end_matches(']').parse().ok()
}

fn load_pipes(pid: i32) -> HashMap<i32, u64> {
    let mut m = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for e in rd.flatten().take(4096) {
            let Some(fd) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            if let Ok(l) = std::fs::read_link(e.path()) {
                if let Some(i) = pipe_ino(&l.to_string_lossy()) {
                    m.insert(fd, i);
                }
            }
        }
    }
    m
}

/// What a blocked thread's wait counts as, within a tree.
fn cat_of(w: &Wait, pid: i32, procs: &HashMap<i32, Pr>, d: bool) -> Cat {
    let shared_pipe = |fd: i32| -> bool {
        let Some(ino) = procs.get(&pid).and_then(|p| p.pipes.get(&fd)) else {
            return false;
        };
        procs
            .iter()
            .any(|(p, pr)| *p != pid && pr.pipes.values().any(|i| i == ino))
    };
    match w {
        Wait::Disk { .. } | Wait::Fault => Cat::Disk,
        Wait::Mem { .. } => Cat::Memory,
        Wait::Futex { lockish: true, .. } | Wait::FileLock { .. } | Wait::KLock { .. } => Cat::Lock,
        Wait::Io { fd, .. } => match fd_info(pid, *fd).map(|i| i.kind) {
            Some(FdKind::Pipe | FdKind::Fifo) if shared_pipe(*fd) => Cat::Idle,
            Some(FdKind::Pipe | FdKind::Fifo | FdKind::Socket(_)) => Cat::Peer,
            Some(FdKind::File | FdKind::BlockDev) => Cat::Disk,
            _ => Cat::Idle,
        },
        Wait::Connect { .. } | Wait::Open { .. } | Wait::Ipc { .. } => Cat::Peer,
        Wait::Blind { .. } => Cat::Peer,
        Wait::Unknown { d: true, .. } => Cat::Disk,
        _ if d => Cat::Disk,
        _ => Cat::Idle,
    }
}

fn children_rusage_ns() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1e9 + t.tv_usec as f64 * 1e3;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Watch `root` and its descendants. `open` lets the command start (called
/// once watching is ready); `done` reaps and says when the root exited.
pub fn watch_tree_gated(
    root: i32,
    stacks: bool,
    open: &mut dyn FnMut(),
    done: &mut dyn FnMut() -> bool,
) -> Tree {
    let me = std::process::id() as i32;
    let rusage0 = children_rusage_ns();
    let session = stacks.then(|| ebpf::Session::start(&[root], true, crate::explain::FREQ));
    let mut sym = Symbolizer::new();
    let mut procs: HashMap<i32, Pr> = HashMap::new();
    let mut captures: Vec<Capture> = Vec::new();
    let mut rq_total = 0.0;
    let tck = proc::clk_tck() as f64;
    open();
    let start = Instant::now();
    let mut tick: u32 = 0;
    let mut gone: HashSet<i32> = HashSet::new();
    let last_live: Vec<i32>;
    loop {
        let finished = done();
        // Discover: everything below us is the command's (we are its
        // subreaper), except ourselves.
        let mut live: Vec<i32> = Vec::new();
        let mut stack = vec![me];
        let mut seen = HashSet::new();
        while let Some(p) = stack.pop() {
            for c in proc::children(p) {
                if seen.insert(c) {
                    live.push(c);
                    stack.push(c);
                }
            }
        }
        let nthreads: usize = procs.values().map(|p| p.threads.len()).sum();
        let step = match nthreads {
            0..=64 => 10,
            65..=512 => 20,
            _ => 50,
        };
        for &pid in &live {
            if gone.contains(&pid) {
                continue;
            }
            let pr = procs.entry(pid).or_insert_with(|| {
                sym.snapshot(pid);
                Pr {
                    name: proc::comm(pid).unwrap_or_default(),
                    threads: HashMap::new(),
                    cpu_ns: 0.0,
                    by_cat: HashMap::new(),
                    mem: stacks.then(|| File::open(format!("/proc/{pid}/mem")).ok()).flatten(),
                    maps: Vec::new(),
                    pipes: load_pipes(pid),
                    seen_tick: tick,
                }
            });
            // The name changes at exec; follow it for the first moments.
            if tick.saturating_sub(pr.seen_tick) < 20 || tick.is_multiple_of(50) {
                if let Some(c) = proc::comm(pid) {
                    if c != pr.name {
                        pr.name = c;
                        pr.mem = stacks.then(|| File::open(format!("/proc/{pid}/mem")).ok()).flatten();
                        pr.maps.clear();
                        sym.snapshot(pid);
                    }
                }
            }
            if tick.is_multiple_of(25) {
                pr.pipes = load_pipes(pid);
                sym.snapshot(pid);
            }
        }
        let now = Instant::now();
        let live_set: HashSet<i32> = live.iter().copied().collect();
        // Sample every thread.
        let pids: Vec<i32> = procs.keys().copied().filter(|p| live_set.contains(p)).collect();
        for pid in pids {
            let tids = proc::tids(pid);
            if tids.is_empty() {
                continue;
            }
            for tid in tids {
                let Some(s) = proc::task_stat(pid, tid) else { continue };
                if s.state == 'Z' || s.state == 'X' {
                    continue;
                }
                let sched = proc::task_schedstat(pid, tid);
                let jiff = s.utime + s.stime;
                let blocked = if s.state == 'R' {
                    None
                } else {
                    let base = format!("/proc/{pid}/task/{tid}");
                    let sc_text = proc::slurp(&format!("{base}/syscall"));
                    let sc = sc_text.as_deref().and_then(proc::parse_syscall);
                    let wchan = proc::slurp(&format!("{base}/wchan"))
                        .map(|w| w.trim().to_string())
                        .filter(|w| !w.is_empty() && w != "0");
                    classify(s.state, sc, wchan.as_deref()).map(|w| (w, sc_text))
                };
                let cat = blocked
                    .as_ref()
                    .map(|(w, _)| cat_of(w, pid, &procs, s.state == 'D'));
                let pr = procs.get_mut(&pid).unwrap();
                let th = pr.threads.entry(tid).or_default();
                let wall = th.last.map(|t| (now - t).as_nanos() as f64).unwrap_or(0.0);
                let (dcpu, drq) = match (th.sched, sched) {
                    (Some(a), Some(b)) => (b.0.saturating_sub(a.0) as f64, b.1.saturating_sub(a.1) as f64),
                    (None, Some(b)) if th.last.is_none() => (b.0 as f64, b.1 as f64),
                    _ => (jiff.saturating_sub(th.jiffies) as f64 / tck * 1e9, 0.0),
                };
                th.sched = sched.or(th.sched);
                th.jiffies = jiff;
                th.last = Some(now);
                let rest = (wall - dcpu - drq).max(0.0);
                pr.cpu_ns += dcpu;
                rq_total += drq;
                *pr.by_cat.entry(Cat::Cpu).or_default() += dcpu;
                *pr.by_cat.entry(Cat::CpuWait).or_default() += drq;
                let c = cat.unwrap_or(Cat::Idle);
                *pr.by_cat.entry(c).or_default() += rest;
                if let Some(&i) = th.cap_of.get(&c) {
                    captures[i].ns += rest;
                }
                // Capture a fresh stack for a blocked thread now and then.
                if let (Some((_, Some(text))), Some(c)) = (&blocked, cat) {
                    if c != Cat::Idle
                        && th.ncap < 50
                        && (th.ncap == 0 || tick >= th.last_cap + 4)
                    {
                        if let (Some(mem), Some((sp, pc))) = (pr.mem.as_ref(), sp_pc(text)) {
                            if pr.maps.is_empty() {
                                pr.maps = proc::slurp(&format!("/proc/{pid}/maps"))
                                    .map(|s| parse_maps(&s))
                                    .unwrap_or_default();
                            }
                            let st = Scanner::new(mem, &pr.maps).stack(sp, pc);
                            th.last_cap = tick;
                            th.ncap += 1;
                            th.cap_of.insert(c, captures.len());
                            let sys = match proc::parse_syscall(text) {
                                Some(proc::Syscall::In { nr, .. }) => crate::sysno::lookup(nr).1,
                                _ => "",
                            };
                            captures.push(Capture {
                                pid,
                                tid,
                                cat: c,
                                stack: st,
                                ns: 0.0,
                                sys,
                            });
                        }
                    }
                }
            }
        }
        for p in procs.keys() {
            if !live_set.contains(p) {
                gone.insert(*p);
            }
        }
        if finished {
            last_live = live;
            break;
        }
        tick += 1;
        let next = start + Duration::from_millis(step) * tick;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }

    let mut tree = Tree::default();
    // CPU of processes too short-lived to be seen: the kernel adds every
    // reaped child's CPU time to ours.
    let measured: f64 = procs.values().map(|p| p.cpu_ns).sum();
    let reaped = children_rusage_ns() - rusage0;
    let unseen = (reaped - measured).max(0.0);
    for (pid, pr) in &procs {
        for (c, v) in &pr.by_cat {
            *tree.by_cat.entry(*c).or_default() += v;
        }
        tree.procs.push(TreeProc {
            pid: *pid,
            name: pr.name.clone(),
            cpu_ns: pr.cpu_ns,
            by_cat: pr.by_cat.clone(),
        });
    }
    if unseen > 0.02 * reaped && unseen > 5e6 {
        *tree.by_cat.entry(Cat::Cpu).or_default() += unseen;
        tree.procs.push(TreeProc {
            pid: 0,
            name: "(short-lived processes)".into(),
            cpu_ns: unseen,
            by_cat: [(Cat::Cpu, unseen)].into_iter().collect(),
        });
    }
    if stacks {
        let raw = match &session {
            Some(Ok(s)) => Some(s.collect()),
            _ => None,
        };
        let why = match &session {
            Some(Err(e)) => Some(e.clone()),
            _ => None,
        };
        drop(session);
        let mut order: Vec<(&i32, &Pr)> = procs.iter().collect();
        order.sort_by(|a, b| {
            let act = |p: &Pr| -> f64 { p.by_cat.iter().filter(|c| *c.0 != Cat::Idle).map(|c| c.1).sum() };
            act(b.1).total_cmp(&act(a.1))
        });
        let mut pids: Vec<(i32, String)> = vec![(root, procs.get(&root).map(|p| p.name.clone()).unwrap_or_default())];
        pids.extend(order.iter().filter(|x| *x.0 != root).map(|(p, pr)| (**p, pr.name.clone())));
        let scans = captures
            .iter()
            .filter(|c| c.ns > 0.0)
            .map(|c| prof::Scan {
                pid: c.pid,
                tid: c.tid,
                cat: c.cat,
                ns: c.ns,
                stack: &c.stack,
                sys: c.sys,
            })
            .collect();
        let st = prof::stacks(
            prof::Inputs {
                pids,
                scans,
                raw: raw.as_deref(),
                cpu_ns: tree.by_cat.get(&Cat::Cpu).copied().unwrap_or(0.0),
                rq_ns: rq_total,
                why_no_cpu: why,
                measured: Some(
                    procs
                        .iter()
                        .flat_map(|(p, pr)| pr.by_cat.iter().map(move |(c, v)| ((*p, *c), *v)))
                        .collect(),
                ),
                in_run: true,
            },
            &mut sym,
        );
        tree.stacks = (!st.is_empty()).then_some(st);
    }
    let left = last_live.iter().filter(|p| **p != root).count();
    if left > 0 {
        tree.notes.push(format!(
            "{left} process{} it started {} still running",
            if left == 1 { "" } else { "es" },
            if left == 1 { "is" } else { "are" }
        ));
    }
    tree
}
