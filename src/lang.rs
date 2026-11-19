//! Languages. A runtime is recognised from the files a process has mapped:
//! wits says how to make it publish its function names when it does not, and
//! `wits run` sets the environment so that it does. The language of a single
//! frame is recognised from its name, file and module: that says which guide
//! applies (guide.rs) and which costs to look for in the stacks (hint.rs).

use crate::code::{Frame, GO_STD};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    Python(u32, u32),
    Node,
    Jvm,
    Dotnet,
    Ruby,
    Beam,
}

impl Runtime {
    pub fn name(self) -> String {
        match self {
            Runtime::Python(a, b) if a > 0 => format!("Python {a}.{b}"),
            Runtime::Python(..) => "Python".into(),
            Runtime::Node => "Node.js".into(),
            Runtime::Jvm => "the JVM".into(),
            Runtime::Dotnet => ".NET".into(),
            Runtime::Ruby => "Ruby".into(),
            Runtime::Beam => "Erlang/Elixir".into(),
        }
    }
}

fn python_version(s: &str) -> Option<(u32, u32)> {
    let i = s.find("python3.")?;
    let rest = &s[i + 8..];
    let minor: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((3, minor.parse().ok()?))
}

/// Which runtime, if any, from the executable and mapped file paths.
pub fn detect<'a>(exe: &'a str, paths: impl Iterator<Item = &'a str>) -> Option<Runtime> {
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let exe_b = base(exe);
    let mut found = None;
    for p in std::iter::once(exe).chain(paths) {
        let b = base(p);
        if b.starts_with("libpython3") || b.starts_with("python3") || b == "python" {
            let (a, m) = python_version(&b).unwrap_or((0, 0));
            return Some(Runtime::Python(a, m));
        }
        if b.starts_with("libjvm") {
            return Some(Runtime::Jvm);
        }
        if b.starts_with("libcoreclr") {
            return Some(Runtime::Dotnet);
        }
        if b.starts_with("libruby") {
            found = Some(Runtime::Ruby);
        }
        if b == "beam.smp" {
            return Some(Runtime::Beam);
        }
    }
    if exe_b == "node" || exe_b == "nodejs" {
        return Some(Runtime::Node);
    }
    if exe_b.starts_with("ruby") {
        return Some(Runtime::Ruby);
    }
    found
}

/// How to see this runtime's own functions, when they are not visible.
/// `in_run`: inside `wits run`, which already sets what it can.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by the Linux backends
pub fn hint(rt: Runtime, pid: i32, in_run: bool) -> String {
    let how = match rt {
        Runtime::Python(3, m) if m >= 12 => {
            "start it with PYTHONPERFSUPPORT=1 (or python -X perf)".to_string()
        }
        Runtime::Python(..) => "Python 3.12 or later can show them (PYTHONPERFSUPPORT=1)".to_string(),
        Runtime::Node => "start it with node --perf-basic-prof".to_string(),
        Runtime::Jvm => format!(
            "run `jcmd {pid} Compiler.perfmap` (JDK 17+); start it with -XX:+PreserveFramePointer for whole stacks"
        ),
        Runtime::Dotnet => "start it with DOTNET_PerfMapEnabled=1".to_string(),
        Runtime::Ruby => "start it with ruby --yjit --yjit-perf (Ruby 3.3+)".to_string(),
        Runtime::Beam => "start it with ERL_FLAGS='+JPperf true'".to_string(),
    };
    let auto = matches!(rt, Runtime::Python(3, 12..) | Runtime::Node | Runtime::Dotnet | Runtime::Beam);
    format!(
        "{} functions are hidden inside the interpreter or JIT: {how}{}",
        rt.name(),
        if auto && !in_run { "; `wits run` sets this for you" } else { "" }
    )
}

/// Environment for `wits run`: ask every runtime to publish its symbols.
/// Each is ignored by programs that do not know it.
pub fn run_env() -> Vec<(String, String)> {
    let add = |key: &str, flag: &str| -> (String, String) {
        let cur = std::env::var(key).unwrap_or_default();
        if cur.split_whitespace().any(|f| f == flag) {
            (key.to_string(), cur)
        } else if cur.is_empty() {
            (key.to_string(), flag.to_string())
        } else {
            (key.to_string(), format!("{cur} {flag}"))
        }
    };
    vec![
        ("PYTHONPERFSUPPORT".into(), "1".into()),
        add("NODE_OPTIONS", "--perf-basic-prof"),
        ("DOTNET_PerfMapEnabled".into(), "1".into()),
        add("ERL_FLAGS", "+JPperf true"),
    ]
}

