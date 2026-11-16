//! Where in the program the time went. Every backend (eBPF, perf, stack
//! scanning, macOS `sample`) produces `Stacks`; this module turns them into
//! the few code locations worth showing, and into folded stacks for flame
//! graphs.

use crate::answer::{Cat, CAUSES};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Frame {
    /// Demangled function name, or "[unknown]".
    pub func: String,
    /// Short name of the binary or library, "[kernel]" or "[jit]".
    pub module: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub kernel: bool,
}

impl Frame {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by the Linux backends
    pub fn unknown(module: &str) -> Frame {
        Frame {
            func: "[unknown]".into(),
            module: module.into(),
            ..Frame::default()
        }
    }

    /// "func (file:line)" or "func (module)".
    pub fn label(&self) -> String {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{} ({}:{l})", self.func, short_path(f)),
            (Some(f), None) => format!("{} ({})", self.func, short_path(f)),
            _ if self.module.is_empty() => self.func.clone(),
            _ => format!("{} ({})", self.func, self.module),
        }
    }
}

/// The last three components of a source path: enough to find it.
pub fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        return p.to_string();
    }
    format!("…/{}", parts[parts.len() - 3..].join("/"))
}

/// One distinct stack and the time spent in it.
#[derive(Clone, Debug)]
pub struct Sample {
    pub pid: i32,
    pub tid: i32,
    /// What the time was: on CPU, queued for a CPU, or the kind of wait.
    /// `Cat::Idle` for waits that are not a cause (sleeping, no work).
    pub cat: Cat,
    /// Indices into `Stacks::frames`, leaf first.
    pub frames: Vec<usize>,
    pub ns: f64,
}

#[derive(Debug, Default)]
pub struct Stacks {
    /// "ebpf", "perf", "stack-scan", "sample".
    pub source: &'static str,
    pub frames: Vec<Frame>,
    index: HashMap<Frame, usize>,
    pub samples: Vec<Sample>,
    pub notes: Vec<String>,
    /// Process names, for folded output.
    pub comms: HashMap<i32, String>,
}

impl Stacks {
    pub fn new(source: &'static str) -> Stacks {
        Stacks {
            source,
            ..Stacks::default()
        }
    }

    pub fn intern(&mut self, f: Frame) -> usize {
        if let Some(&i) = self.index.get(&f) {
            return i;
        }
        let i = self.frames.len();
        self.frames.push(f.clone());
        self.index.insert(f, i);
        i
    }

    pub fn add(&mut self, pid: i32, tid: i32, cat: Cat, frames: Vec<usize>, ns: f64) {
        if ns > 0.0 {
            self.samples.push(Sample {
                pid,
                tid,
                cat,
                frames,
                ns,
            });
        }
    }

