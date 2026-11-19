//! `wits diff` on reports written by hand: which functions it says changed.

use std::path::PathBuf;
use std::process::Command;

const WITS: &str = env!("CARGO_BIN_EXE_wits");

/// A `wits run -o` report whose sources are under `tree`. `sort` has one
/// hot line that is part of its time; `init` and `take` have one line each.
/// `lock`: (seconds, the hot line's seconds); a lock time under a
/// millisecond leaves a hot spot and no function.
fn report(tree: &str, wall: [u32; 3], sort: f64, init: f64, lock: (f64, f64), functions: bool) -> String {
    let frame = |func: &str, file: &str, line: u32| format!(r#"{{"func":"{func}","module":"work","file":"{tree}/{file}","line":{line},"kind":"app","package":null}}"#);
    let hot = |seconds: f64, f: String| format!(r#"{{"share":0.5,"seconds":{seconds:.3},"frame":{f},"leaf":null,"via":null,"callers":[]}}"#);
    let func = |cause: &str, func: &str, file: &str, seconds: f64| format!(r#"{{"cause":"{cause}","func":"{func}","file":"{tree}/{file}","seconds":{seconds:.3}}}"#);
    let mut fns = vec![func("cpu", "sort", "lib/deflate.c", sort), func("cpu", "init", "lib/mf.c", init)];
    if lock.0 >= 0.001 {
        fns.push(func("lock", "take", "lib/pool.c", lock.0));
    }
    format!(
        concat!(
            r#"{{"version":2,"kind":"run","command":["work"],"exit":0,"wall_ms":{},"runs_ms":[{},{},{}],"cpu_ms":{:.0},"#,
            r#""seconds":{{"cpu":{:.3},"lock":{:.3},"idle":0.000}},"#,
            r#""code":{{"source":"ebpf","lang":"c","causes":["#,
            r#"{{"cause":"cpu","seconds":{:.3},"hot":[{},{}]}},"#,
            r#"{{"cause":"lock","seconds":{:.3},"hot":[{}]}}],"hints":[],"notes":[]}},"#,
            r#"{}"notes":[]}}"#
        ),
        wall[1],
        wall[0],
        wall[1],
        wall[2],
        (sort + init) * 1000.0,
        sort + init,
        lock.0,
        sort + init,
        hot(init, frame("init", "lib/mf.c", 93)),
        hot(sort * 0.4, frame("sort", "lib/deflate.c", 989)),
        lock.0,
        hot(lock.1, frame("take", "lib/pool.c", 12)),
        if functions { format!(r#""functions":[{}],"#, fns.join(",")) } else { String::new() },
    )
}

fn diff(name: &str, before: &str, after: &str, args: &[&str]) -> String {
    let dir: PathBuf = std::env::temp_dir().join(format!("wits-diff-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("before.json"), dir.join("after.json"));
    std::fs::write(&a, before).unwrap();
    std::fs::write(&b, after).unwrap();
    let out = Command::new(WITS).arg("diff").args(args).args([&a, &b]).env_remove("CLICOLOR_FORCE").output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_function_is_all_of_its_lines() {
    // `init` no longer runs each time, and the lock is all but gone: under
    // a millisecond is not a function's time, but the cause is still there.
    let before = report("/src", [1990, 2000, 2010], 0.767, 0.904, (0.5, 0.5), true);
    let after = report("/src", [1090, 1100, 1110], 0.741, 0.040, (0.0004, 0.0), true);
    let out = diff("whole", &before, &after, &["--json"]);
    assert!(out.contains(r#""verdict":"faster""#), "{out}");
    // `sort` with all of its time, not the 40% that is its hottest line.
    for row in [
        r#"{"cause":"cpu","func":"init","file":"/src/lib/mf.c","before":0.904,"after":0.040}"#,
        r#"{"cause":"lock","func":"take","file":"/src/lib/pool.c","before":0.500,"after":0.000}"#,
        r#"{"cause":"cpu","func":"sort","file":"/src/lib/deflate.c","before":0.767,"after":0.741}"#,
    ] {
        assert!(out.contains(row), "{row} is not in {out}");
    }
    assert_eq!(out.matches(r#""func":"#).count(), 3, "{out}");
    assert!(out.contains(r#""notes":[]"#), "{out}");
}

#[test]
fn a_report_of_an_older_wits_has_only_its_hottest_lines() {
    // Without `functions` in one of them both are compared by hot lines,
    // which is 40% of `sort` here, and the diff says so.
    let before = report("/src", [1990, 2000, 2010], 0.767, 0.904, (0.5, 0.5), false);
    let after = report("/src", [1090, 1100, 1110], 0.500, 0.040, (0.5, 0.5), true);
    let out = diff("older", &before, &after, &["--json"]);
    assert!(out.contains(r#"{"cause":"cpu","func":"sort","file":"/src/lib/deflate.c","before":0.307,"after":0.200}"#), "{out}");
    assert!(out.contains("a report was saved by an older wits"), "{out}");
}
