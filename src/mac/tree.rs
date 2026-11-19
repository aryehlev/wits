//! Watching a whole process tree on macOS, for `wits run`.
//!
//! libproc every 10 ms gives each thread's exact CPU time and state, and
//! each process's exact time runnable but not running. What a blocked
//! thread waits on comes from `sample`, run over the lifetime of each
//! longer-lived process of the tree; its leaf counts split the blocked time.
//! Waiting for a child, or on a pipe whose other end is in the tree, is
//! internal: the other process's own time is counted instead.

use super::ffi;
use super::stacks::{self, Leaf, Running, SampleOut};
use crate::answer::{Cat, Tree, TreeProc};
use crate::code::Stacks;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// At most this many `sample` runs at once.
const MAX_SAMPLERS: usize = 8;
/// A process is sampled once it has lived this long (past its exec).
const SAMPLE_AFTER: Duration = Duration::from_millis(60);

#[derive(Default)]
struct Th {
    last: Option<Instant>,
    cpu: Option<u64>,
    cpu_ns: f64,
    rq_ns: f64,
    /// Blocked or sleeping (split later by `sample`'s leaves).
    wait_ns: f64,
    uninter_ns: f64,
    stopped_ns: f64,
    /// Blocked while the process had children in the tree: without a
    /// stack, taken as waiting for them.
    parent_ns: f64,
}

