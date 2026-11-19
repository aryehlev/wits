//! `wits on`, with this machine standing in for the other one: `--via
//! 'sh -c'` runs each script here, and the cache directory of the test is
//! where wits is put "there".

use std::path::Path;
use std::process::{Command, Output};

const WITS: &str = env!("CARGO_BIN_EXE_wits");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn on(cache: &Path, before: &[&str], args: &[&str]) -> Output {
    Command::new(WITS)
        .arg("on")
        .args(before)
        .args(["--via", "sh -c"])
        .args(args)
        .env("XDG_CACHE_HOME", cache)
        .env("WITS_REMOTE_BINARY", WITS)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn installs_asks_and_brings_files_back() {
    let dir = std::env::temp_dir().join(format!("wits-on-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let there = dir.join("wits");
    let bin = there.join(format!("wits-{VERSION}"));

    // The first time wits is not there: it is put there, and then answers.
    let out = on(&dir, &[], &["--version"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), format!("wits {VERSION}"));
    assert!(text(&out.stderr).contains("putting wits"), "{}", text(&out.stderr));
    assert_eq!(std::fs::metadata(&bin).unwrap().len(), std::fs::metadata(WITS).unwrap().len());

    // After that it is only asked.
    let out = on(&dir, &[], &["--version"]);
    assert!(out.status.success());
    assert_eq!(text(&out.stderr), "");

    // A run there: the command's status is ours, and the files wits wrote
    // there are here, under the names given here.
    let report = dir.join("report.json");
    let page = dir.join("it's a page.html");
    let out = on(
        &dir,
        &[],
        &["run", "-o", report.to_str().unwrap(), "--html", page.to_str().unwrap(), "--", "sh", "-c", "echo out; exit 3"],
    );
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(text(&out.stdout), "out\n");
    assert!(std::fs::read_to_string(&report).unwrap().contains("\"kind\":\"run\""));
    assert!(std::fs::read_to_string(&page).unwrap().contains("<html"));
    assert!(err.contains(&format!("wrote {}", report.display())), "{err}");
    assert!(!err.contains("out-"), "a file's name there is shown: {err}");
    let left: Vec<String> = std::fs::read_dir(&there).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, [format!("wits-{VERSION}")]);

    // What works on this machine is not sent to the other.
    let out = on(&dir, &[], &["guide", "go"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("without `on`"));

    // --rm leaves nothing there.
    let out = on(&dir, &["--rm"], &["--version"]);
    assert!(out.status.success());
    assert!(!bin.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_way_that_fails_is_said() {
    let out = Command::new(WITS).args(["on", "--via", "no-such-command-for-wits", "1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("is not a command here"));
    let out = Command::new(WITS).args(["on", "-oProxyCommand=id", "1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