    pub fn total(&self, cat: Cat) -> f64 {
        self.samples
            .iter()
            .filter(|s| s.cat == cat)
            .map(|s| s.ns)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Merge identical (pid, tid, cat, stack) entries.
    pub fn compact(&mut self) {
        let mut m: HashMap<(i32, i32, Cat, Vec<usize>), f64> = HashMap::new();
        for s in self.samples.drain(..) {
            *m.entry((s.pid, s.tid, s.cat, s.frames)).or_default() += s.ns;
        }
        self.samples = m
            .into_iter()
            .map(|((pid, tid, cat, frames), ns)| Sample {
                pid,
                tid,
                cat,
                frames,
                ns,
            })
            .collect();
        self.samples.sort_by(|a, b| b.ns.total_cmp(&a.ns));
    }

    /// Keep only these processes' samples.
    /// Scale each category so its total matches a measured amount (stack
    /// samples count events; the scheduler counters measure time).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by the Linux backends
    pub fn scale_to(&mut self, cat: Cat, ns: f64) {
        let have = self.total(cat);
        if have > 0.0 && ns > 0.0 {
            let k = ns / have;
            for s in self.samples.iter_mut().filter(|s| s.cat == cat) {
                s.ns *= k;
            }
        }
    }
}

/// Code that belongs to the language runtime or the C library rather than
/// to the program: skipped when looking for "where in your code".
pub fn is_runtime(f: &Frame) -> bool {
    if f.kernel || f.func == "[unknown]" {
        return true;
    }
    // A system header's inline wrapper (glibc's fortified open(), say),
    // inlined into the program: the program's own frame is the caller.
    if f.file.as_deref().is_some_and(|p| p.starts_with("/usr/include/")) {
        return true;
    }
    let m = f.module.as_str();
    const LIBS: &[&str] = &[
        "libc.so",
        "libc-",
        "libc.musl",
        "ld-musl",
        "libpthread",
        "ld-linux",
        "ld64.so",
        "libgcc_s",
        "libstdc++",
        "libc++",
        "libm.so",
        "libdl.so",
        "librt.so",
        "linux-vdso",
        "[vdso]",
        "libsystem_",
        "libdyld",
        "libdispatch",
        "dyld",
        "libobjc",
        "libjvm",
        "libpython",
        "libruby",
        "libcoreclr",
        "libuv",
    ];
    if LIBS.iter().any(|l| m.starts_with(l)) {
        return true;
    }
    let n = f.func.as_str();
    const FUNCS: &[&str] = &[
        // Rust standard library
        "std::",
        "core::",
        "alloc::",
        "<std::",
        "<core::",
        "<alloc::",
        "__rust",
        "rust_begin_unwind",
        // Go runtime and standard plumbing
        "runtime.",
        "syscall.",
        "internal/",
        // CPython interpreter
        "_Py",
        "Py",
        "method_vectorcall",
        "cfunction_",
        "slot_tp_",
        "builtin_",
        // V8 / Node.js
        "Builtin:",
        "Stub:",
        "BytecodeHandler:",
        "v8::",
        "node::",
        "uv_",
        "uv__",
        // JVM
        "Interpreter",
        "StubRoutines",
        // glibc symbols that leak into executables
        "__libc_",
        "__GI_",
        "_pthread_",
    ];
    const EXACT: &[&str] = &["_start", "start_thread", "thread_start", "clone", "clone3"];
    FUNCS.iter().any(|p| n.starts_with(p)) || EXACT.contains(&n)
}

/// The first frame from the leaf that is the program's own code, or the
/// leaf itself when the whole stack is runtime.
fn anchor(st: &Stacks, frames: &[usize]) -> Option<usize> {
    let user: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| !st.frames[i].kernel)
        .collect();
    user.iter()
        .copied()
        .find(|&i| !is_runtime(&st.frames[i]))
        .or(user.first().copied())
}

#[derive(Clone, Debug)]
pub struct Hot {
    pub ns: f64,
    /// Share of this cause's time seen in stacks.
    pub share: f64,
    /// The first frame in the program's own code.
    pub frame: Frame,
    /// The frame actually on top, when it is runtime code (memcpy, read...).
    pub leaf: Option<Frame>,
    /// Up to three callers of `frame`, nearest first.
    pub callers: Vec<Frame>,
}

#[derive(Clone, Debug)]
pub struct CauseCode {
    pub cat: Cat,
    pub ns: f64,
    pub hot: Vec<Hot>,
}

#[derive(Clone, Debug)]
pub struct CodeView {
    pub source: &'static str,
    /// Causes in order of time, then idle last.
    pub causes: Vec<CauseCode>,
    pub notes: Vec<String>,
}

impl CodeView {
    pub fn get(&self, cat: Cat) -> Option<&CauseCode> {
        self.causes.iter().find(|c| c.cat == cat)
    }
}

/// Group each cause's time by the program frame it passes through.
pub fn summarize(st: &Stacks, per_cause: usize) -> CodeView {
    let mut causes = Vec::new();
    for cat in CAUSES.iter().copied().chain([Cat::Idle]) {
        let total = st.total(cat);
        if total <= 0.0 {
            continue;
        }
        // anchor frame -> (ns, heaviest sample index, its ns)
        let mut by: HashMap<Option<usize>, (f64, usize, f64)> = HashMap::new();
        for (si, s) in st.samples.iter().enumerate().filter(|x| x.1.cat == cat) {
            let a = anchor(st, &s.frames);
            let e = by.entry(a).or_insert((0.0, si, 0.0));
            e.0 += s.ns;
            if s.ns > e.2 {
                e.1 = si;
                e.2 = s.ns;
            }
        }
        let mut v: Vec<(Option<usize>, (f64, usize, f64))> = by.into_iter().collect();
        v.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
        let hot = v
            .into_iter()
            // A stack nobody could name says nothing about where.
            .filter(|(a, _)| a.map(|i| st.frames[i].func != "[unknown]").unwrap_or(false))
            .take(per_cause)
            .map(|(a, (ns, si, _))| {
                let a = a.unwrap();
                let s = &st.samples[si];
                let user: Vec<usize> = s
                    .frames
                    .iter()
                    .copied()
                    .filter(|&i| !st.frames[i].kernel)
                    .collect();
                let pos = user.iter().position(|&i| i == a).unwrap_or(0);
                let leaf = (pos > 0).then(|| st.frames[user[0]].clone());
                let callers = user
                    .iter()
                    .skip(pos + 1)
                    .filter(|&&i| !is_runtime(&st.frames[i]))
                    .take(3)
                    .map(|&i| st.frames[i].clone())
                    .collect();
                Hot {
                    ns,
                    share: ns / total,
                    frame: st.frames[a].clone(),
                    leaf,
                    callers,
                }
            })
            .collect();
        causes.push(CauseCode { cat, ns: total, hot });
    }
    let idle = causes.iter().position(|c| c.cat == Cat::Idle);
    let idle = idle.map(|i| causes.remove(i));
    causes.sort_by(|a, b| b.ns.total_cmp(&a.ns));
    causes.extend(idle);
    CodeView {
        source: st.source,
        causes,
        notes: st.notes.clone(),
    }
}

