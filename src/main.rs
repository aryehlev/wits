//! wits: explain why a process is slow, and where in its code.

// libc::c_long is i64 on the targets we ship and i32 on 32-bit ones.
#![allow(clippy::unnecessary_cast)]

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("wits works on Linux and macOS.");

mod answer;
mod code;
mod guide;
mod hint;
mod json;
mod lang;
mod out;
mod mcp;
mod on;
mod run;
mod ui;

#[cfg(target_os = "linux")]
mod bpf;
#[cfg(target_os = "linux")]
mod cluster;
#[cfg(target_os = "linux")]
mod ebpf;
#[cfg(target_os = "linux")]
mod explain;
#[cfg(target_os = "linux")]
mod http;
#[cfg(target_os = "linux")]
mod group;
#[cfg(target_os = "linux")]
mod k8s;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod proc;
#[cfg(target_os = "linux")]
mod prof;
#[cfg(target_os = "linux")]
mod sample;
#[cfg(target_os = "linux")]
mod stackscan;
#[cfg(target_os = "linux")]
mod symbolize;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod sysno;
#[cfg(target_os = "linux")]
mod top;
#[cfg(target_os = "linux")]
mod tree;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
use mac as platform;

use std::time::Duration;

const HELP: &str = "\
wits: explain why a process is slow, and where in its code

USAGE
  wits <pid|name>            why is this process slow? (cause, share of time, object,
                             who is to blame, and where in the code)
  wits                       the processes waiting most right now
  wits profile <pid|name>    where the time goes, cause by cause, with call paths
  wits run [-n <count>] [-o out.json] -- <command...>
                             run a command (count times) and explain where its
                             time went; -o saves the report for diff
  wits diff <before.json> <after.json>
                             compare two runs: did the change help, and where?
  wits <target> <target>...  several at once, busiest first
  wits <namespace>/<pod>[/<container>]
                             (Linux) a Kubernetes pod: each of its processes
  wits <namespace>/<deployment|statefulset|daemonset>
                             (Linux) every replica, on this node and, with
                             agents, on the others
  wits agent [--listen <addr:port>] [--peers <host[:port],...>]
                             (Linux) answer other nodes' wits: who holds this
                             connection, and why is it slow?
  wits on <where> [...]      the same on another machine: [user@]host over
                             ssh, or a cloud VM by name (gcp:<instance>,
                             aws:<instance>, azure:<group>/<vm>); what
                             follows is what you would type there
                             (see `wits on --help`)

  wits guide [<topic>]       what usually makes a program slow and what usually
                             fixes it: python, node, go, rust, jvm, dotnet,
                             ruby, beam, c, or general (any language)
  wits mcp                   an MCP server on stdin/stdout for coding agents

OPTIONS
  -t, --time <secs>   how long to watch (default 2)
      --json          machine-readable output
      --no-follow     do not descend into the child a process is waiting for
      --no-stacks     do not collect stacks (no `where`)
      --folded        (profile) folded stacks for flamegraph.pl, inferno, speedscope
      --html <file>   also write the answer as a web page, with flame graphs
      --peers <list>  wits agents on other machines to follow blame to
                      (host[:port],...; a name may resolve to many;
                      default $WITS_PEERS; token from $WITS_TOKEN(_FILE))
  -h, --help          this text
  -V, --version       version

CAUSES
  cpu        busy on CPU: nothing is holding it back
  cpu-wait   runnable, but another process or a cgroup limit has the CPU
  disk       blocked on storage I/O
  memory     swapping or reclaiming memory
  lock       queued on a file lock or a mutex
  peer       waiting for another process or the network

wits needs no root and changes nothing. With sudo it sees other users'
processes, and on Linux uses eBPF to show where on-CPU time goes.
";

pub fn die(msg: &str, code: i32) -> ! {
    eprintln!("wits: {msg}");
    std::process::exit(code);
}

#[derive(Default)]
pub struct Args {
    pub secs: Option<f64>,
    pub json: bool,
    pub follow: bool,
    pub stacks: bool,
    pub folded: bool,
    pub out: Option<String>,
    pub repeat: u32,
    pub peers: Option<String>,
    pub listen: Option<String>,
    pub html: Option<String>,
    pub rest: Vec<String>,
}

fn parse(args: impl Iterator<Item = String>) -> Args {
    let mut a = Args {
        follow: true,
        stacks: true,
        repeat: 1,
        ..Args::default()
    };
    let mut it = args.peekable();
    while let Some(s) = it.next() {
        match s.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("wits {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--json" => a.json = true,
            "--no-follow" => a.follow = false,
            "--no-stacks" => a.stacks = false,
            "--folded" => a.folded = true,
            "-o" | "--output" => {
                a.out = Some(it.next().unwrap_or_else(|| die("-o needs a file name", 2)))
            }
            "--peers" => {
                a.peers = Some(it.next().unwrap_or_else(|| die("--peers needs a list", 2)))
            }
            "--html" => {
                a.html = Some(it.next().unwrap_or_else(|| die("--html needs a file name", 2)))
            }
            "--listen" => {
                a.listen = Some(it.next().unwrap_or_else(|| die("--listen needs addr:port", 2)))
            }
            "-n" | "--repeat" => {
                a.repeat = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n: &u32| (1..=100).contains(n))
                    .unwrap_or_else(|| die("-n needs a count between 1 and 100", 2))
            }
            "-t" | "--time" => {
                a.secs = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .filter(|v: &f64| *v >= 0.1 && *v <= 3600.0)
                        .unwrap_or_else(|| {
                            die("--time needs a number of seconds between 0.1 and 3600", 2)
                        }),
                )
            }
            "--" => {
                a.rest.extend(it.by_ref());
            }
            s if s.starts_with('-') && s.len() > 1 && a.rest.is_empty() => {
                die(&format!("unknown option '{s}' (try --help)"), 2)
            }
            _ => a.rest.push(s),
        }
    }
    a
}

