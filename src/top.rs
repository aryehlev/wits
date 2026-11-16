//! `wits` with no arguments: find the processes that are waiting most right
//! now, then explain each of them.

pub use crate::answer::Row;
use crate::explain::{self, Opts};
use crate::proc;
use crate::sys;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Acc {
    name: String,
    run0: u64,
    wait0: u64,
    run1: u64,
    wait1: u64,
    d_samples: u32,
}

fn sched_totals(pid: i32) -> (u64, u64) {
    let mut t = (0, 0);
    for tid in proc::tids(pid) {
        if let Some((r, w)) = proc::task_schedstat(pid, tid) {
            t.0 += r;
            t.1 += w;
        }
    }
    t
}

pub fn overview(dur: Duration) -> (Vec<Row>, u64) {
    let me = std::process::id() as i32;
    let rank = dur.mul_f64(0.4);
    let start = Instant::now();
    let mut acc: HashMap<i32, Acc> = HashMap::new();
    for pid in proc::pids() {
        if pid == me {
            continue;
        }
        let Some(st) = proc::stat(pid) else { continue };
        if st.is_kthread() || st.state == 'Z' {
            continue;
        }
        let (r, w) = sched_totals(pid);
        acc.insert(
            pid,
            Acc {
                name: st.comm,
                run0: r,
                wait0: w,
                ..Acc::default()
            },
        );
    }
    const PASSES: u32 = 5;
    for i in 1..=PASSES {
        let next = start + rank.mul_f64(i as f64 / PASSES as f64);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        for (pid, a) in acc.iter_mut() {
            for tid in proc::tids(*pid) {
                if proc::task_stat(*pid, tid)
                    .map(|s| s.state == 'D')
                    .unwrap_or(false)
                {
                    a.d_samples += 1;
                }
            }
        }
    }
    let win = start.elapsed().as_nanos() as f64;
    for (pid, a) in acc.iter_mut() {
        let (r, w) = sched_totals(*pid);
        a.run1 = r;
        a.wait1 = w;
    }
    // Processes queued on a file lock are certainly waiting.
    let lock_waiters: Vec<i32> = sys::parse_locks(&proc::slurp("/proc/locks").unwrap_or_default())
        .into_iter()
        .filter(|l| l.waiting)
        .map(|l| l.pid)
        .collect();
    let mut scored: Vec<(i32, f64, f64)> = acc
        .iter()
        .map(|(pid, a)| {
            let mut waiting =
                a.wait1.saturating_sub(a.wait0) as f64 / win + a.d_samples as f64 / PASSES as f64;
            if lock_waiters.contains(pid) {
                waiting += 1.0;
            }
            (*pid, waiting, a.run1.saturating_sub(a.run0) as f64 / win)
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
        // Fill up with the busiest processes so there is always something to see.
        let mut busy: Vec<&(i32, f64, f64)> = scored
            .iter()
            .filter(|s| s.1 < 0.01 && s.2 >= 0.01)
            .collect();
        busy.sort_by(|a, b| b.2.total_cmp(&a.2));
        chosen.extend(busy.iter().take(5 - chosen.len()).map(|s| (s.0, s.1)));
    }
    let left = dur
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(300));
    let handles: Vec<_> = chosen
        .iter()
        .map(|&(pid, _)| {
            std::thread::spawn(move || {
                explain::analyze(
                    pid,
                    Opts {
                        dur: left,
                        follow: false,
                        light: false,
                        stacks: false,
                    },
                )
            })
        })
        .collect();
    let mut rows: Vec<Row> = chosen
        .into_iter()
        .zip(handles)
        .map(|((pid, rough), h)| {
            let answer = h.join().ok().flatten();
            // Prefer the careful measurement: non-idle time not spent on CPU.
            let waiting = match &answer {
                Some(a) => {
                    let cpu = a
                        .breakdown
                        .iter()
                        .find(|b| b.0 == explain::Cat::Cpu)
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