fn folded_name(f: &Frame) -> String {
    let n = f.func.replace(';', ":");
    if f.kernel {
        format!("{n}_[k]")
    } else if f.func == "[unknown]" && !f.module.is_empty() {
        format!("[{}]", f.module)
    } else {
        n
    }
}

/// Folded stacks ("a;b;c 123"), one line per stack, rooted at the cause,
/// weighted in microseconds. Feed to flamegraph.pl, inferno or speedscope.
pub fn folded(st: &Stacks, with_idle: bool) -> String {
    let mut m: HashMap<String, f64> = HashMap::new();
    for s in &st.samples {
        if s.cat == Cat::Idle && !with_idle {
            continue;
        }
        let comm = st.comms.get(&s.pid).cloned().unwrap_or_default();
        let mut parts = vec![format!("[{}]", s.cat.key()), format!("{comm} ({})", s.pid)];
        parts.extend(s.frames.iter().rev().map(|&i| folded_name(&st.frames[i])));
        *m.entry(parts.join(";")).or_default() += s.ns;
    }
    let mut v: Vec<(String, f64)> = m.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.into_iter()
        .filter(|(_, ns)| *ns >= 1000.0)
        .map(|(k, ns)| format!("{k} {}", (ns / 1000.0).round() as u64))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(func: &str, module: &str) -> Frame {
        Frame {
            func: func.into(),
            module: module.into(),
            ..Frame::default()
        }
    }

    #[test]
    fn runtime_frames() {
        assert!(is_runtime(&f("read", "libc.so.6")));
        assert!(is_runtime(&f("std::io::read_to_end", "app")));
        assert!(is_runtime(&f("runtime.futex", "server")));
        assert!(is_runtime(&f("_PyEval_EvalFrameDefault", "python3.12")));
        assert!(!is_runtime(&f("app::db::query", "app")));
        assert!(!is_runtime(&f("main.handler", "server")));
        assert!(!is_runtime(&f("fib", "[jit]")));
    }

    #[test]
    fn summarize_anchors_on_program_code() {
        let mut st = Stacks::new("test");
        let read = st.intern(f("read", "libc.so.6"));
        let k = st.intern(Frame {
            func: "pipe_read".into(),
            module: "[kernel]".into(),
            kernel: true,
            ..Frame::default()
        });
        let q = st.intern(f("app::db::query", "app"));
        let h = st.intern(f("app::handler", "app"));
        let m = st.intern(f("main", "app"));
        let x = st.intern(f("app::compute", "app"));
        st.add(1, 1, Cat::Peer, vec![k, read, q, h, m], 3e9);
        st.add(1, 2, Cat::Peer, vec![k, read, h, m], 1e9);
        st.add(1, 1, Cat::Cpu, vec![x, m], 2e9);
        let cv = summarize(&st, 3);
        assert_eq!(cv.causes[0].cat, Cat::Peer);
        let top = &cv.causes[0].hot[0];
        assert_eq!(top.frame.func, "app::db::query");
        assert_eq!(top.leaf.as_ref().unwrap().func, "read");
        assert_eq!(top.callers[0].func, "app::handler");
        assert!((top.share - 0.75).abs() < 1e-9);
        assert_eq!(cv.get(Cat::Cpu).unwrap().hot[0].frame.func, "app::compute");
        let fo = folded(&st, false);
        assert!(fo.contains("[peer]; (1);main;app::handler;app::db::query;read;pipe_read_[k] 3000000"));
    }

    #[test]
    fn short_paths() {
        assert_eq!(short_path("src/main.rs"), "src/main.rs");
        assert_eq!(short_path("/home/u/proj/src/db/pool.rs"), "…/src/db/pool.rs");
    }
}
