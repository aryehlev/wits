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

/// A Python lock taken by `Condition.wait` (and so by `Event.wait`,
/// `Queue.get`, `Future.result`) or by joining a thread is a wait for
/// something to happen, not a fight over a lock. A semaphore's slots are
/// fought over. Needs the Python frames (`PYTHONPERFSUPPORT=1`).
pub fn python_event_wait(frames: &[Frame]) -> bool {
    let mut py = frames
        .iter()
        .filter(|f| f.module == "[python]")
        .map(|f| f.func.as_str());
    match py.next() {
        Some("Condition.wait") => py.next() != Some("Semaphore.acquire"),
        Some("Thread._wait_for_tstate_lock") => true,
        _ => false,
    }
}

/// Whose code a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The program's own code.
    App,
    /// A dependency: a crate, Go module, npm, Python or Ruby package, or a
    /// shared library.
    Lib,
    /// The language's standard library and runtime.
    Std,
    /// The C library, the dynamic loader and their kin.
    Sys,
    Kernel,
    Unknown,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Lib => "lib",
            Kind::Std => "std",
            Kind::Sys => "sys",
            Kind::Kernel => "kernel",
            Kind::Unknown => "unknown",
        }
    }
}

/// Whose code a frame is and, for a dependency, its package: from the
/// source path when there is one (Rust's /rustc/ and cargo registry, Go's
/// module cache, site-packages, node_modules, gems...), else from the name
/// and the binary it is in. Anything else is the program's own code.
pub fn origin(f: &Frame) -> (Kind, Option<String>) {
    if f.kernel {
        return (Kind::Kernel, None);
    }
    if f.func == "[unknown]" {
        return (Kind::Unknown, None);
    }
    if let Some(o) = f.file.as_deref().and_then(by_path) {
        return o;
    }
    if let Some(o) = by_name(&f.func, f.file.is_none()) {
        return o;
    }
    if let Some(o) = by_module(&f.module) {
        return o;
    }
    (Kind::App, None)
}

/// Code that belongs to the language runtime or the C library rather than
/// to the program or its dependencies.
pub fn is_runtime(f: &Frame) -> bool {
    !matches!(origin(f).0, Kind::App | Kind::Lib)
}

fn lib(name: &str) -> Option<(Kind, Option<String>)> {
    Some((Kind::Lib, (!name.is_empty()).then(|| name.to_string())))
}

