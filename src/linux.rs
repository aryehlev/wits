//! The Linux side of the command line: finding a process by name, checking
//! it exists, and the analyses.

pub use crate::explain::{analyze_full, Opts};
pub use crate::top::overview;
use crate::proc;
use std::time::Duration;

/// Resolve a name to a pid. With several matches, pick the one that is
/// doing or waiting the most over a short look. Also returns how many
/// other processes matched.
pub fn resolve(name: &str) -> Result<(i32, usize), String> {
    if name.contains('/') && !name.starts_with('/') {
        let v = crate::k8s::pod_pids(name)?;
        return Ok((v[0], v.len() - 1));
    }
    let me = std::process::id() as i32;
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    for pid in proc::pids() {
        if pid == me {
            continue;
        }
        let Some(st) = proc::stat(pid) else { continue };
        if st.state == 'Z' || st.is_kthread() {
            continue;
        }
        if st.comm == name {
            exact.push(pid);
        } else if proc::cmdline(pid).contains(name) {
            loose.push(pid);
        }
    }
    let mut c = if exact.is_empty() { loose } else { exact };
    c.sort_unstable();
    match c.len() {
        0 => Err(format!("no process named or matching '{name}'")),
        1 => Ok((c[0], 0)),
        n => {
            let score = |pid: i32| -> u64 {
                proc::tids(pid)
                    .into_iter()
                    .filter_map(|t| proc::task_schedstat(pid, t))
                    .map(|(r, w)| r + w)
                    .sum()
            };
            let before: Vec<u64> = c.iter().map(|&p| score(p)).collect();
            std::thread::sleep(Duration::from_millis(150));
            let best = c
                .iter()
                .zip(before)
                .map(|(&p, b)| {
                    let d_state = proc::stat(p).map(|s| s.state == 'D').unwrap_or(false);
                    (
                        score(p).saturating_sub(b) + if d_state { 150_000_000 } else { 0 },
                        p,
                    )
                })
                // Most active wins; among equals, the oldest (lowest pid).
                .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
                .map(|x| x.1)
                .unwrap_or(c[0]);
            Ok((best, n - 1))
        }
    }
}

/// Is there a live process with this pid?
pub fn check(pid: i32) -> Result<(), String> {
    match proc::stat(pid) {
        None => Err(format!("no process with pid {pid}")),
        Some(st) if st.state == 'Z' => Err(format!(
            "pid {pid} ({}) is a zombie: it has exited and its parent (pid {}) has not collected it",
            st.comm, st.ppid
        )),
        _ => Ok(()),
    }
}
