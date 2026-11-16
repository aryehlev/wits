//! Language runtimes: recognise one from the files a process has mapped,
//! say how to make it publish its function names when it does not, and the
//! environment `wits run` sets so that it does.

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
}