/// "tokio-util-1.2.0" -> "tokio-util": up to the first '-' before a digit.
fn strip_version(s: &str) -> &str {
    s.match_indices('-')
        .find(|(i, _)| s[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .map(|(i, _)| &s[..i])
        .unwrap_or(s)
}

fn by_path(p: &str) -> Option<(Kind, Option<String>)> {
    let after = |m: &str| p.find(m).map(|i| &p[i + m.len()..]);
    let first = |s: &str| s.split('/').next().unwrap_or("").to_string();
    // Rust
    if p.starts_with("/rustc/") || p.contains("/rustlib/src/rust/library/") {
        return Some((Kind::Std, None));
    }
    if let Some(r) = after("/registry/src/") {
        // index.crates.io-<hash>/tokio-1.38.0/src/...
        return lib(strip_version(r.split('/').nth(1).unwrap_or("")));
    }
    if let Some(r) = after("/git/checkouts/") {
        let repo = first(r);
        return lib(repo.rsplit_once('-').map(|x| x.0).unwrap_or(&repo));
    }
    // Go: the module cache, then GOROOT
    if let Some(r) = after("/pkg/mod/") {
        let m = r.split('@').next().unwrap_or("");
        let mut out = String::new();
        let mut up = false;
        for c in m.chars() {
            match c {
                '!' => up = true,
                _ if up => {
                    out.push(c.to_ascii_uppercase());
                    up = false;
                }
                _ => out.push(c),
            }
        }
        return lib(&out);
    }
    for m in ["/go/src/", "/libexec/src/", "/goroot/src/"] {
        if let Some(r) = after(m) {
            if !first(r).contains('.') {
                return Some((Kind::Std, None));
            }
        }
    }
    if let Some(i) = p.find("/src/") {
        let before = p[..i].rsplit('/').next().unwrap_or("");
        if before.starts_with("go1.") || before.starts_with("go-1.") {
            return Some((Kind::Std, None));
        }
    }
    // Python, Node, Ruby: packages first, then the standard library
    if let Some(r) = after("/site-packages/").or_else(|| after("/dist-packages/")) {
        return lib(first(r).trim_end_matches(".py"));
    }
    if let Some(r) = p.rfind("node_modules/").map(|i| &p[i + 13..]) {
        let mut it = r.split('/');
        let a = it.next().unwrap_or("");
        return if a.starts_with('@') {
            lib(&format!("{a}/{}", it.next().unwrap_or("")))
        } else {
            lib(a)
        };
    }
    if let Some(r) = p.rfind("/gems/").map(|i| &p[i + 6..]) {
        let g = first(r);
        if g.contains('-') {
            return lib(strip_version(&g));
        }
    }
    if let Some(r) = after("/vendor/") {
        // Go (github.com/x/y), PHP (x/y) or cargo vendoring
        let parts: Vec<&str> = r.split('/').collect();
        let n = if parts.first().is_some_and(|x| x.contains('.')) { 3 } else { 1 };
        return lib(&parts[..n.min(parts.len().saturating_sub(1)).max(1)].join("/"));
    }
    if p.contains("/lib/python")
        || p.starts_with("<frozen")
        || p.starts_with("node:")
        || p.starts_with("internal/")
        || p.contains("/lib/ruby/")
        || p.starts_with("<internal:")
    {
        return Some((Kind::Std, None));
    }
    // C and C++
    if p.starts_with("/usr/include/c++/") || p.contains("/libstdc++") || p.contains("/libcxx/") {
        return Some((Kind::Std, None));
    }
    if p.starts_with("/usr/include/")
        || p.contains("/glibc-")
        || p.contains("/musl-")
        || p.starts_with("/usr/src/debug/glibc")
    {
        return Some((Kind::Sys, None));
    }
    None
}

/// The crate of a Rust (or C++) path: "tokio" for "tokio::net::...",
/// the type's for "<mio::X as std::io::Write>::f", the trait's for
/// "<F as core::future::Future>::poll".
fn crate_of(n: &str) -> Option<&str> {
    let s = if let Some(r) = n.strip_prefix('<') {
        let r = r
            .trim_start_matches('&')
            .trim_start_matches("mut ")
            .trim_start_matches("dyn ")
            .trim_start_matches("*const ")
            .trim_start_matches("*mut ");
        if r.starts_with('<') {
            return crate_of(r);
        }
        match r.find(" as ") {
            Some(i) if !r[..i].contains("::") => &r[i + 4..],
            _ => r,
        }
    } else {
        n
    };
    let (c, _) = s.split_once("::")?;
    (!c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')).then_some(c)
}

fn by_name(n: &str, no_file: bool) -> Option<(Kind, Option<String>)> {
    const STD: Option<(Kind, Option<String>)> = Some((Kind::Std, None));
    const SYS: Option<(Kind, Option<String>)> = Some((Kind::Sys, None));
    // Rust and C++
    if n.starts_with("__rust") || n.starts_with("rust_begin_unwind") || n.starts_with("rust_panic") {
        return STD;
    }
    if let Some(c) = crate_of(n) {
        const RT: &[&str] = &["std", "core", "alloc", "proc_macro", "panic_unwind", "panic_abort", "__gnu_cxx", "__cxxabiv1"];
        const LIBS: &[&str] = &[
            "tokio", "tokio_util", "tokio_stream", "mio", "futures", "futures_util", "futures_core",
            "futures_channel", "futures_executor", "hyper", "hyper_util", "h2", "http", "http_body",
            "tower", "tower_http", "tonic", "axum", "actix_web", "actix_rt", "actix_server",
            "async_std", "smol", "async_io", "async_executor", "tracing", "tracing_subscriber",
            "tracing_core", "log", "parking_lot", "parking_lot_core", "crossbeam", "crossbeam_channel",
            "crossbeam_epoch", "crossbeam_utils", "crossbeam_deque", "rayon", "rayon_core", "serde",
            "serde_json", "bytes", "prost", "reqwest", "rustls", "ring", "aws_lc_rs", "openssl",
            "sqlx", "diesel", "redis", "rdkafka", "regex", "regex_automata", "aho_corasick", "memchr",
            "hashbrown", "indexmap", "dashmap", "chrono", "uuid", "rand", "flate2", "miniz_oxide",
            "zstd", "warp", "rocket", "quinn", "socket2", "signal_hook", "boost", "absl", "folly",
            "grpc", "google", "tbb", "Eigen",
        ];
        if RT.contains(&c) {
            return STD;
        }
        if LIBS.contains(&c) {
            return lib(c);
        }
    }
    // Go's standard library: an import path whose first element is one of these
    if !n.contains("::") && !n.contains(' ') && n.contains('.') {
        const GO: &[&str] = &[
            "runtime", "syscall", "internal", "sync", "net", "os", "io", "bufio", "fmt", "strings",
            "bytes", "strconv", "encoding", "crypto", "reflect", "time", "sort", "math", "compress",
            "context", "errors", "unicode", "regexp", "hash", "path", "database", "log", "mime",
            "text", "html", "container", "slices", "maps", "iter", "unique", "weak", "vendor",
            "archive", "debug", "embed", "expvar", "flag", "image", "plugin", "testing",
        ];
        let head = n.split(['/', '.']).next().unwrap_or("");
        if GO.contains(&head) {
            return STD;
        }
    }
    // Java and the JVM, .NET
    let j = n.trim_start_matches('L').replace('/', ".");
    for p in ["java.", "javax.", "jdk.", "sun.", "com.sun.", "kotlin.", "kotlinx.", "scala.", "System.", "Microsoft."] {
        if j.starts_with(p) {
            return STD;
        }
    }
    for p in ["org.springframework.", "io.netty.", "org.apache.", "com.fasterxml.", "org.hibernate.", "io.grpc.", "com.google.", "io.micrometer.", "reactor."] {
        if j.starts_with(p) {
            return lib(&p.split('.').take(2).collect::<Vec<_>>().join("."));
        }
    }
    // Interpreters: CPython, V8 and Node, the JVM
    const RTP: &[&str] = &[
        "_Py", "Py", "method_vectorcall", "cfunction_", "slot_tp_", "builtin_", "Builtin:",
        "Stub:", "BytecodeHandler:", "v8::", "node::", "uv_", "uv__", "Interpreter",
        "StubRoutines",
    ];
    if RTP.iter().any(|p| n.starts_with(p)) {
        return STD;
    }
    // The C library and the loader
    const SYSP: &[&str] = &["__libc_", "__GI_", "_pthread_", "__pthread", "pthread_", "__syscall", "_dl_", "__memcpy", "__memset", "__memmove", "__strlen"];
    const SYSX: &[&str] = &["_start", "start_thread", "thread_start", "clone", "clone3", "__clone", "__clone3"];
    if SYSP.iter().any(|p| n.starts_with(p)) || SYSX.contains(&n) {
        return SYS;
    }
    // Statically linked C library functions have no file to tell by.
    if no_file {
        const LIBC: &[&str] = &[
            "memcpy", "memmove", "memset", "memcmp", "strlen", "strcmp", "malloc", "free", "calloc",
            "realloc", "read", "write", "readv", "writev", "pread64", "pwrite64", "send", "sendto",
            "sendmsg", "recv", "recvfrom", "recvmsg", "poll", "ppoll", "select", "epoll_wait",
            "epoll_pwait", "nanosleep", "clock_nanosleep", "clock_gettime", "fsync", "fdatasync",
            "open", "openat", "close", "mmap", "munmap", "accept", "accept4", "connect", "syscall",
            "futex", "sched_yield", "lseek", "ioctl", "fcntl", "getpid", "gettid", "abort",
        ];
        if LIBC.contains(&n) {
            return SYS;
        }
    }
    None
}

fn by_module(m: &str) -> Option<(Kind, Option<String>)> {
    if m.is_empty() {
        return None;
    }
    if m == "[vdso]" || m.starts_with("linux-vdso") {
        return Some((Kind::Sys, None));
    }
    if m.starts_with('[') {
        return None;
    }
    const SYS: &[&str] = &[
        "libc.so", "libc-", "libc.musl", "ld-musl", "libpthread", "ld-linux", "ld64.so", "libgcc_s",
        "libm.so", "libdl.so", "librt.so", "libsystem_", "libdyld", "dyld", "libobjc",
    ];
    if SYS.iter().any(|l| m.starts_with(l)) {
        return Some((Kind::Sys, None));
    }
    const RT: &[&str] = &[
        "libstdc++", "libc++", "libjvm", "libpython", "libruby", "libcoreclr", "libclrjit", "libuv",
        "libnode", "libv8", "libdispatch", "libgo.so", "libjli",
    ];
    if RT.iter().any(|l| m.starts_with(l)) {
        return Some((Kind::Std, None));
    }
    // An interpreter's own binary: its C code is the runtime.
    let base = m.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if ["python", "node", "java", "ruby", "perl", "php", "php-fpm", "dotnet", "beam.smp", "erl"].contains(&base) {
        return Some((Kind::Std, None));
    }
    if m.contains(".so") || m.ends_with(".dylib") || m.ends_with(".dll") || m.starts_with("lib") {
        let name = m.split(".so").next().unwrap_or(m).trim_end_matches(".dylib").trim_end_matches(".dll");
        return lib(name);
    }
    None
}

/// A short name for display: "TcpStream::write_vectored" for
/// "<&std::net::tcp::TcpStream as std::io::Write>::write_vectored",
/// "http.(*conn).serve" for "net/http.(*conn).serve".
pub fn short_func(n: &str) -> String {
    let mut s = n.to_string();
    // "<Type as Trait>::f" -> "Type::f"
    while s.starts_with('<') {
        let mut depth = 0;
        let mut end = None;
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(e) = end else { break };
        let inner = s[1..e].trim_start_matches('&').trim_start_matches("mut ").trim_start_matches("dyn ");
        let ty = match inner.find(" as ") {
            Some(i) if inner[..i].contains("::") || !inner.starts_with(char::is_uppercase) => &inner[..i],
            Some(i) => &inner[i + 4..],
            None => inner,
        };
        s = format!("{ty}{}", &s[e + 1..]);
    }
    // generic arguments
    let mut out = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    if !out.contains("::") {
        return out.rsplit('/').next().unwrap_or(&out).to_string();
    }
    let parts: Vec<&str> = out.split("::").filter(|p| !p.is_empty() && !p.starts_with("{{")).collect();
    parts[parts.len().saturating_sub(2)..].join("::")
}

/// The first frame from the leaf that is the program's own code; else the
/// first dependency; else the leaf.
fn anchor(st: &Stacks, kinds: &[Kind], frames: &[usize]) -> Option<usize> {
    let user: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| !st.frames[i].kernel)
        .collect();
    let find = |k: Kind| user.iter().copied().find(|&i| kinds[i] == k);
    find(Kind::App).or_else(|| find(Kind::Lib)).or(user.first().copied())
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
    /// What `frame` called on the way there, when that is not the leaf:
    /// the library function the program's code went into.
    pub via: Option<Frame>,
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
    let kinds: Vec<Kind> = st.frames.iter().map(|f| origin(f).0).collect();
    for cat in CAUSES.iter().copied().chain([Cat::Idle]) {
        let total = st.total(cat);
        if total <= 0.0 {
            continue;
        }
        // anchor frame -> (ns, heaviest sample index, its ns)
        let mut by: HashMap<Option<usize>, (f64, usize, f64)> = HashMap::new();
        for (si, s) in st.samples.iter().enumerate().filter(|x| x.1.cat == cat) {
            let a = anchor(st, &kinds, &s.frames);
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
                let via = (pos > 1).then(|| st.frames[user[pos - 1]].clone());
                // Callers in the program's own code, else in its dependencies.
                let above = |k: Kind| -> Vec<Frame> {
                    user.iter()
                        .skip(pos + 1)
                        .filter(|&&i| kinds[i] == k)
                        .take(3)
                        .map(|&i| st.frames[i].clone())
                        .collect()
                };
                let mut callers = above(Kind::App);
                if callers.is_empty() {
                    callers = above(Kind::Lib);
                }
                Hot {
                    ns,
                    share: ns / total,
                    frame: st.frames[a].clone(),
                    leaf,
                    via,
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

/// The frame that ends a stack cut short by `flame_json`.
pub const SMALLER: &str = "(smaller stacks)";

/// A flame graph's stacks within a budget: each cause's heaviest stacks
/// whole, the rest cut down to their callers until they fit, so that no
/// time is lost. Stacks are `(cause, pid, frames leaf first, ns)`; a cut
/// one ends in `None`.
fn fit_stacks(st: &Stacks, max: usize) -> Vec<(Cat, i32, Vec<Option<usize>>, f64)> {
    type Key<'a> = (Cat, i32, &'a [usize]);
    let mut agg: HashMap<Key, f64> = HashMap::new();
    for s in &st.samples {
        *agg.entry((s.cat, s.pid, s.frames.as_slice())).or_default() += s.ns;
    }
    let mut total: HashMap<Cat, f64> = HashMap::new();
    for (k, ns) in &agg {
        *total.entry(k.0).or_default() += ns;
    }
    // Heaviest first within its own cause, so that a big cause (idle,
    // often) does not take every place from a small one.
    let share = |k: &Key, ns: f64| ns / total[&k.0].max(1.0);
    let mut v: Vec<(Key, f64)> = agg.into_iter().collect();
    v.sort_by(|a, b| share(&b.0, b.1).total_cmp(&share(&a.0, a.1)));
    let whole = |v: &[(Key, f64)]| -> Vec<(Cat, i32, Vec<Option<usize>>, f64)> {
        v.iter()
            .map(|((c, p, fr), ns)| (*c, *p, fr.iter().map(|&i| Some(i)).collect(), *ns))
            .collect()
    };
    if v.len() <= max {
        return whole(&v);
    }
    let room = (max / 4).max(1);
    let keep = max.saturating_sub(room);
    let rest = &v[keep..];
    // Each stack cut to its root-most `d` frames.
    let cut = |d: usize| -> HashMap<(Cat, i32, &[usize]), f64> {
        let mut m: HashMap<(Cat, i32, &[usize]), f64> = HashMap::new();
        for ((c, p, fr), ns) in rest {
            *m.entry((*c, *p, &fr[fr.len().saturating_sub(d)..])).or_default() += ns;
        }
        m
    };
    // The deepest cut that fits.
    let deepest = rest.iter().map(|x| x.0 .2.len()).max().unwrap_or(0);
    let (mut lo, mut hi) = (0, deepest);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if cut(mid).len() <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut folded: Vec<_> = cut(lo).into_iter().collect();
    // Even cut to nothing, more processes than room: the lightest go.
    folded.sort_by(|a, b| b.1.total_cmp(&a.1));
    folded.truncate(room);
    let mut out = whole(&v[..keep]);
    out.extend(folded.into_iter().map(|((c, p, fr), ns)| {
        let mut f: Vec<Option<usize>> = vec![None];
        f.extend(fr.iter().map(|&i| Some(i)));
        (c, p, f, ns)
    }));
    out
}

/// Every stack, for a flame graph, as JSON: each frame once
/// (`[func, module, file, line, kernel, kind, package]`), each stack as
/// `[cause, pid, microseconds, [frame indices from the root]]`, and the
/// process names. At most `max` stacks: the lighter ones end early, in a
/// `SMALLER` frame that stands for what they called.
pub fn flame_json(st: &Stacks, max: usize) -> String {
    use crate::out::esc;
    let mut v = fit_stacks(st, max);
    v.retain(|x| x.3 >= 1000.0);
    v.sort_by(|a, b| b.3.total_cmp(&a.3));
    let mut ids: HashMap<Option<usize>, usize> = HashMap::new();
    let mut used: Vec<Option<usize>> = Vec::new();
    let stacks: Vec<String> = v
        .iter()
        .map(|(c, pid, fr, ns)| {
            let f: Vec<String> = fr
                .iter()
                .rev()
                .map(|&i| {
                    let id = *ids.entry(i).or_insert_with(|| {
                        used.push(i);
                        used.len() - 1
                    });
                    id.to_string()
                })
                .collect();
            format!("[{},{pid},{},[{}]]", esc(c.key()), (ns / 1000.0).round() as u64, f.join(","))
        })
        .collect();
    let frames: Vec<String> = used
        .iter()
        .map(|i| {
            let Some(i) = *i else {
                return format!("[{},\"\",null,null,false,\"unknown\",null]", esc(SMALLER));
            };
            let f = &st.frames[i];
            let (kind, pkg) = origin(f);
            format!(
                "[{},{},{},{},{},{},{}]",
                esc(&f.func),
                esc(&f.module),
                f.file.as_deref().map(esc).unwrap_or_else(|| "null".into()),
                f.line.map(|l| l.to_string()).unwrap_or_else(|| "null".into()),
                f.kernel,
                esc(kind.key()),
                pkg.as_deref().map(esc).unwrap_or_else(|| "null".into())
            )
        })
        .collect();
    let mut pids: Vec<i32> = v.iter().map(|x| x.1).collect();
    pids.sort_unstable();
    pids.dedup();
    let procs: Vec<String> = pids
        .iter()
        .map(|p| format!("\"{p}\":{}", esc(st.comms.get(p).map(String::as_str).unwrap_or(""))))
        .collect();
    format!(
        "{{\"source\":{},\"frames\":[{}],\"stacks\":[{}],\"procs\":{{{}}}}}",
        esc(st.source),
        frames.join(","),
        stacks.join(","),
        procs.join(",")
    )
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

    fn ff(func: &str, module: &str, file: Option<&str>) -> Frame {
        Frame {
            func: func.into(),
            module: module.into(),
            file: file.map(Into::into),
            ..Frame::default()
        }
    }

    #[test]
    fn whose_code() {
        let k = |func: &str, module: &str, file: Option<&str>| {
            let (k, p) = origin(&ff(func, module, file));
            format!("{}{}", k.key(), p.map(|p| format!(":{p}")).unwrap_or_default())
        };
        // Rust: std by path or name, dependencies by registry path or name
        assert_eq!(k("<&std::net::tcp::TcpStream as std::io::Write>::write_vectored", "svc", Some("/rustc/9b00956e/library/std/src/net/tcp.rs")), "std");
        assert_eq!(k("<&std::net::tcp::TcpStream as std::io::Write>::write_vectored", "svc", None), "std");
        assert_eq!(k("<&mio::net::tcp::stream::TcpStream as std::io::Write>::write_vectored", "svc", Some("/home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/mio-0.8.11/src/net/tcp/stream.rs")), "lib:mio");
        assert_eq!(k("tokio_util::codec::framed_impl::poll_next", "svc", Some("/usr/local/cargo/registry/src/index.crates.io-6f17d22bba15001f/tokio-util-0.7.10/src/codec/framed_impl.rs")), "lib:tokio-util");
        assert_eq!(k("tokio::runtime::task::raw::poll", "svc", None), "lib:tokio");
        assert_eq!(k("<F as core::future::future::Future>::poll", "svc", None), "std");
        assert_eq!(k("filtration::pipeline::run::{{closure}}", "svc", Some("/src/filtration/src/pipeline.rs")), "app");
        assert_eq!(k("filtration::pipeline::run", "svc", None), "app");
        // Go
        assert_eq!(k("net/http.(*conn).serve", "server", Some("/usr/local/go/src/net/http/server.go")), "std");
        assert_eq!(k("runtime.mallocgc", "server", None), "std");
        assert_eq!(k("github.com/jackc/pgx/v5.(*Conn).Query", "server", Some("/root/go/pkg/mod/github.com/jackc/pgx/v5@v5.5.0/conn.go")), "lib:github.com/jackc/pgx/v5");
        assert_eq!(k("main.handler", "server", Some("/src/app/main.go")), "app");
        // Python, Node, Ruby
        assert_eq!(k("get", "[python]", Some("/app/.venv/lib/python3.12/site-packages/requests/api.py")), "lib:requests");
        assert_eq!(k("loads", "[python]", Some("/usr/lib/python3.12/json/__init__.py")), "std");
        assert_eq!(k("handler", "[python]", Some("/app/server.py")), "app");
        assert_eq!(k("handle", "[js]", Some("/app/node_modules/@fastify/router/index.js")), "lib:@fastify/router");
        assert_eq!(k("processTicksAndRejections", "[js]", Some("node:internal/process/task_queues")), "std");
        assert_eq!(k("call", "[ruby]", Some("/usr/lib/ruby/gems/3.2.0/gems/rack-3.0.8/lib/rack/builder.rb")), "lib:rack");
        // C and C++, the JVM
        assert_eq!(k("std::vector<int>::push_back", "app", None), "std");
        assert_eq!(k("writev", "libc.so.6", None), "sys");
        assert_eq!(k("writev", "svc", None), "sys");
        assert_eq!(k("SSL_write", "libssl.so.3", None), "lib:libssl");
        assert_eq!(k("flush_log", "where", Some("/home/u/wits/cases/where.c")), "app");
        assert_eq!(k("java.util.HashMap::get", "[jit]", None), "std");
        assert_eq!(k("Lio/netty/channel/nio/NioEventLoop;::run", "[jit]", None), "lib:io.netty");
    }

    #[test]
    fn short_names() {
        assert_eq!(short_func("<&std::net::tcp::TcpStream as std::io::Write>::write_vectored"), "TcpStream::write_vectored");
        assert_eq!(short_func("<F as core::future::future::Future>::poll"), "Future::poll");
        assert_eq!(short_func("tokio::runtime::task::harness::Harness<T,S>::poll"), "Harness::poll");
        assert_eq!(short_func("app::run::{{closure}}"), "app::run");
        assert_eq!(short_func("net/http.(*conn).serve"), "http.(*conn).serve");
        assert_eq!(short_func("writev"), "writev");
    }

    #[test]
    fn anchors_on_own_code_past_dependencies() {
        let mut st = Stacks::new("test");
        let w = st.intern(ff("writev", "libc.so.6", None));
        let s = st.intern(ff("<&std::net::tcp::TcpStream as std::io::Write>::write_vectored", "svc", Some("/rustc/x/library/std/src/net/tcp.rs")));
        let m = st.intern(ff("<&mio::net::tcp::stream::TcpStream as std::io::Write>::write_vectored", "svc", Some("/r/.cargo/registry/src/i/mio-0.8.11/src/net/tcp/stream.rs")));
        let t = st.intern(ff("tokio::io::poll_evented::PollEvented<E>::poll_write", "svc", None));
        let a = st.intern(ff("svc::conn::flush", "svc", Some("/src/svc/src/conn.rs")));
        let r = st.intern(ff("tokio::runtime::task::raw::poll", "svc", None));
        st.add(1, 1, Cat::Cpu, vec![w, s, m, t, a, r], 1e9);
        let cv = summarize(&st, 3);
        let h = &cv.get(Cat::Cpu).unwrap().hot[0];
        assert_eq!(h.frame.func, "svc::conn::flush");
        assert_eq!(h.leaf.as_ref().unwrap().func, "writev");
        assert_eq!(h.via.as_ref().unwrap().func, "tokio::io::poll_evented::PollEvented<E>::poll_write");
        assert!(crate::out::where_text(h, Cat::Cpu).starts_with("100% in writev via PollEvented::poll_write, called from svc::conn::flush"));
    }

    #[test]
    fn flame_fits_without_losing_time() {
        let mut st = Stacks::new("ebpf");
        let root = st.intern(f("main", "app"));
        let mid: Vec<usize> = (0..40).map(|i| st.intern(f(&format!("mid{i}"), "app"))).collect();
        let leaf: Vec<usize> = (0..50).map(|i| st.intern(f(&format!("leaf{i}"), "app"))).collect();
        // 2000 small CPU stacks and one huge idle one.
        for (i, &m) in mid.iter().enumerate() {
            for (j, &l) in leaf.iter().enumerate() {
                st.add(1, 1, Cat::Cpu, vec![l, m, root], 1e6 + (i * 50 + j) as f64);
            }
        }
        st.add(1, 2, Cat::Idle, vec![root], 1e12);
        let want: f64 = st.samples.iter().filter(|s| s.cat == Cat::Cpu).map(|s| s.ns).sum();
        let v = fit_stacks(&st, 400);
        assert!(v.len() <= 400);
        let got: f64 = v.iter().filter(|x| x.0 == Cat::Cpu).map(|x| x.3).sum();
        assert!((got - want).abs() < 1.0, "{got} vs {want}");
        // The idle stack is not given every place, and is still there.
        assert_eq!(v.iter().filter(|x| x.0 == Cat::Idle).count(), 1);
        // The heaviest CPU stack is whole; cut ones end in SMALLER and keep
        // their callers.
        assert!(v.iter().any(|x| x.2 == vec![Some(leaf[49]), Some(mid[39]), Some(root)]));
        let cut: Vec<_> = v.iter().filter(|x| x.2[0].is_none()).collect();
        assert!(!cut.is_empty());
        assert!(cut.iter().all(|x| x.2.last() == Some(&Some(root))));
        let j = flame_json(&st, 400);
        assert!(j.contains(SMALLER));
        // Nothing to cut: every stack whole.
        assert!(fit_stacks(&st, 5000).iter().all(|x| x.2[0].is_some()));
    }

    #[test]
    fn python_waits_for_events() {
        let py = |n: &str| Frame { func: n.into(), module: "[python]".into(), ..Frame::default() };
        let c = |n: &str| f(n, "python3.13");
        let lock = c("lock_PyThread_acquire_lock");
        // Event.wait in a test server: waiting for a signal.
        assert!(python_event_wait(&[lock.clone(), py("Condition.wait"), py("Event.wait"), py("Server.run")]));
        assert!(python_event_wait(&[lock.clone(), py("Thread._wait_for_tstate_lock"), py("Thread.join")]));
        // A semaphore's slots, and `with self._lock:`, are fought over.
        assert!(!python_event_wait(&[lock.clone(), py("Condition.wait"), py("Semaphore.acquire")]));
        assert!(!python_event_wait(&[lock.clone(), py("Pool.get_conn")]));
        // No Python frames: cannot tell.
        assert!(!python_event_wait(&[lock]));
    }

    #[test]
    fn short_paths() {
        assert_eq!(short_path("src/main.rs"), "src/main.rs");
        assert_eq!(short_path("/home/u/proj/src/db/pool.rs"), "…/src/db/pool.rs");
    }
}
