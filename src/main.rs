//! wits: explain why a process is slow on your Linux system.

// libc::c_long is i64 on the targets we ship and i32 on 32-bit ones.
#![allow(clippy::unnecessary_cast)]

#[cfg(not(target_os = "linux"))]
compile_error!("wits only works on Linux: it reads /proc.");

mod explain;
mod net;
mod out;
mod proc;
mod sample;
mod sys;
mod sysno;
mod top;

use std::time::Duration;

const HELP: &str = "\
wits: explain why a process is slow on your Linux system

USAGE
  wits <pid|name>     why is this process slow? (cause, share of time, object, who is to blame)
  wits                the processes waiting most right now

OPTIONS
  -t, --time <secs>   how long to watch (default 2)
      --json          machine-readable output
      --no-follow     do not descend into the child a process is waiting for
  -h, --help          this text
  -V, --version       version

CAUSES
  cpu        busy on CPU: nothing is holding it back
  cpu-wait   runnable, but another process or a cgroup limit has the CPU
  disk       blocked on storage I/O
  memory     swapping or reclaiming memory
  lock       queued on a file lock or a mutex
  peer       waiting for another process or the network

wits reads /proc for a couple of seconds. No root, no eBPF, nothing to install.
It sees most for your own processes; for others' it needs sudo.
";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("wits: {msg}");
    std::process::exit(code);
}

/// Resolve a name to a pid. With several matches, pick the one that is
/// doing or waiting the most over a short look.
fn resolve(name: &str) -> (i32, usize) {
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
        0 => die(&format!("no process named or matching '{name}'"), 1),
        1 => (c[0], 0),
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
            (best, n - 1)
        }
    }
}

fn main() {
    let mut secs = 2.0f64;
    let mut json = false;
    let mut follow = true;
    let mut target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("wits {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--json" => json = true,
            "--no-follow" => follow = false,
            "-t" | "--time" => {
                secs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|v: &f64| *v >= 0.1 && *v <= 3600.0)
                    .unwrap_or_else(|| {
                        die("--time needs a number of seconds between 0.1 and 3600", 2)
                    });
            }
            s if s.starts_with('-') && s.len() > 1 => {
                die(&format!("unknown option '{s}' (try --help)"), 2)
            }
            _ if target.is_some() => die("give one pid or name (try --help)", 2),
            _ => target = Some(a),
        }
    }
    let dur = Duration::from_secs_f64(secs);
    let style = out::Style::detect();

    let Some(t) = target else {
        let (rows, ms) = top::overview(dur);
        if json {
            println!("{}", out::overview_json(&rows, ms));
        } else {
            println!("{}", out::overview_text(&rows, ms, &style));
        }
        return;
    };

    let (pid, others) = match t.parse::<i32>() {
        Ok(p) if p > 0 => (p, 0),
        _ => resolve(&t),
    };
    match proc::stat(pid) {
        None => die(&format!("no process with pid {pid}"), 1),
        Some(st) if st.state == 'Z' => {
            die(&format!("pid {pid} ({}) is a zombie: it has exited and its parent (pid {}) has not collected it", st.comm, st.ppid), 1)
        }
        _ => {}
    }
    let Some(mut ans) = explain::analyze(
        pid,
        explain::Opts {
            dur,
            follow,
            light: false,
        },
    ) else {
        die(&format!("pid {pid} exited before wits could look at it"), 1);
    };
    if others > 0 {
        ans.notes.push(format!(
            "{others} other processes match '{t}'; this is the most active one"
        ));
    }
    if json {
        println!("{}", out::json(&ans));
    } else {
        println!("{}", out::text(&ans, &style));
    }
}