fn target(a: &Args) -> (i32, usize, String) {
    let t = match a.rest.as_slice() {
        [t] => t.clone(),
        [] => die("which process? give a pid or a name (try --help)", 2),
        _ => die("give one pid or name (try --help)", 2),
    };
    let (pid, others) = match t.parse::<i32>() {
        Ok(p) if p > 0 => (p, 0),
        _ => match platform::resolve(&t) {
            Ok(x) => x,
            Err(e) => {
                #[cfg(target_os = "linux")]
                if t.contains('/') {
                    remote_pod(a, &t, &e);
                }
                die(&e, 1)
            }
        },
    };
    if let Err(e) = platform::check(pid) {
        die(&e, 1);
    }
    (pid, others, t)
}

fn explain(a: &Args, profile: bool) {
    let style = out::Style::detect();
    let (pid, others, t) = target(a);
    let dur = Duration::from_secs_f64(a.secs.unwrap_or(2.0));
    let opts = platform::Opts {
        dur,
        follow: a.follow,
        light: false,
        stacks: a.stacks || profile,
    };
    let Some((mut ans, stacks)) = platform::analyze_full(pid, opts) else {
        die(&format!("pid {pid} exited before wits could look at it"), 1);
    };
    if others > 0 {
        ans.notes.push(format!(
            "{others} other processes match '{t}'; this is the most active one"
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let cfg = cluster::Config::load(a.peers.as_deref());
        cluster::follow(&mut ans, &cfg, dur.as_secs_f64(), opts.stacks, 0);
    }
    if profile && a.folded {
        match &stacks {
            Some(s) if !s.is_empty() => println!("{}", code::folded(s, false)),
            _ => die("no stacks were collected", 1),
        }
        return;
    }
    // `profile --json` and the page carry every stack, for a flame graph.
    let page = a.html.is_some();
    if a.json || page {
        let mut j = out::json(&ans);
        if profile || page {
            if let Some(s) = stacks.as_ref().filter(|s| !s.is_empty()) {
                j = ui::with_field(&j, "flame", &code::flame_json(s, 3000));
            }
        }
        if let Some(p) = &a.html {
            ui::write(p, &j);
        }
        if a.json {
            println!("{j}");
            return;
        }
    }
    if profile {
        println!("{}", out::profile_text(&ans, &style));
    } else {
        println!("{}", out::text(&ans, &style));
    }
}

/// A pod that is not on this node: ask the agents on the others.
#[cfg(target_os = "linux")]
fn remote_pod(a: &Args, t: &str, local_err: &str) -> ! {
    let cfg = cluster::Config::load(a.peers.as_deref());
    if cfg.peers.is_empty() {
        die(local_err, 1);
    }
    match cluster::explain_remote(&cfg, t, a.secs.unwrap_or(2.0), a.stacks) {
        Ok((_, v)) => {
            if a.json {
                println!("{}", v.get("answer"));
            } else {
                let node = v.get("node").str().unwrap_or("?");
                println!("{}", v.get("text").str().unwrap_or(""));
                let st = out::Style::detect();
                println!("{}", st.dim(&format!("  (answered by the wits agent on node {node})")));
            }
            std::process::exit(0)
        }
        Err(e) => die(&e, 1),
    }
}

fn main() {
    let mut argv = std::env::args().skip(1).peekable();
    let cmd = argv.peek().cloned();
    // `wits ... | head`: exit quietly when the reader goes away. The
    // long-running servers keep ignoring SIGPIPE, so a client that hangs
    // up cannot stop them.
    if !matches!(cmd.as_deref(), Some("agent") | Some("mcp")) {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match cmd.as_deref() {
        Some("profile") => {
            argv.next();
            explain(&parse(argv), true);
        }
        Some("run") => {
            argv.next();
            std::process::exit(run::run(&parse(argv)));
        }
        Some("mcp") => {
            argv.next();
            std::process::exit(mcp::serve());
        }
        Some("guide") => {
            argv.next();
            std::process::exit(guide::run(&parse(argv).rest));
        }
        Some("diff") => {
            argv.next();
            std::process::exit(run::diff(&parse(argv)));
        }
        Some("on") => {
            argv.next();
            std::process::exit(on::run(argv.collect()));
        }
        #[cfg(target_os = "linux")]
        Some("agent") => {
            argv.next();
            let a = parse(argv);
            std::process::exit(cluster::agent(a.listen.as_deref(), a.peers.as_deref()));
        }
        _ => {
            let a = parse(argv);
            if a.rest.is_empty() {
                let style = out::Style::detect();
                let dur = Duration::from_secs_f64(a.secs.unwrap_or(2.0));
                let (rows, ms) = platform::overview(dur);
                if let Some(p) = &a.html {
                    ui::write(p, &out::overview_json(&rows, ms));
                }
                if a.json {
                    println!("{}", out::overview_json(&rows, ms));
                } else {
                    println!("{}", out::overview_text(&rows, ms, &style));
                }
            } else {
                #[cfg(target_os = "linux")]
                if a.rest.len() > 1 || a.rest[0].contains('/') && !a.rest[0].starts_with('/') {
                    let opts = platform::Opts {
                        dur: Duration::from_secs_f64(a.secs.unwrap_or(2.0)),
                        follow: a.follow,
                        light: false,
                        stacks: a.stacks,
                    };
                    group::explain(&a.rest, opts, a.peers.as_deref(), a.json, a.html.as_deref());
                    return;
                }
                explain(&a, false);
            }
        }
    }
}