/// The languages wits knows the usual costs of. Each has a guide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Python,
    Node,
    Go,
    Rust,
    Jvm,
    Dotnet,
    Ruby,
    Beam,
    /// C and C++.
    C,
}

impl Lang {
    #[cfg(test)]
    pub const ALL: [Lang; 9] = [
        Lang::Python,
        Lang::Node,
        Lang::Go,
        Lang::Rust,
        Lang::Jvm,
        Lang::Dotnet,
        Lang::Ruby,
        Lang::Beam,
        Lang::C,
    ];

    /// The name in JSON, and of the guide.
    pub fn key(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Node => "node",
            Lang::Go => "go",
            Lang::Rust => "rust",
            Lang::Jvm => "jvm",
            Lang::Dotnet => "dotnet",
            Lang::Ruby => "ruby",
            Lang::Beam => "beam",
            Lang::C => "c",
        }
    }
}

/// The language a frame's code was written in, when its file, module or
/// name says so. A plain C function says nothing: `None`.
pub fn of(f: &Frame) -> Option<Lang> {
    match f.module.as_str() {
        "[python]" => return Some(Lang::Python),
        "[js]" => return Some(Lang::Node),
        "[ruby]" => return Some(Lang::Ruby),
        _ => {}
    }
    // A runtime's own binary first: CPython is written in C, but what it
    // does is Python's work.
    let jit = f.module == "[jit]";
    by_module(&f.module)
        .or_else(|| f.file.as_deref().and_then(by_file))
        .or_else(|| by_func(&f.func, jit))
}

fn by_file(p: &str) -> Option<Lang> {
    if p.starts_with("node:") {
        return Some(Lang::Node);
    }
    if p.starts_with("<frozen") {
        return Some(Lang::Python);
    }
    if p.starts_with("<internal:") {
        return Some(Lang::Ruby);
    }
    let (_, ext) = p.rsplit('/').next()?.rsplit_once('.')?;
    Some(match ext {
        "py" | "pyx" => Lang::Python,
        "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx" => Lang::Node,
        "go" => Lang::Go,
        "rs" => Lang::Rust,
        "java" | "kt" | "kts" | "scala" | "groovy" | "clj" => Lang::Jvm,
        "cs" | "fs" | "vb" => Lang::Dotnet,
        "rb" => Lang::Ruby,
        "ex" | "exs" | "erl" | "hrl" => Lang::Beam,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ipp" | "inl" | "tcc" => Lang::C,
        _ => return None,
    })
}

/// A runtime's own binary or library: the work is that runtime's.
fn by_module(m: &str) -> Option<Lang> {
    let base = m.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let starts = |l: &[&str]| l.iter().any(|p| m.starts_with(p));
    if m.starts_with("libpython") || base == "python" || m == "Python" {
        Some(Lang::Python)
    } else if base == "node" || base == "nodejs" || m.starts_with("libnode") {
        Some(Lang::Node)
    } else if starts(&["libjvm", "libjava", "libjli"]) || base == "java" {
        Some(Lang::Jvm)
    } else if starts(&["libcoreclr", "libclrjit", "libclrgc", "libhostfxr", "libhostpolicy"]) || base == "dotnet" {
        Some(Lang::Dotnet)
    } else if m.starts_with("libruby") || base == "ruby" {
        Some(Lang::Ruby)
    } else if m.starts_with("beam.") || base == "erl" || base == "erlexec" {
        Some(Lang::Beam)
    } else {
        None
    }
}

/// Whether `root` ("core::") starts a path in `n`, not ends another name
/// ("mycore::") or continues one ("x::core::").
fn has_root(n: &str, root: &str) -> bool {
    n.match_indices(root).any(|(i, _)| {
        !n[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c == ':' || c == '_' || c.is_alphanumeric())
    })
}