struct Pr {
    name: String,
    first: Instant,
    threads: HashMap<u64, Th>,
    rq: Option<u64>,
    /// (fd, pipe handle, peer handle) refreshed now and then.
    pipes: Vec<(i32, u64, u64)>,
    /// Kinds of fds 0..2 and whether it has connected sockets / files.
    stdin: Option<StdKind>,
    sockets: bool,
    /// A TCP connect not answered yet (SYN_SENT).
    connecting: bool,
    files: bool,
    sampler: Option<Running>,
    out: Option<SampleOut>,
    why_not: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StdKind {
    Pipe,
    Tty,
    File,
    Socket,
    Other,
}

fn snapshot_fds(pid: i32, pr: &mut Pr) {
    let Some(fds) = ffi::fds(pid) else { return };
    pr.pipes.clear();
    pr.sockets = false;
    pr.connecting = false;
    pr.files = false;
    pr.stdin = None;
    for f in fds.iter().take(4096) {
        let kind = match f.kind {
            ffi::FD_PIPE => {
                if let Some(p) = ffi::pipe(pid, f.fd) {
                    pr.pipes.push((f.fd, p.handle, p.peer));
                }
                StdKind::Pipe
            }
            ffi::FD_SOCKET => {
                if let Some(s) = ffi::socket(pid, f.fd) {
                    pr.sockets |= super::fds::connected(&s);
                    pr.connecting |= s.kind == 2 && s.tcp_state == 2;
                }
                StdKind::Socket
            }
            ffi::FD_VNODE => match ffi::vnode(pid, f.fd) {
                Some(v) if v.mode & 0o170000 == 0o100000 => {
                    pr.files = true;
                    StdKind::File
                }
                Some(v) if v.mode & 0o170000 == 0o010000 => StdKind::Pipe,
                Some(v) if v.path.starts_with("/dev/tty") => StdKind::Tty,
                _ => StdKind::Other,
            },
            _ => StdKind::Other,
        };
        if f.fd == 0 {
            pr.stdin = Some(kind);
        }
    }
}

/// What a blocked leaf counts as, within the tree.
fn cat_of(l: &Leaf, pid: i32, procs: &HashMap<i32, Pr>) -> Cat {
    let pr = &procs[&pid];
    // Does another process of the tree hold the other end of our pipe?
    let internal_pipe = || {
        pr.pipes.iter().any(|(_, _, peer)| {
            procs
                .iter()
                .any(|(p, o)| *p != pid && o.pipes.iter().any(|(_, h, _)| h == peer))
        })
    };
    match l {
        Leaf::Cpu => Cat::Cpu,
        Leaf::Lock(_) | Leaf::FileLock => Cat::Lock,
        Leaf::Fsync | Leaf::FileOp(_) => Cat::Disk,
        Leaf::Fcntl => Cat::Disk,
        Leaf::Read { sock: true, .. } | Leaf::Write { sock: true, .. } => Cat::Peer,
        Leaf::Read { .. } | Leaf::Write { .. } => match pr.stdin {
            Some(StdKind::Pipe) if internal_pipe() => Cat::Idle,
            _ if pr.sockets => Cat::Peer,
            Some(StdKind::Pipe) => Cat::Peer,
            Some(StdKind::Tty) => Cat::Idle,
            _ if pr.files => Cat::Disk,
            _ => Cat::Idle,
        },
        Leaf::Connect | Leaf::Open | Leaf::Reply => Cat::Peer,
        // An event loop waiting while one of its connections is being opened.
        Leaf::Events(_) | Leaf::RunLoop if pr.connecting => Cat::Peer,
        Leaf::Events(_) if pr.sockets => Cat::Peer,
        // Waiting for a child of the tree: that child's time is counted.
        Leaf::Child => Cat::Idle,
        _ => Cat::Idle,
    }
}

fn children_rusage_ns() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1e9 + t.tv_usec as f64 * 1e3;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Watch `root` and its descendants until `done` says the root exited.
pub fn watch_tree(root: i32, want_stacks: bool, done: &mut dyn FnMut() -> bool) -> Tree {
    let rusage0 = children_rusage_ns();
    let launch0 = stacks::LAUNCHER_NS.load(std::sync::atomic::Ordering::Relaxed);
    let mut procs: HashMap<i32, Pr> = HashMap::new();
    let start = Instant::now();
    let mut tick: u32 = 0;
    let mut gone: HashSet<i32> = HashSet::new();
    let last_live: Vec<i32>;
    loop {
        let finished = done();
        // Descendants, plus anything seen before and still alive (orphans
        // are reparented to launchd: macOS has no subreaper).
        let mut live: Vec<i32> = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        if ffi::short(root).map(|s| s.status != 5).unwrap_or(false) {
            seen.insert(root);
            live.push(root);
        }
        for p in procs.keys() {
            if !gone.contains(p) && seen.insert(*p) {
                stack.push(*p);
                live.push(*p);
            }
        }
        while let Some(p) = stack.pop() {
            for c in ffi::children(p) {
                if seen.insert(c) {
                    live.push(c);
                    stack.push(c);
                }
            }
        }
        live.retain(|p| match ffi::short(*p) {
            Some(s) => s.status != 5,
            None => false,
        });
        let parents: HashSet<i32> = live
            .iter()
            .filter_map(|p| ffi::short(*p).map(|s| s.ppid))
            .collect();
        let nthreads: usize = procs.values().map(|p| p.threads.len()).sum();
        let step = match nthreads {
            0..=64 => 10,
            65..=512 => 20,
            _ => 50,
        };
        let now = Instant::now();
        for &pid in &live {
            let has_kids = parents.contains(&pid);
            let pr = procs.entry(pid).or_insert_with(|| Pr {
                name: ffi::short(pid).map(|s| s.comm).unwrap_or_default(),
                first: now,
                threads: HashMap::new(),
                rq: None,
                pipes: Vec::new(),
                stdin: None,
                sockets: false,
                connecting: false,
                files: false,
                sampler: None,
                out: None,
                why_not: None,
            });
            let age = now - pr.first;
            if age < Duration::from_millis(200) || tick.is_multiple_of(50) {
                if let Some(s) = ffi::short(pid) {
                    pr.name = s.comm;
                }
            }
            if age < Duration::from_millis(100) || tick.is_multiple_of(25) {
                snapshot_fds(pid, pr);
            }
            // rusage: the process's exact runnable time.
            let rq = ffi::usage(pid).map(|u| u.runnable_ns);
            let drq = match (pr.rq, rq) {
                (Some(a), Some(b)) => b.saturating_sub(a) as f64,
                (None, Some(b)) => b as f64,
                _ => 0.0,
            };
            pr.rq = rq.or(pr.rq);
            // Threads: CPU delta and state.
            let mut rows: Vec<(u64, f64, f64, i32)> = Vec::new();
            for id in ffi::thread_ids(pid) {
                let Some(t) = ffi::thread(pid, id) else { continue };
                let th = pr.threads.entry(id).or_default();
                let cpu = t.user_ns + t.system_ns;
                let wall = th.last.map(|l| (now - l).as_nanos() as f64).unwrap_or(0.0);
                let dcpu = match th.cpu {
                    Some(c) => cpu.saturating_sub(c) as f64,
                    None => cpu as f64,
                };
                th.cpu = Some(cpu);
                th.last = Some(now);
                th.cpu_ns += dcpu;
                rows.push((id, wall, (wall - dcpu).max(0.0), t.state));
            }
            // Spread the runnable time over the threads' off-CPU time.
            let off: f64 = rows.iter().map(|r| r.2).sum();
            let drq = if off > 0.0 { drq.min(off) } else { drq };
            for (id, _wall, rest, state) in rows {
                let th = pr.threads.get_mut(&id).unwrap();
                let share = if off > 0.0 { drq * rest / off } else { 0.0 };
                th.rq_ns += share;
                let rest = (rest - share).max(0.0);
                match state {
                    4 => th.uninter_ns += rest,
                    2 | 5 => th.stopped_ns += rest,
                    3 if has_kids => th.parent_ns += rest,
                    _ => th.wait_ns += rest,
                }
            }
            if off == 0.0 && drq > 0.0 {
                // First sighting: its runnable time so far, on its first thread.
                if let Some(th) = pr.threads.values_mut().next() {
                    th.rq_ns += drq;
                }
            }
            // Stacks: one long `sample` per longer-lived process.
            if want_stacks
                && pr.sampler.is_none()
                && pr.out.is_none()
                && pr.why_not.is_none()
                && age >= SAMPLE_AFTER
                && !finished
            {
                let running = procs_samplers(&procs);
                let pr = procs.get_mut(&pid).unwrap();
                if running < MAX_SAMPLERS {
                    match stacks::start_detached(pid, Duration::from_secs(3600), 10) {
                        Some(r) => pr.sampler = Some(r),
                        None => pr.why_not = Some("could not run /usr/bin/sample".into()),
                    }
                }
            }
        }
        let live_set: HashSet<i32> = live.iter().copied().collect();
        for (p, pr) in procs.iter_mut() {
            if !live_set.contains(p) && gone.insert(*p) {
                // `sample -mayDie` writes its report when the target dies.
                if let Some(r) = pr.sampler.take() {
                    match r.finish() {
                        Ok(o) => pr.out = Some(o),
                        Err(e) => pr.why_not = Some(e),
                    }
                }
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
    // Stop the samplers of processes still running.
    for pr in procs.values_mut() {
        if let Some(r) = pr.sampler.take() {
            match r.stop() {
                Ok(o) => pr.out = Some(o),
                Err(e) => pr.why_not = Some(e),
            }
        }
    }

    let mut tree = Tree::default();
    let mut st = want_stacks.then(|| Stacks::new("sample"));
    let mut runtimes: Vec<String> = Vec::new();
    let mut blind_ns = 0.0;
    let mut all_wait = 0.0;
    let pids: Vec<i32> = procs.keys().copied().collect();
    let mut results: Vec<(i32, HashMap<Cat, f64>, f64)> = Vec::new();
    for &pid in &pids {
        let pr = &procs[&pid];
        let mut by: HashMap<Cat, f64> = HashMap::new();
        let mut cpu = 0.0;
        for (id, th) in &pr.threads {
            cpu += th.cpu_ns;
            *by.entry(Cat::Cpu).or_default() += th.cpu_ns;
            *by.entry(Cat::CpuWait).or_default() += th.rq_ns;
            *by.entry(Cat::Idle).or_default() += th.stopped_ns;
            let sth = pr.out.as_ref().and_then(|o| o.thread(*id));
            // With a stack, a parent's wait is split like any other.
            let (wait_ns, parent_ns) = if sth.is_some() {
                (th.wait_ns + th.parent_ns, 0.0)
            } else {
                (th.wait_ns, th.parent_ns)
            };
            *by.entry(Cat::Idle).or_default() += parent_ns;
            all_wait += wait_ns + th.uninter_ns;
            // Leaf counts of this thread, by the category they count as.
            let mut blocked: HashMap<Cat, f64> = HashMap::new();
            if let Some(s) = sth {
                for (frames, c) in &s.stacks {
                    let l = stacks::classify(frames);
                    if l == Leaf::Cpu {
                        continue;
                    }
                    let cat = cat_of(&l, pid, &procs);
                    *blocked.entry(cat).or_default() += *c as f64;
                }
            }
            let total: f64 = blocked.values().sum();
            if total > 0.0 {
                for (cat, c) in &blocked {
                    *by.entry(*cat).or_default() += wait_ns * c / total;
                }
            } else {
                *by.entry(Cat::Idle).or_default() += wait_ns;
                if sth.is_none() {
                    blind_ns += wait_ns;
                }
            }
            // Uninterruptible: disk, whatever `sample` saw.
            *by.entry(Cat::Disk).or_default() += th.uninter_ns;
        }
        results.push((pid, by, cpu));
    }
    for (pid, by, cpu) in &results {
        let pr = &procs[pid];
        for (c, v) in by {
            *tree.by_cat.entry(*c).or_default() += v;
        }
        tree.procs.push(TreeProc {
            pid: *pid,
            name: pr.name.clone(),
            cpu_ns: *cpu,
            by_cat: by.clone(),
        });
        // Stacks, weighted by the thread's measured time in each category.
        if let (Some(st), Some(out)) = (st.as_mut(), pr.out.as_ref()) {
            let mut counts: HashMap<(u64, Cat), f64> = HashMap::new();
            for t in &out.threads {
                for (f, c) in &t.stacks {
                    let l = stacks::classify(f);
                    let cat = cat_of(&l, *pid, &procs);
                    *counts.entry((t.id, cat)).or_default() += *c as f64;
                }
            }
            let one = stacks::to_stacks(out, *pid, &pr.name, |tid, leaf, c| {
                let th = pr.threads.get(&tid)?;
                let cat = cat_of(leaf, *pid, &procs);
                let ns = match cat {
                    Cat::Cpu => th.cpu_ns,
                    Cat::Idle => return None,
                    Cat::Disk => th.wait_ns + th.parent_ns + th.uninter_ns,
                    _ => th.wait_ns + th.parent_ns,
                };
                let n = counts.get(&(tid, cat)).copied().unwrap_or(0.0);
                (n > 0.0 && ns > 0.0).then(|| (cat, ns * c as f64 / n))
            });
            merge(st, one);
            let exe = ffi::path(*pid).unwrap_or_default();
            if let Some(rt) = crate::lang::detect(&exe, out.images.iter().map(|p| p.as_str())) {
                let h = super::stacks::runtime_note(rt);
                if !runtimes.contains(&h) {
                    runtimes.push(h);
                }
            }
        }
    }
    // CPU of processes too short-lived to be seen.
    let measured: f64 = procs.values().flat_map(|p| p.threads.values()).map(|t| t.cpu_ns).sum();
    let launched = stacks::LAUNCHER_NS
        .load(std::sync::atomic::Ordering::Relaxed)
        .saturating_sub(launch0) as f64;
    let reaped = children_rusage_ns() - rusage0 - launched;
    let unseen = (reaped - measured).max(0.0);
    if unseen > 0.02 * reaped && unseen > 5e6 {
        *tree.by_cat.entry(Cat::Cpu).or_default() += unseen;
        tree.procs.push(TreeProc {
            pid: 0,
            name: "(short-lived processes)".into(),
            cpu_ns: unseen,
            by_cat: [(Cat::Cpu, unseen)].into_iter().collect(),
        });
    }
    if let Some(mut st) = st {
        st.compact();
        st.notes.extend(runtimes);
        // Not the failures of processes that came and went too fast.
        let failed: Vec<String> = procs
            .values()
            .filter_map(|p| p.why_not.clone())
            .filter(|e| e.contains("privilege") || e.contains("could not run") || e.contains("took over"))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for e in failed {
            st.notes.push(format!("no stacks for some processes: {e}"));
        }
        tree.stacks = (!st.is_empty()).then_some(st);
    }
    // Only when it hides a real amount of time, not a few ms around exit.
    if want_stacks && blind_ns > 20e6 && blind_ns > 0.5 * all_wait {
        tree.notes.push(
            "processes that lived under 60 ms, or beyond the first 8 at once, were not stack-sampled: their blocked time counts as idle".into(),
        );
    } else if !want_stacks && all_wait > 0.0 {
        tree.notes.push(
            "without stacks, macOS does not say what a blocked thread waits on: blocked time counts as idle (uninterruptible as disk)".into(),
        );
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

fn procs_samplers(procs: &HashMap<i32, Pr>) -> usize {
    procs.values().filter(|p| p.sampler.is_some()).count()
}

/// Add one process's stacks into the tree's.
fn merge(into: &mut Stacks, from: Stacks) {
    for (p, c) in &from.comms {
        into.comms.insert(*p, c.clone());
    }
    for s in &from.samples {
        let ids: Vec<usize> = s.frames.iter().map(|&i| into.intern(from.frames[i].clone())).collect();
        into.add(s.pid, s.tid, s.cat, ids, s.ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_lists_a_spawned_child() {
        let mut c = std::process::Command::new("/bin/sleep").arg("2").spawn().unwrap();
        let me = std::process::id() as i32;
        let kids = ffi::children(me);
        let _ = c.kill();
        let _ = c.wait();
        assert!(kids.contains(&(c.id() as i32)), "{kids:?}");
    }
}
