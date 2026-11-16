//! The macOS side: libproc for exact per-thread CPU time and runnable time,
//! Apple's `sample` for every thread's stack and what it is blocked in, and
//! descriptors to name the pipe, socket or file and the process at the
//! other end. No root needed for the user's own processes.

mod analyze;
mod fds;
mod ffi;
mod stacks;
mod tree;

pub use crate::answer::Opts;
pub use analyze::analyze_full;
pub use tree::watch_tree;

use crate::answer::{Cat, Row};
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn activity(pid: i32) -> u64 {
    let cpu = ffi::task(pid).map(|t| t.user_ns + t.system_ns).unwrap_or(0);
    let rq = ffi::usage(pid).map(|u| u.runnable_ns).unwrap_or(0);
    cpu + rq
}

/// Resolve a name to a pid: the command name first, then the executable
/// path's last component, then the command line. With several matches,
/// the most active over a short look.
pub fn resolve(name: &str) -> Result<(i32, usize), String> {
    let me = std::process::id() as i32;
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    for pid in ffi::pids() {
        if pid == me {
            continue;
        }
        let Some(s) = ffi::short(pid) else { continue };
        if s.status == 5 {
            continue;
        }
        let base = ffi::path(pid)
            .map(|p| p.rsplit('/').next().unwrap_or("").to_string())
            .unwrap_or_default();
        if s.comm == name || base == name {
            exact.push(pid);
        } else if ffi::cmdline(pid).map(|c| c.contains(name)).unwrap_or(false) {
            loose.push(pid);
        }
    }
    let mut c = if exact.is_empty() { loose } else { exact };
    c.sort_unstable();
    match c.len() {
        0 => Err(format!("no process named or matching '{name}'")),
        1 => Ok((c[0], 0)),
        n => {
            let before: Vec<u64> = c.iter().map(|&p| activity(p)).collect();
            std::thread::sleep(Duration::from_millis(150));
            let best = c
                .iter()
                .zip(before)
                .map(|(&p, b)| (activity(p).saturating_sub(b), p))
                .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
                .map(|x| x.1)
                .unwrap_or(c[0]);
            Ok((best, n - 1))
        }
    }
}

pub fn check(pid: i32) -> Result<(), String> {
    match ffi::short(pid) {
        None => Err(format!("no process with pid {pid}")),
        Some(s) if s.status == 5 => Err(format!(
            "pid {pid} ({}) is a zombie: it has exited and its parent (pid {}) has not collected it",
            s.comm, s.ppid
        )),
        _ => Ok(()),
    }
}

#[derive(Default)]
struct Acc {
    name: String,
    cpu0: u64,
    rq0: u64,
    cpu1: u64,
    rq1: u64,
    d_samples: u32,
}

/// `wits` with no arguments: the processes waiting most right now, each
/// explained. Ranking uses what macOS shows for every own process cheaply:
/// time runnable but not running, uninterruptible threads, CPU.
pub fn overview(dur: Duration) -> (Vec<Row>, u64) {
    let me = std::process::id() as i32;
    let uid = unsafe { libc::getuid() };
    let rank = dur.mul_f64(0.4);
    let start = Instant::now();
    let mut acc: HashMap<i32, Acc> = HashMap::new();
    for pid in ffi::pids() {
        if pid == me {
            continue;
        }
        let Some(s) = ffi::short(pid) else { continue };
        if s.status == 5 || (uid != 0 && s.uid != uid) {
            continue;
        }
        let Some(u) = ffi::usage(pid) else { continue };
        acc.insert(
            pid,
            Acc {
                name: s.comm,
                cpu0: u.user_ns + u.system_ns,
                rq0: u.runnable_ns,
                ..Acc::default()
            },
        );
    }
    const PASSES: u32 = 4;
    for i in 1..=PASSES {
        let next = start + rank.mul_f64(i as f64 / PASSES as f64);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        for (pid, a) in acc.iter_mut() {
            for id in ffi::thread_ids(*pid) {
                if ffi::thread(*pid, id).map(|t| t.state == 4).unwrap_or(false) {
                    a.d_samples += 1;
                }
            }
        }
    }
    let win = start.elapsed().as_nanos() as f64;
    for (pid, a) in acc.iter_mut() {
        if let Some(u) = ffi::usage(*pid) {
            a.cpu1 = u.user_ns + u.system_ns;
            a.rq1 = u.runnable_ns;
        } else {
            a.cpu1 = a.cpu0;
            a.rq1 = a.rq0;
        }
    }
    let mut scored: Vec<(i32, f64, f64)> = acc
        .iter()
        .map(|(pid, a)| {
            let waiting = a.rq1.saturating_sub(a.rq0) as f64 / win + a.d_samples as f64 / PASSES as f64;
            (*pid, waiting, a.cpu1.saturating_sub(a.cpu0) as f64 / win)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.total_cmp(&a.2)));
    let mut chosen: Vec<(i32, f64)> = scored
        .iter()
        .filter(|s| s.1 >= 0.01)
        .take(5)
        .map(|s| (s.0, s.1))
        .collect();
    if chosen.len() < 5 {
        let mut busy: Vec<&(i32, f64, f64)> = scored
            .iter()
            .filter(|s| s.1 < 0.01 && s.2 >= 0.01 && !chosen.iter().any(|c| c.0 == s.0))
            .collect();
        busy.sort_by(|a, b| b.2.total_cmp(&a.2));
        chosen.extend(busy.iter().take(5 - chosen.len()).map(|s| (s.0, s.1)));
    }
    let left = dur
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(500));
    let handles: Vec<_> = chosen
        .iter()
        .map(|&(pid, _)| {
            std::thread::spawn(move || {
                analyze_full(
                    pid,
                    Opts {
                        dur: left,
                        follow: false,
                        light: false,
                        stacks: false,
                    },
                )
                .map(|x| x.0)
            })
        })
        .collect();
    let mut rows: Vec<Row> = chosen
        .into_iter()
        .zip(handles)
        .map(|((pid, rough), h)| {
            let answer = h.join().ok().flatten();
            let waiting = match &answer {
                Some(a) => {
                    let cpu = a
                        .breakdown
                        .iter()
                        .find(|b| b.0 == Cat::Cpu)
                        .map(|b| b.1)
                        .unwrap_or(0.0);
                    a.active * (1.0 - cpu)
                }
                None => rough,
            };
            Row {
                pid,
                name: acc.get(&pid).map(|a| a.name.clone()).unwrap_or_default(),
                waiting,
                answer,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.waiting.total_cmp(&a.waiting));
    (rows, start.elapsed().as_millis() as u64)
}