fn by_func(n: &str, jit: bool) -> Option<Lang> {
    // CPython's C API
    let py_api = n
        .strip_prefix("Py")
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_uppercase()) && r.contains('_'));
    if n.starts_with("_Py") || py_api {
        return Some(Lang::Python);
    }
    if n.contains("v8::") || n.starts_with("node::") || n.starts_with("Builtins_") {
        return Some(Lang::Node);
    }
    if n.starts_with("rb_vm_") || n == "vm_exec_core" || n.starts_with("rb_funcall") {
        return Some(Lang::Ruby);
    }
    if jit {
        // Names from a perf map: the JVM's, .NET's, the BEAM's.
        let head = params_at(n).map(|i| &n[..i]).unwrap_or(n);
        let j = head.rsplit(' ').next().unwrap_or(head).trim_start_matches('L').replace('/', ".");
        const JVM: &[&str] = &["java.", "javax.", "jdk.", "sun.", "kotlin.", "kotlinx.", "scala.", "clojure."];
        const JVM_STUBS: &[&str] = &["Interpreter", "StubRoutines", "vtable stub", "itable stub", "call_stub"];
        if JVM.iter().any(|p| j.starts_with(p)) || JVM_STUBS.iter().any(|p| n.starts_with(p)) {
            return Some(Lang::Jvm);
        }
        const NET: &[&str] = &["[System.", "[Microsoft.", "[Optimized", "[Tier", "[QuickJitted", "[PreJIT", "[MinOpt", "[Instrumented"];
        if NET.iter().any(|p| n.contains(p)) {
            return Some(Lang::Dotnet);
        }
        if n.starts_with('$') {
            return Some(Lang::Beam);
        }
        // V8's code that is not a JavaScript function
        const V8: &[&str] = &["RegExp:", "Builtin:", "BytecodeHandler:", "Stub:", "Handler:"];
        if V8.iter().any(|p| n.starts_with(p)) {
            return Some(Lang::Node);
        }
        return None;
    }
    // Rust: syntax C++ does not have, or a path into its standard library.
    // A demangled C++ function has a parameter list; a Rust one never does.
    const RUST_ONLY: &[&str] = &["__rust_", "__rdl_", "{{closure}}", "{closure#", "<impl ", "$LT$", "$u20$"];
    const RUST: &[&str] = &[
        "core::", "alloc::", "hashbrown::", "std::rt::", "std::sys::", "std::sys_common::", "std::io::",
        "std::sync::", "std::thread::", "std::collections::", "std::panicking::", "std::panic::", "std::fs::",
        "std::net::", "std::process::", "std::time::", "std::env::", "std::os::", "std::ffi::", "std::path::",
        "std::backtrace", "std::alloc::", "std::hash::", "std::num::", "std::error::", "std::f32::", "std::f64::",
    ];
    let params = params_at(n).is_some();
    if RUST_ONLY.iter().any(|p| n.contains(p)) || n.starts_with('<') && n.contains(" as ") || rust_hash(n).is_some() {
        return Some(Lang::Rust);
    }
    if !params && RUST.iter().any(|r| has_root(n, r)) {
        return Some(Lang::Rust);
    }
    // C++: its standard library and ABI, or a parameter list.
    const CXX: &[&str] = &[
        "__cxa_", "__gxx_", "__gnu_cxx::", "__gnu_debug::", "__cxxabiv1::", "__dynamic_cast", "operator new",
        "operator delete",
    ];
    if has_root(n, "std::") || CXX.iter().any(|p| n.starts_with(p)) {
        return Some(Lang::C);
    }
    if n.contains("::") {
        return params.then_some(Lang::C);
    }
    // Go: "import/path.Name". A dot also ends the name of a part of a C
    // function the compiler split off ("main.cold", "sort.constprop.0").
    const SPLIT: &[&str] = &["cold", "part", "isra", "constprop", "lto_priv", "llvm", "localalias"];
    if n.contains(' ') || !n.contains('.') || n.split('.').skip(1).any(|s| SPLIT.contains(&s)) {
        return None;
    }
    let head = n.split(['/', '.']).next().unwrap_or("");
    let domain = n.split('/').next().filter(|_| n.contains('/')).is_some_and(|d| d.contains('.'));
    (GO_STD.contains(&head) || domain || n.starts_with("main.") || n.contains(".(*")).then_some(Lang::Go)
}

