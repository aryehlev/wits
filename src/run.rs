//! `wits run` and `wits diff`: the improvement loop. Run a command, see
//! where its time went (cause by cause, down to functions), change the
//! code, run it again, and compare.

use crate::answer::{Cat, Tree, CAUSES};
use crate::code::{self, CodeView, Stacks};
use crate::json::{self, Value};
use crate::out::{esc, num, pct, Style};
#[cfg(not(target_os = "linux"))]
use crate::platform;
use crate::{die, Args};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Instant;

/// Find `prog` on PATH, as execvp would.
fn which(prog: &str) -> Option<String> {
    if prog.contains('/') {
        return Some(prog.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    path.split(':').map(|d| format!("{d}/{prog}")).find(|p| {
        let c = CString::new(p.as_str()).unwrap_or_default();
        unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
    })
}

struct Child {
    pid: i32,
    gate: i32,
    err: i32,
}

/// Fork the command, held at a gate until we have started watching it.
fn spawn(cmd: &[String], env: &[(String, String)]) -> Result<Child, String> {
    let path = which(&cmd[0]).ok_or_else(|| format!("{}: command not found", cmd[0]))?;
    let cpath = CString::new(path).map_err(|_| "bad command name")?;
    let argv: Vec<CString> = cmd
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap_or_default())
        .collect();
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    for (k, v) in env {
        vars.insert(k.clone(), v.clone());
    }
    let envs: Vec<CString> = vars
        .iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect();
    let mut envp: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());

    let mut gate = [0i32; 2];
    let mut err = [0i32; 2];
    unsafe {
        if libc::pipe(gate.as_mut_ptr()) != 0 || libc::pipe(err.as_mut_ptr()) != 0 {
            return Err("pipe failed".into());
        }
        libc::fcntl(err[1], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(gate[1], libc::F_SETFD, libc::FD_CLOEXEC);
        let pid = libc::fork();
        if pid < 0 {
            return Err("fork failed".into());
        }
        if pid == 0 {
            // Child: only async-signal-safe calls from here on.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::close(gate[1]);
            libc::close(err[0]);
            let mut b = [0u8; 1];
            libc::read(gate[0], b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(gate[0]);
            libc::execve(cpath.as_ptr(), argp.as_ptr(), envp.as_ptr());
            let e = *errno();
            libc::write(err[1], &e as *const i32 as *const libc::c_void, 4);
            libc::_exit(127);
        }
        libc::close(gate[0]);
        libc::close(err[1]);
        Ok(Child {
            pid,
            gate: gate[1],
            err: err[0],
        })
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut i32 {
    libc::__errno_location()
}
#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut i32 {
    libc::__error()
}

impl Child {
    /// Let it run; Err if exec failed.
    fn open(&self) -> Result<(), String> {
        unsafe {
            libc::write(self.gate, b"g".as_ptr() as *const libc::c_void, 1);
            libc::close(self.gate);
            let mut e = 0i32;
            let n = libc::read(self.err, &mut e as *mut i32 as *mut libc::c_void, 4);
            libc::close(self.err);
            if n == 4 {
                return Err(std::io::Error::from_raw_os_error(e).to_string());
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn become_subreaper() {
    // Orphaned grandchildren are reparented to us, so they stay in the tree.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}
#[cfg(not(target_os = "linux"))]
fn become_subreaper() {}

pub struct RunResult {
    pub wall_ns: f64,
    pub exit: i32,
    pub tree: Tree,
}

fn one_run(cmd: &[String], stacks: bool) -> Result<RunResult, String> {
    become_subreaper();
    let child = spawn(cmd, &crate::lang::run_env())?;
    let pid = child.pid;
    let mut t0 = None;
    // When the command was reaped: the time spent building the report
    // afterwards (symbolizing stacks) is not the command's.
    let mut t1 = None;
    let mut exec_err = None;
    let mut status = None;
    let mut open = || {
        t0 = Some(Instant::now());
        if let Err(e) = child.open() {
            exec_err = Some(e);
        }
    };
    let mut done = || {
        // Reap everything that exited, ours and adopted orphans.
        loop {
            let mut st = 0;
            let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
            if r <= 0 {
                break;
            }
            if r == pid {
                status = Some(st);
                t1 = Some(Instant::now());
            }
        }
        status.is_some()
    };
    #[cfg(target_os = "linux")]
    let tree = crate::tree::watch_tree_gated(pid, stacks, &mut open, &mut done);
    #[cfg(not(target_os = "linux"))]
    let tree = {
        open();
        platform::watch_tree(pid, stacks, &mut done)
    };
    let wall = match (t0, t1) {
        (Some(a), Some(b)) => b.duration_since(a).as_nanos() as f64,
        (Some(a), None) => a.elapsed().as_nanos() as f64,
        _ => 0.0,
    };
    if let Some(e) = exec_err {
        return Err(format!("{}: {e}", cmd[0]));
    }
    let st = status.unwrap_or(0);
    let exit = if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else if libc::WIFSIGNALED(st) {
        128 + libc::WTERMSIG(st)
    } else {
        1
    };
    Ok(RunResult {
        wall_ns: wall,
        exit,
        tree,
    })
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    if s.is_empty() {
        0.0
    } else if s.len() % 2 == 1 {
        s[s.len() / 2]
    } else {
        (s[s.len() / 2 - 1] + s[s.len() / 2]) / 2.0
    }
}

fn secs(ns: f64) -> String {
    let s = ns / 1e9;
    if s >= 100.0 {
        format!("{s:.0}s")
    } else if s >= 1.0 {
        format!("{s:.1}s")
    } else {
        format!("{:.0}ms", s * 1000.0)
    }
}

/// A finished `wits run`, as saved to JSON and read back by `wits diff`.
pub struct Report {
    pub command: Vec<String>,
    pub walls: Vec<f64>,
    pub exits: Vec<i32>,
    /// Mean thread time per cause over the runs, including idle.
    pub by_cat: Vec<(Cat, f64)>,
    pub cpu_ns: f64,
    pub procs: Vec<ProcRow>,
    pub code: Option<CodeView>,
    /// Every stack, for a flame graph (`code::flame_json`).
    pub flame: Option<String>,
    pub notes: Vec<String>,
}

/// A process name, how many ran, their CPU time, and their time by cause.
type ProcRow = (String, usize, f64, Vec<(Cat, f64)>);

impl Report {
    fn active(&self) -> f64 {
        self.by_cat
            .iter()
            .filter(|c| c.0 != Cat::Idle)
            .map(|c| c.1)
            .sum()
    }
}

fn build(cmd: &[String], runs: Vec<RunResult>) -> Report {
    let n = runs.len().max(1) as f64;
    let mut by_cat: HashMap<Cat, f64> = HashMap::new();
    let mut procs: HashMap<String, (usize, f64, HashMap<Cat, f64>)> = HashMap::new();
    let mut merged = Stacks::new("");
    let mut notes: Vec<String> = Vec::new();
    for r in &runs {
        for (c, v) in &r.tree.by_cat {
            *by_cat.entry(*c).or_default() += v / n;
        }
        for p in &r.tree.procs {
            let e = procs.entry(p.name.clone()).or_default();
            // pid 0: CPU time of processes too short-lived to be seen.
            if p.pid != 0 {
                e.0 += 1;
            }
            e.1 += p.cpu_ns / n;
            for (c, v) in &p.by_cat {
                *e.2.entry(*c).or_default() += v / n;
            }
        }
        if let Some(st) = &r.tree.stacks {
            if merged.source.is_empty() {
                merged.source = st.source;
            }
            for s in &st.samples {
                let ids = s
                    .frames
                    .iter()
                    .map(|&i| merged.intern(st.frames[i].clone()))
                    .collect();
                merged.add(s.pid, s.tid, s.cat, ids, s.ns / n);
            }
            for (p, c) in &st.comms {
                merged.comms.insert(*p, c.clone());
            }
            for x in &st.notes {
                if !notes.contains(x) {
                    notes.push(x.clone());
                }
            }
        }
        for x in &r.tree.notes {
            if !notes.contains(x) {
                notes.push(x.clone());
            }
        }
    }
    merged.compact();
    let mut by_cat: Vec<(Cat, f64)> = CAUSES
        .iter()
        .chain([Cat::Idle].iter())
        .map(|c| (*c, by_cat.get(c).copied().unwrap_or(0.0)))
        .collect();
    by_cat.sort_by(|a, b| (a.0 == Cat::Idle).cmp(&(b.0 == Cat::Idle)).then(b.1.total_cmp(&a.1)));
    let mut procs: Vec<ProcRow> = procs
        .into_iter()
        .map(|(name, (count, cpu, cats))| {
            let mut v: Vec<(Cat, f64)> = cats.into_iter().collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            (name, ((count as f64) / n).ceil() as usize, cpu, v)
        })
        .collect();
    let active = |p: &(String, usize, f64, Vec<(Cat, f64)>)| -> f64 {
        p.3.iter().filter(|c| c.0 != Cat::Idle).map(|c| c.1).sum()
    };
    procs.sort_by(|a, b| active(b).total_cmp(&active(a)));
    let cpu_ns = procs.iter().map(|p| p.2).sum();
    let code = (!merged.is_empty()).then(|| {
        let mut cv = code::summarize(&merged, 15);
        cv.notes.clear();
        cv
    });
    let flame = (!merged.is_empty()).then(|| code::flame_json(&merged, 2000));
    // Time nothing accounts for: no thread ran, and what they waited on has
    // no name. A program asleep on a timer (a retry back-off) looks like this,
    // and the causes above would otherwise say nothing about most of the run.
    let walls: Vec<f64> = runs.iter().map(|r| r.wall_ns).collect();
    let (wall, busy) = (median(&walls), by_cat.iter().filter(|c| c.0 != Cat::Idle).map(|c| c.1).sum::<f64>());
    if wall >= 1e9 && busy < 0.5 * wall {
        notes.insert(
            0,
            format!(
                "{} of the {} it took is not accounted for: nothing ran and it waited on nothing wits could name, so it was most likely asleep on a timer (a sleep, a retry back-off) or waiting for input",
                secs(wall - busy),
                secs(wall)
            ),
        );
    }
    Report {
        command: cmd.to_vec(),
        walls: runs.iter().map(|r| r.wall_ns).collect(),
        exits: runs.iter().map(|r| r.exit).collect(),
        by_cat,
        cpu_ns,
        procs,
        code,
        flame,
        notes,
    }
}

pub fn text(r: &Report, st: &Style) -> String {
    let wall = median(&r.walls);
    let times = if r.walls.len() > 1 {
        let lo = r.walls.iter().cloned().fold(f64::MAX, f64::min);
        let hi = r.walls.iter().cloned().fold(0.0, f64::max);
        format!("{} (median of {}, {}–{})", secs(wall), r.walls.len(), secs(lo), secs(hi))
    } else {
        secs(wall)
    };
    let exit = r.exits.last().copied().unwrap_or(0);
    let nproc: usize = r.procs.iter().map(|p| p.1).sum();
    let mut out = vec![st.bold(&format!(
        "wits: `{}` took {times} (exit {exit}); {nproc} process{}, {} on CPU",
        crate::out::shorten(&r.command.join(" "), 60),
        if nproc == 1 { "" } else { "es" },
        secs(r.cpu_ns)
    ))];
    let active = r.active();
    let row = |k: &str, v: &str| format!("  {} {v}", st.dim(&format!("{k:<9}")));
    for (c, ns) in &r.by_cat {
        if *c == Cat::Idle || active <= 0.0 || ns / active < 0.01 {
            continue;
        }
        // Which processes this time was in.
        let mut who: Vec<(String, usize, f64)> = r
            .procs
            .iter()
            .filter_map(|p| p.3.iter().find(|x| x.0 == *c).map(|x| (p.0.clone(), p.1, x.1)))
            .filter(|x| x.2 > 0.05 * ns && x.2 >= 1e6)
            .collect();
        who.sort_by(|a, b| b.2.total_cmp(&a.2));
        let who: Vec<String> = who
            .iter()
            .take(3)
            .map(|(n, k, v)| {
                if *k > 1 {
                    format!("{n} ×{k} {}", secs(*v))
                } else {
                    format!("{n} {}", secs(*v))
                }
            })
            .collect();
        out.push(row(
            c.key(),
            &format!("{:>4}  {:>6}   {}", pct(ns / active), secs(*ns), who.join(", ")),
        ));
    }
    if active <= 0.0 {
        out.push(row("", "it spent all its time idle"));
    }
    if let Some(cv) = &r.code {
        let mut first = true;
        for cc in cv.causes.iter().filter(|c| c.cat != Cat::Idle) {
            if active <= 0.0 || cc.ns / active < 0.05 {
                continue;
            }
            for h in cc.hot.iter().take(2).filter(|h| h.share >= 0.05) {
                let w = crate::out::where_text(h, cc.cat);
                out.push(row(if first { "where" } else { "" }, &format!("{}: {w}", cc.cat.key())));
                first = false;
            }
        }
    }
    for n in &r.notes {
        out.push(row("note", n));
    }
    out.join("\n")
}

fn seconds_json(v: &[(Cat, f64)]) -> String {
    let f: Vec<String> = v
        .iter()
        .map(|(c, ns)| format!("{}:{}", esc(c.key()), num(ns / 1e9)))
        .collect();
    format!("{{{}}}", f.join(","))
}

pub fn json(r: &Report) -> String {
    let mut f = vec![
        "\"version\":2".to_string(),
        "\"kind\":\"run\"".to_string(),
    ];
    let cmd: Vec<String> = r.command.iter().map(|c| esc(c)).collect();
    f.push(format!("\"command\":[{}]", cmd.join(",")));
    f.push(format!("\"exit\":{}", r.exits.last().copied().unwrap_or(0)));
    let walls: Vec<String> = r.walls.iter().map(|w| format!("{:.0}", w / 1e6)).collect();
    f.push(format!("\"wall_ms\":{:.0}", median(&r.walls) / 1e6));
    f.push(format!("\"runs_ms\":[{}]", walls.join(",")));
    f.push(format!("\"cpu_ms\":{:.0}", r.cpu_ns / 1e6));
    f.push(format!("\"seconds\":{}", seconds_json(&r.by_cat)));
    let active = r.active();
    let bd: Vec<String> = r
        .by_cat
        .iter()
        .filter(|c| c.0 != Cat::Idle)
        .map(|(c, ns)| format!("{}:{}", esc(c.key()), num(if active > 0.0 { ns / active } else { 0.0 })))
        .collect();
    f.push(format!("\"breakdown\":{{{}}}", bd.join(",")));
    let procs: Vec<String> = r
        .procs
        .iter()
        .take(50)
        .map(|(name, count, cpu, cats)| {
            format!(
                "{{\"name\":{},\"count\":{count},\"cpu_ms\":{:.0},\"seconds\":{}}}",
                esc(name),
                cpu / 1e6,
                seconds_json(cats)
            )
        })
        .collect();
    f.push(format!("\"processes\":[{}]", procs.join(",")));
    f.push(format!(
        "\"code\":{}",
        r.code
            .as_ref()
            .map(crate::out::code_json)
            .unwrap_or_else(|| "null".into())
    ));
    if let Some(fl) = &r.flame {
        f.push(format!("\"flame\":{fl}"));
    }
    let notes: Vec<String> = r.notes.iter().map(|n| esc(n)).collect();
    f.push(format!("\"notes\":[{}]", notes.join(",")));
    format!("{{{}}}", f.join(","))
}

pub fn run(a: &Args) -> i32 {
    if a.rest.is_empty() {
        die("which command? wits run [-o out.json] -- <command...>", 2);
    }
    let style = Style::detect();
    // Like time(1): the command gets the interrupt, we stay to report.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let mut runs = Vec::new();
    for i in 0..a.repeat {
        match one_run(&a.rest, a.stacks) {
            Ok(r) => {
                let failed = r.exit != 0;
                runs.push(r);
                if failed && i + 1 < a.repeat {
                    eprintln!("wits: run {} exited {}; stopping", i + 1, runs.last().unwrap().exit);
                    break;
                }
            }
            Err(e) => die(&e, 127),
        }
    }
    let exit = runs.last().map(|r| r.exit).unwrap_or(1);
    let rep = build(&a.rest, runs);
    let j = json(&rep);
    if let Some(p) = &a.html {
        crate::ui::write(p, &j);
    }
    if let Some(path) = &a.out {
        if let Err(e) = std::fs::write(path, format!("{j}\n")) {
            die(&format!("cannot write {path}: {e}"), 1);
        }
    }
    if a.json {
        eprintln!("{j}");
    } else {
        eprintln!("{}", text(&rep, &style));
        match &a.out {
            Some(p) => eprintln!(
                "{}",
                style.dim(&format!(
                    "saved to {p}; after a change, run it again with -o new.json and: wits diff {p} new.json"
                ))
            ),
            None => eprintln!(
                "{}",
                style.dim("save it with -o before.json to compare after a change (wits diff)")
            ),
        }
    }
    exit
}

// ---- diff ----------------------------------------------------------------

fn load(path: &str) -> Value {
    let s = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}"), 1));
    let v = json::parse(s.trim()).unwrap_or_else(|e| die(&format!("{path}: {e}"), 1));
    if v.get("kind").str() != Some("run") {
        die(&format!("{path}: not a `wits run -o` file"), 1);
    }
    v
}

/// Function seconds by (cause, function, file): lines move when code
/// changes, names rarely do.
fn functions(v: &Value) -> HashMap<(String, String, String), f64> {
    let mut m = HashMap::new();
    for c in v.get("code").get("causes").arr() {
        let cause = c.get("cause").str().unwrap_or("").to_string();
        if cause == "idle" {
            continue;
        }
        for h in c.get("hot").arr() {
            let f = h.get("frame");
            let key = (
                cause.clone(),
                f.get("func").str().unwrap_or("").to_string(),
                f.get("file").str().unwrap_or("").to_string(),
            );
            *m.entry(key).or_default() += h.get("seconds").num().unwrap_or(0.0);
        }
    }
    m
}

fn runs(v: &Value) -> Vec<f64> {
    let r: Vec<f64> = v.get("runs_ms").arr().iter().filter_map(|x| x.num()).collect();
    if r.is_empty() {
        vec![v.get("wall_ms").num().unwrap_or(0.0)]
    } else {
        r
    }
}

fn change(a: f64, b: f64) -> String {
    if a <= 0.0 {
        return String::new();
    }
    let d = (b - a) / a;
    format!("{}{:.0}%", if d >= 0.0 { "+" } else { "−" }, (d * 100.0).abs())
}

fn signed_secs(d: f64) -> String {
    let s = if d.abs() >= 1.0 {
        format!("{:.1}s", d.abs())
    } else {
        format!("{:.0}ms", d.abs() * 1000.0)
    };
    format!("{}{s}", if d >= 0.0 { "+" } else { "−" })
}

pub fn diff(a: &Args) -> i32 {
    let [pa, pb] = a.rest.as_slice() else {
        die("usage: wits diff <before.json> <after.json>", 2);
    };
    let style = Style::detect();
    let (va, vb) = (load(pa), load(pb));
    let (ra, rb) = (runs(&va), runs(&vb));
    let (wa, wb) = (median(&ra), median(&rb));
    let (lo_a, hi_a) = (ra.iter().cloned().fold(f64::MAX, f64::min), ra.iter().cloned().fold(0.0, f64::max));
    let (lo_b, hi_b) = (rb.iter().cloned().fold(f64::MAX, f64::min), rb.iter().cloned().fold(0.0, f64::max));
    let rel = if wa > 0.0 { (wb - wa) / wa } else { 0.0 };
    // A change counts when the runs do not overlap, or, with single runs,
    // when it is larger than single runs usually vary (5%).
    let enough = ra.len() >= 3 && rb.len() >= 3;
    let separated = hi_b < lo_a || lo_b > hi_a;
    let verdict = if enough {
        if separated && rel < 0.0 {
            "faster"
        } else if separated {
            "slower"
        } else {
            "no clear change"
        }
    } else if rel <= -0.05 {
        "faster"
    } else if rel >= 0.05 {
        "slower"
    } else {
        "no clear change"
    };
    let noise = if enough {
        if separated {
            "beyond run-to-run noise (the runs do not overlap)".to_string()
        } else {
            format!(
                "within run-to-run noise ({:.0}–{:.0} ms before, {:.0}–{:.0} ms after)",
                lo_a, hi_a, lo_b, hi_b
            )
        }
    } else {
        "single runs: repeat with -n 5 to tell a change from noise".to_string()
    };
    let cats: Vec<&str> = ["cpu", "cpu-wait", "disk", "memory", "lock", "peer"].to_vec();
    let sa = va.get("seconds");
    let sb = vb.get("seconds");
    let mut cause_rows: Vec<(String, f64, f64)> = cats
        .iter()
        .map(|c| (c.to_string(), sa.get(c).num().unwrap_or(0.0), sb.get(c).num().unwrap_or(0.0)))
        .filter(|(_, x, y)| (x - y).abs() >= 0.01 && (x - y).abs() >= 0.03 * x.max(*y))
        .collect();
    cause_rows.sort_by(|a, b| (b.2 - b.1).abs().total_cmp(&(a.2 - a.1).abs()));
    let (fa, fb) = (functions(&va), functions(&vb));
    // Compare functions only for causes both runs have stacks for: a run
    // without root has no on-CPU stacks, and that is not a speedup.
    let causes_of = |m: &HashMap<(String, String, String), f64>| -> std::collections::HashSet<String> {
        m.keys().map(|k| k.0.clone()).collect()
    };
    let (ca_set, cb_set) = (causes_of(&fa), causes_of(&fb));
    let src = |v: &Value| v.get("code").get("source").str().unwrap_or("none").to_string();
    let mut notes = Vec::new();
    if src(&va) != src(&vb) {
        let only: Vec<&String> = ca_set.symmetric_difference(&cb_set).collect();
        notes.push(format!(
            "stacks were collected differently ({} before, {} after){}",
            src(&va),
            src(&vb),
            if only.is_empty() {
                String::new()
            } else {
                let mut o: Vec<String> = only.iter().map(|s| s.to_string()).collect();
                o.sort();
                format!("; functions are not compared for {}", o.join(", "))
            }
        ));
    }
    let mut keys: Vec<&(String, String, String)> = fa
        .keys()
        .chain(fb.keys())
        .filter(|k| ca_set.contains(&k.0) && cb_set.contains(&k.0))
        .collect();
    keys.sort();
    keys.dedup();
    let mut fn_rows: Vec<(&(String, String, String), f64, f64)> = keys
        .into_iter()
        .map(|k| (k, fa.get(k).copied().unwrap_or(0.0), fb.get(k).copied().unwrap_or(0.0)))
        .filter(|(_, x, y)| (x - y).abs() >= 0.005)
        .collect();
    fn_rows.sort_by(|a, b| (b.2 - b.1).abs().total_cmp(&(a.2 - a.1).abs()));
    let (ca, cb) = (va.get("cpu_ms").num().unwrap_or(0.0), vb.get("cpu_ms").num().unwrap_or(0.0));

    if a.json || a.html.is_some() {
        let causes: Vec<String> = cause_rows
            .iter()
            .map(|(c, x, y)| format!("{{\"cause\":{},\"before\":{},\"after\":{}}}", esc(c), num(*x), num(*y)))
            .collect();
        let fns: Vec<String> = fn_rows
            .iter()
            .take(20)
            .map(|((c, f, file), x, y)| {
                format!(
                    "{{\"cause\":{},\"func\":{},\"file\":{},\"before\":{},\"after\":{}}}",
                    esc(c),
                    esc(f),
                    esc(file),
                    num(*x),
                    num(*y)
                )
            })
            .collect();
        let notes: Vec<String> = notes.iter().map(|n| esc(n)).collect();
        let j = format!(
            "{{\"version\":2,\"kind\":\"diff\",\"verdict\":{},\"wall_ms\":{{\"before\":{wa:.0},\"after\":{wb:.0},\"change\":{}}},\"noise\":{},\"cpu_ms\":{{\"before\":{ca:.0},\"after\":{cb:.0}}},\"causes\":[{}],\"functions\":[{}],\"notes\":[{}]}}",
            esc(verdict),
            num(rel),
            esc(&noise),
            causes.join(","),
            fns.join(","),
            notes.join(",")
        );
        if let Some(p) = &a.html {
            crate::ui::write(p, &j);
        }
        if a.json {
            println!("{j}");
            return 0;
        }
    }
    let mut out = vec![style.bold(&format!(
        "{verdict}: {} → {} ({}), {noise}",
        secs(wa * 1e6),
        secs(wb * 1e6),
        change(wa, wb)
    ))];
    let row = |k: &str, v: &str| format!("  {} {v}", style.dim(&format!("{k:<9}")));
    out.push(row(
        "on CPU",
        &format!("{} → {} ({})", secs(ca * 1e6), secs(cb * 1e6), change(ca, cb)),
    ));
    for (c, x, y) in cause_rows.iter().take(6) {
        out.push(row(
            c,
            &format!("{} → {} ({})", secs(x * 1e9), secs(y * 1e9), signed_secs(y - x)),
        ));
    }
    if !fn_rows.is_empty() {
        out.push(String::new());
        out.push(style.bold("  functions that changed most"));
        for ((c, f, file), x, y) in fn_rows.iter().take(8) {
            let loc = if file.is_empty() {
                String::new()
            } else {
                format!(" ({})", code::short_path(file))
            };
            let tag = if *x == 0.0 {
                " new"
            } else if *y == 0.0 {
                " gone"
            } else {
                ""
            };
            out.push(format!(
                "    {:>7}  {f}{loc}  {}{tag}",
                signed_secs(y - x),
                style.dim(&format!("{c} {} → {}", secs(x * 1e9), secs(y * 1e9)))
            ));
        }
    }
    for n in &notes {
        out.push(row("note", n));
    }
    println!("{}", out.join("\n"));
    0
}