/// Where the parameter list of a demangled C++ (or perf-map) name opens:
/// the first "(" outside template arguments that is not "operator()", a
/// lambda's own list or "(anonymous namespace)". `None` for names without
/// one, and for Go's "pkg.(*T).Method".
fn params_at(n: &str) -> Option<usize> {
    const OPS: &[u8] = b"<>=!+-*/%^&|~,[]";
    let b = n.as_bytes();
    let (mut angle, mut brace) = (0usize, 0usize);
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'o' && n[i..].starts_with("operator") && !n[..i].ends_with(|c: char| c == '_' || c.is_alphanumeric()) {
            i += "operator".len();
            if n[i..].starts_with("()") {
                i += 2;
            } else {
                while i < b.len() && OPS.contains(&b[i]) {
                    i += 1;
                }
            }
            continue;
        }
        match b[i] {
            b'<' => angle += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' => angle = angle.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b'(' if angle == 0 && brace == 0 => {
                if n[i..].starts_with("(anonymous") {
                    i += n[i..].find(')').unwrap_or(0);
                } else if i == 0 || b[i - 1] == b'.' {
                    return None;
                } else {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A Rust name without the hash rustc ends it with ("::h" and 16 hex
/// digits), when it has one.
fn rust_hash(n: &str) -> Option<&str> {
    let (name, h) = n.rsplit_once("::h")?;
    (h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(name)
}

/// A Rust name as a C++ demangler leaves it (macOS's `sample` does): the
/// path is split, but its parts keep their escapes and the hash stays.
/// "_$LT$impl$u20$core..fmt..Debug$u20$for$u20$T$GT$::fmt::h0123456789abcdef"
/// is "<impl core::fmt::Debug for T>::fmt".
fn rust_legacy(n: &str) -> Option<String> {
    let hashed = rust_hash(n);
    if hashed.is_none() && !n.contains("$LT$") && !n.contains("$u20$") {
        return None;
    }
    let mut rest = hashed.unwrap_or(n);
    let mut out = String::with_capacity(rest.len());
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("..") {
            out.push_str("::");
            rest = r;
            continue;
        }
        let escape = rest.strip_prefix('$').and_then(|r| r.split_once('$')).and_then(|(code, r)| {
            let c = match code {
                "LT" => '<',
                "GT" => '>',
                "LP" => '(',
                "RP" => ')',
                "C" => ',',
                "SP" => '@',
                "BP" => '*',
                "RF" => '&',
                _ => char::from_u32(u32::from_str_radix(code.strip_prefix('u')?, 16).ok()?)?,
            };
            Some((c, r))
        });
        match escape {
            Some((c, r)) => {
                // "_$LT$": the underscore only keeps the part a valid name.
                if out == "_" || out.ends_with("::_") {
                    out.pop();
                }
                out.push(c);
                rest = r;
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Some(out)
}

/// A function's qualified name alone, to recognise it by: a demangled C++,
/// JVM or .NET name without its return type, parameters and inline
/// namespaces ("std::__1::" is "std::"), a Rust name without its hash.
/// Other names are returned as they are.
pub fn bare(f: &Frame) -> Cow<'_, str> {
    let n = f.func.as_str();
    if matches!(f.module.as_str(), "[python]" | "[js]" | "[ruby]") {
        return Cow::Borrowed(n);
    }
    if let Some(rust) = rust_legacy(n) {
        return Cow::Owned(rust);
    }
    let mut name = n;
    if let Some(cut) = params_at(n) {
        let head = n[..cut].trim_end();
        // The return type ends at the last space outside template
        // arguments and "(anonymous namespace)", before any "operator".
        let upto = head.rfind("operator").unwrap_or(head.len());
        let (mut angle, mut paren) = (0usize, 0usize);
        let mut start = 0;
        for (i, c) in head[..upto].char_indices() {
            match c {
                '<' => angle += 1,
                '>' => angle = angle.saturating_sub(1),
                '(' => paren += 1,
                ')' => paren = paren.saturating_sub(1),
                ' ' if angle == 0 && paren == 0 => start = i + 1,
                _ => {}
            }
        }
        name = &head[start..];
    }
    const INLINE: &[&str] = &["std::__1::", "std::__cxx11::", "std::__ndk1::"];
    if INLINE.iter().any(|p| name.contains(p)) {
        let mut s = name.to_string();
        for p in INLINE {
            s = s.replace(p, "std::");
        }
        return Cow::Owned(s);
    }
    Cow::Borrowed(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_runtimes() {
        assert_eq!(
            detect("/usr/bin/python3.12", std::iter::empty()),
            Some(Runtime::Python(3, 12))
        );
        assert_eq!(
            detect("/usr/bin/uwsgi", ["/usr/lib/libpython3.11.so.1.0"].into_iter()),
            Some(Runtime::Python(3, 11))
        );
        assert_eq!(detect("/usr/bin/node", std::iter::empty()), Some(Runtime::Node));
        assert_eq!(
            detect("/opt/java/bin/java", ["/opt/java/lib/server/libjvm.so"].into_iter()),
            Some(Runtime::Jvm)
        );
        assert_eq!(detect("/usr/bin/app", ["/lib/libc.so.6"].into_iter()), None);
    }

    #[test]
    fn hints_name_the_switch() {
        assert!(hint(Runtime::Python(3, 12), 1, false).contains("PYTHONPERFSUPPORT=1"));
        assert!(hint(Runtime::Node, 1, false).contains("--perf-basic-prof"));
        assert!(hint(Runtime::Jvm, 42, false).contains("jcmd 42"));
    }

    fn fr(func: &str, module: &str, file: Option<&str>) -> Frame {
        Frame {
            func: func.into(),
            module: module.into(),
            file: file.map(str::to_string),
            ..Frame::default()
        }
    }

    #[test]
    fn frame_languages() {
        let l = |func: &str, module: &str, file: Option<&str>| of(&fr(func, module, file));
        // by file
        assert_eq!(l("handler", "[python]", Some("/app/server.py")), Some(Lang::Python));
        assert_eq!(l("handler", "server", Some("/src/app/main.go")), Some(Lang::Go));
        assert_eq!(l("svc::conn::flush", "svc", Some("/src/svc/src/conn.rs")), Some(Lang::Rust));
        assert_eq!(l("flush_log", "server", Some("/srv/app/log.c")), Some(Lang::C));
        assert_eq!(l("processTicksAndRejections", "[js]", Some("node:internal/process/task_queues")), Some(Lang::Node));
        // by the runtime's own binary
        assert_eq!(l("_PyEval_EvalFrameDefault", "python3.12", None), Some(Lang::Python));
        assert_eq!(l("thread_native_entry(Thread*)", "libjvm.so", None), Some(Lang::Jvm));
        assert_eq!(l("process_main", "beam.smp", None), Some(Lang::Beam));
        assert_eq!(l("WKS::gc_heap::mark_phase", "libcoreclr.so", None), Some(Lang::Dotnet));
        // by name
        assert_eq!(l("runtime.mallocgc", "server", None), Some(Lang::Go));
        assert_eq!(l("net/http.(*conn).serve", "server", None), Some(Lang::Go));
        assert_eq!(l("github.com/jackc/pgx/v5.(*Conn).Query", "server", None), Some(Lang::Go));
        assert_eq!(l("main.handler", "server", None), Some(Lang::Go));
        assert_eq!(l("main.cold", "app", None), None);
        assert_eq!(l("sort.constprop.0", "app", None), None);
        assert_eq!(l("<alloc::vec::Vec<T> as core::clone::Clone>::clone", "svc", None), Some(Lang::Rust));
        assert_eq!(l("core::ptr::drop_in_place<alloc::string::String>", "svc", None), Some(Lang::Rust));
        assert_eq!(l("std::rt::lang_start::{{closure}}", "svc", None), Some(Lang::Rust));
        assert_eq!(l("__rustc::__rust_alloc", "svc", None), Some(Lang::Rust));
        assert_eq!(l("std::vector<int, std::allocator<int> >::push_back(int const&)", "app", None), Some(Lang::C));
        assert_eq!(l("game::core::Engine::step(double)", "app", None), Some(Lang::C));
        assert_eq!(l("operator new(unsigned long)", "libstdc++.so.6", None), Some(Lang::C));
        assert_eq!(l("v8::internal::Heap::CollectGarbage(v8::internal::AllocationSpace)", "node", None), Some(Lang::Node));
        assert_eq!(l("java.lang.Object java.util.HashMap.get(java.lang.Object)", "[jit]", None), Some(Lang::Jvm));
        assert_eq!(l("Interpreter", "[jit]", None), Some(Lang::Jvm));
        assert_eq!(l("void [System.Private.CoreLib] System.Threading.Thread::StartCallback()[Optimized]", "[jit]", None), Some(Lang::Dotnet));
        assert_eq!(l("$lists:foldl/3", "[jit]", None), Some(Lang::Beam));
        // nothing to tell by
        assert_eq!(l("inner_loop", "work", None), None);
        assert_eq!(l("malloc", "libc.so.6", None), None);
        assert_eq!(l("app::db::query", "app", None), None);
    }

    #[test]
    fn rust_names_from_a_cxx_demangler() {
        let b = |n: &str| bare(&fr(n, "app", None)).into_owned();
        assert_eq!(b("a::main::h9bd9cd9c9e348f97"), "a::main");
        assert_eq!(
            b("_$LT$std..hash..random..DefaultHasher$u20$as$u20$core..hash..Hasher$GT$::finish::h18af9158f137a963"),
            "<std::hash::random::DefaultHasher as core::hash::Hasher>::finish"
        );
        assert_eq!(
            b("core::fmt::num::imp::_$LT$impl$u20$u32$GT$::_fmt_inner::h9d2b6267034f09d1"),
            "core::fmt::num::imp::<impl u32>::_fmt_inner"
        );
        assert_eq!(b("a::main::_$u7b$$u7b$closure$u7d$$u7d$::h0123456789abcdef"), "a::main::{{closure}}");
        assert_eq!(
            b("_$LT$alloc..vec..Vec$LT$T$C$A$GT$$u20$as$u20$core..ops..drop..Drop$GT$::drop::h0123456789abcdef"),
            "<alloc::vec::Vec<T,A> as core::ops::drop::Drop>::drop"
        );
        // Not Rust: a price, a hash that is not one.
        assert_eq!(b("pay$5"), "pay$5");
        assert_eq!(b("ns::hello"), "ns::hello");
        assert_eq!(of(&fr("a::main::h9bd9cd9c9e348f97", "a", None)), Some(Lang::Rust));
        assert_eq!(of(&fr("ns::hello", "a", None)), None);
    }

    #[test]
    fn bare_names() {
        let b = |func: &str, module: &str| bare(&fr(func, module, None)).into_owned();
        assert_eq!(b("malloc", "libc.so.6"), "malloc");
        assert_eq!(b("operator new(unsigned long)", "libstdc++.so.6"), "operator new");
        assert_eq!(b("log(std::basic_ostream<char, std::char_traits<char> >&, int)", "app"), "log");
        assert_eq!(
            b("std::basic_ostream<char, std::char_traits<char> >& std::operator<< <std::char_traits<char> >(std::basic_ostream<char, std::char_traits<char> >&, char const*)", "libstdc++.so.6"),
            "std::operator<< <std::char_traits<char> >"
        );
        assert_eq!(
            b("std::__1::basic_string<char, std::__1::char_traits<char>, std::__1::allocator<char>>::append(char const*)", "libc++.1.dylib"),
            "std::basic_string<char, std::char_traits<char>, std::allocator<char>>::append"
        );
        assert_eq!(b("(anonymous namespace)::Parser::next(int) const", "app"), "(anonymous namespace)::Parser::next");
        assert_eq!(b("Functor::operator()(int) const", "app"), "Functor::operator()");
        assert_eq!(b("std::vector<int> split<int>(char const*)", "app"), "split<int>");
        assert_eq!(b("java.lang.Object java.util.HashMap.get(java.lang.Object)", "[jit]"), "java.util.HashMap.get");
        assert_eq!(
            b("instance void [System.Private.CoreLib] System.Threading.Tasks.Task::Execute()[Optimized]", "[jit]"),
            "System.Threading.Tasks.Task::Execute"
        );
        // not parameter lists
        assert_eq!(b("net/http.(*conn).serve", "server"), "net/http.(*conn).serve");
        assert_eq!(b("core::ptr::drop_in_place<(alloc::string::String,u32)>", "svc"), "core::ptr::drop_in_place<(alloc::string::String,u32)>");
        assert_eq!(b("<fn(u8) -> u8 as core::ops::function::FnOnce<(u8,)>>::call_once", "svc"), "<fn(u8) -> u8 as core::ops::function::FnOnce<(u8,)>>::call_once");
        assert_eq!(b("block (2 levels) in <main>", "[ruby]"), "block (2 levels) in <main>");
        assert_eq!(b("(anonymous)", "[js]"), "(anonymous)");
    }
}
