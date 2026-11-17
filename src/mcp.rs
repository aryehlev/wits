//! `wits mcp`: an MCP server on stdin and stdout, for coding agents.
//!
//! Each tool runs this same binary with `--json` and returns its answer as
//! text and as structured content, so the tools do exactly what the
//! commands do. Hosts that show tool results as pages (MCP Apps) get the
//! page in ui.rs, with flame graphs; others get the JSON, and any tool can
//! also write the page to a file (`html_file`).

use crate::json::{self, Value};
use crate::out::esc;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const VIEW: &str = "ui://wits/view.html";
const VIEW_MIME: &str = "text/html;profile=mcp-app";
/// Versions that use `initialize`, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "wits measures why a process is slow instead of guessing. \
Call wits_explain on the slow process (a pid you started is best; a name, or on a Kubernetes \
node namespace/pod or namespace/<deployment> for every replica). Read cause, what, where and \
blame: blame names the process, lock holder or remote host responsible; ask wits_explain about \
a blamed process in turn until the answer is cpu, disk, memory, a cgroup limit, a remote host \
or an idle process. Before changing code, list candidate fixes and rank them by \
score = 0.7 x gain + 0.3 x ease. gain (0-1) is the share of the measured time the change would \
remove: at most the cause's share times the hot spot's share, less for a partial fix. ease (0-1) \
is 1 for a flag, config value or one-line change, 0.7 for a small change in one function, 0.4 \
for several functions or files or an API change, 0.1 for a redesign or new dependency. Make the \
highest-scoring change first, one at a time, and tell the user the estimated scores. To check \
each fix, call wits_run with repeat 5 before and after the change (save each report) and \
wits_diff on the two files; report its verdict as it is: 'no clear change' is not a speedup, \
and a change that buys no speed should be undone unless it is wanted for another reason. wits never changes or signals the processes it watches. \
Without root it cannot see other users' processes, and on Linux shows no on-CPU stacks.";

const TOOLS: &str = r#"[
 {"name":"wits_explain",
  "title":"Why is it slow?",
  "description":"Watch one or more running processes (2 s by default) and explain why they are slow: the cause (cpu, cpu-wait, disk, memory, lock, peer), its share of the time, the specific file, lock or socket, who is to blame (another process, a lock holder, a cgroup limit, a remote host), and the function and line where the time goes. Several targets, a pod with several processes, or a workload give one answer per process or pod, busiest first.",
  "inputSchema":{"type":"object","properties":{
    "target":{"type":"string","description":"A pid, a process name, or (on a Kubernetes node) namespace/pod[/container], or namespace/<deployment|statefulset|daemonset name> for all its replicas."},
    "targets":{"type":"array","items":{"type":"string"},"description":"Several targets at once, instead of target."},
    "seconds":{"type":"number","minimum":0.1,"maximum":60,"description":"How long to watch (default 2). Longer catches stalls that come and go."},
    "stacks":{"type":"boolean","description":"Collect stacks for the 'where' field (default true)."},
    "html_file":{"type":"string","description":"Also write the answer as a web page to this path."}}},
  "annotations":{"readOnlyHint":true,"openWorldHint":false},
  "_meta":{"ui":{"resourceUri":"ui://wits/view.html"}}},
 {"name":"wits_overview",
  "title":"What is waiting right now?",
  "description":"The processes on this machine that are waiting most right now, each with a one-line reason. Use it when you do not know which process is slow.",
  "inputSchema":{"type":"object","properties":{
    "seconds":{"type":"number","minimum":0.1,"maximum":60,"description":"How long to watch (default 2)."},
    "html_file":{"type":"string","description":"Also write the result as a web page to this path."}}},
  "annotations":{"readOnlyHint":true,"openWorldHint":false},
  "_meta":{"ui":{"resourceUri":"ui://wits/view.html"}}},
 {"name":"wits_profile",
  "title":"Profile by cause",
  "description":"Like wits_explain for one process, plus every stack: where the time goes for each cause (on CPU, disk, lock, waiting on another process...), for a flame graph. The result includes 'flame' (frames and weighted stacks) next to the answer.",
  "inputSchema":{"type":"object","properties":{
    "target":{"type":"string","description":"A pid, a process name, or namespace/pod[/container] on a Kubernetes node."},
    "seconds":{"type":"number","minimum":0.1,"maximum":60,"description":"How long to watch (default 2)."},
    "html_file":{"type":"string","description":"Also write the profile as a web page with flame graphs to this path."}},
   "required":["target"]},
  "annotations":{"readOnlyHint":true,"openWorldHint":false},
  "_meta":{"ui":{"resourceUri":"ui://wits/view.html"}}},
 {"name":"wits_run",
  "title":"Run a command and measure it",
  "description":"Run a command, watch it and everything it starts until it exits, and report where its time went: wall time, time per cause, per process, and the hottest functions. Save the report and compare two with wits_diff to check whether a change helped. Only repeat commands that are safe to run again (tests, builds, benchmarks).",
  "inputSchema":{"type":"object","properties":{
    "command":{"type":"array","items":{"type":"string"},"minItems":1,"description":"The command and its arguments, run directly (not through a shell): [\"cargo\",\"test\"]. Use [\"sh\",\"-c\",\"...\"] for a pipeline."},
    "repeat":{"type":"integer","minimum":1,"maximum":20,"description":"Run it this many times and use the median (default 1; 5 to compare with wits_diff)."},
    "save":{"type":"string","description":"Save the report to this JSON file, for wits_diff."},
    "cwd":{"type":"string","description":"Directory to run it in (default: the server's)."},
    "html_file":{"type":"string","description":"Also write the report as a web page to this path."}},
   "required":["command"]},
  "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false},
  "_meta":{"ui":{"resourceUri":"ui://wits/view.html"}}},
 {"name":"wits_diff",
  "title":"Did the change help?",
  "description":"Compare two reports saved by wits_run: verdict (faster, slower or no clear change, judged against run-to-run noise), wall and CPU time before and after, time per cause, and the functions whose time changed most.",
  "inputSchema":{"type":"object","properties":{
    "before":{"type":"string","description":"The report saved before the change."},
    "after":{"type":"string","description":"The report saved after it."},
    "html_file":{"type":"string","description":"Also write the comparison as a web page to this path."}},
   "required":["before","after"]},
  "annotations":{"readOnlyHint":true,"openWorldHint":false},
  "_meta":{"ui":{"resourceUri":"ui://wits/view.html"}}}
]"#;

type Out = Arc<Mutex<std::io::Stdout>>;

fn send(out: &Out, line: &str) {
    if let Ok(mut o) = out.lock() {
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }
}

fn result(id: &Value, r: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{r}}}")
}

fn error(id: &Value, code: i32, msg: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        esc(msg)
    )
}

/// The answer to every request except tools/call; None for notifications.
fn reply(msg: &Value) -> Option<String> {
    let id = msg.get("id");
    let method = msg.get("method").str()?;
    if id.is_null() {
        return None;
    }
    Some(match method {
        "initialize" => {
            let asked = msg.get("params").get("protocolVersion").str().unwrap_or("");
            let v = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
            result(
                id,
                &format!(
                    "{{\"protocolVersion\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}},\"resources\":{{\"listChanged\":false}}}},\"serverInfo\":{{\"name\":\"wits\",\"title\":\"wits\",\"version\":{}}},\"instructions\":{}}}",
                    esc(v),
                    esc(env!("CARGO_PKG_VERSION")),
                    esc(INSTRUCTIONS)
                ),
            )
        }
        "ping" => result(id, "{}"),
        // One line: stdio messages must not contain newlines.
        "tools/list" => result(
            id,
            &format!("{{\"tools\":{}}}", json::parse(TOOLS).unwrap_or(Value::Null)),
        ),
        "resources/list" => result(
            id,
            &format!(
                "{{\"resources\":[{{\"uri\":{},\"name\":\"wits view\",\"description\":\"Shows a wits answer, run or diff, with flame graphs by cause\",\"mimeType\":{},\"_meta\":{{\"ui\":{{\"prefersBorder\":true}}}}}}]}}",
                esc(VIEW),
                esc(VIEW_MIME)
            ),
        ),
        "resources/templates/list" => result(id, "{\"resourceTemplates\":[]}"),
        "resources/read" => {
            if msg.get("params").get("uri").str() != Some(VIEW) {
                return Some(error(id, -32002, "no such resource"));
            }
            result(
                id,
                &format!(
                    "{{\"contents\":[{{\"uri\":{},\"mimeType\":{},\"text\":{},\"_meta\":{{\"ui\":{{\"prefersBorder\":true}}}}}}]}}",
                    esc(VIEW),
                    esc(VIEW_MIME),
                    esc(crate::ui::PAGE)
                ),
            )
        }
        _ => error(id, -32601, &format!("no method {method}")),
    })
}

/// The wits command line a tool call stands for, the directory to run it
/// in, and the file a run report is read from.
struct Plan {
    args: Vec<String>,
    cwd: Option<String>,
    report: Option<(String, bool)>,
}

fn plan(name: &str, a: &Value, n: u64) -> Result<Plan, String> {
    let secs = |a: &Value| -> Vec<String> {
        match a.get("seconds").num() {
            Some(s) if (0.1..=60.0).contains(&s) => vec!["-t".into(), format!("{s}")],
            Some(_) => vec!["-t".into(), "60".into()],
            None => Vec::new(),
        }
    };
    let html: Vec<String> = match a.get("html_file").str() {
        Some(p) if !p.is_empty() => vec!["--html".into(), p.into()],
        _ => Vec::new(),
    };
    let strs = |v: &Value| -> Vec<String> { v.arr().iter().filter_map(|x| x.str().map(str::to_string)).collect() };
    let mut args: Vec<String> = Vec::new();
    let mut report = None;
    let mut cwd = None;
    match name {
        "wits_overview" => {
            args.push("--json".into());
            args.extend(secs(a));
            args.extend(html);
        }
        "wits_explain" | "wits_profile" => {
            let mut targets = strs(a.get("targets"));
            if let Some(t) = a.get("target").str() {
                targets.insert(0, t.to_string());
            }
            targets.retain(|t| !t.trim().is_empty());
            if targets.is_empty() {
                return Err("give a target: a pid, a process name, or namespace/pod".into());
            }
            if name == "wits_profile" {
                if targets.len() > 1 {
                    return Err("wits_profile takes one target; use wits_explain for several".into());
                }
                args.push("profile".into());
            }
            args.push("--json".into());
            args.extend(secs(a));
            if a.get("stacks").bool() == Some(false) {
                args.push("--no-stacks".into());
            }
            args.extend(html);
            // Targets never start with '-': keep them from being read as options.
            if let Some(t) = targets.iter().find(|t| t.starts_with('-')) {
                return Err(format!("'{t}' is not a pid, name or pod"));
            }
            args.extend(targets);
        }
        "wits_run" => {
            let cmd = strs(a.get("command"));
            if cmd.is_empty() {
                return Err("give the command as a list, e.g. [\"cargo\", \"test\"]".into());
            }
            let repeat = a.get("repeat").int().unwrap_or(1).clamp(1, 20);
            let (file, keep) = match a.get("save").str() {
                Some(p) if !p.is_empty() => (p.to_string(), true),
                _ => (
                    std::env::temp_dir()
                        .join(format!("wits-mcp-{}-{n}.json", std::process::id()))
                        .to_string_lossy()
                        .into_owned(),
                    false,
                ),
            };
            args.extend(["run".into(), "-n".into(), repeat.to_string(), "-o".into(), file.clone()]);
            args.extend(html);
            args.push("--".into());
            args.extend(cmd);
            report = Some((file, keep));
            cwd = a.get("cwd").str().map(str::to_string);
        }
        "wits_diff" => {
            let (Some(b), Some(f)) = (a.get("before").str(), a.get("after").str()) else {
                return Err("give before and after: two reports saved by wits_run".into());
            };
            args.extend(["diff".into(), "--json".into()]);
            args.extend(html);
            args.extend([b.to_string(), f.to_string()]);
        }
        _ => return Err(format!("no tool {name}")),
    }
    Ok(Plan { args, cwd, report })
}

fn tail(b: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim_end();
    match s.char_indices().rev().nth(max) {
        Some((i, _)) => format!("…{}", &s[i..]),
        None => s.to_string(),
    }
}

/// Run a tool: the CallToolResult as JSON.
fn call(params: &Value, n: u64, running: &Mutex<HashMap<String, i32>>, key: &str) -> String {
    let name = params.get("name").str().unwrap_or("");
    let failed = |msg: &str| format!("{{\"content\":[{{\"type\":\"text\",\"text\":{}}}],\"isError\":true}}", esc(msg));
    let p = match plan(name, params.get("arguments"), n) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return failed(&format!("cannot find the wits binary: {e}")),
    };
    let mut cmd = Command::new(exe);
    cmd.args(&p.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = &p.cwd {
        cmd.current_dir(d);
    }
    // Its own process group, so a cancelled call stops what it started.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(&format!("cannot run wits: {e}")),
    };
    if let Ok(mut r) = running.lock() {
        r.insert(key.to_string(), child.id() as i32);
    }
    let out = child.wait_with_output();
    if let Ok(mut r) = running.lock() {
        r.remove(key);
    }
    let out = match out {
        Ok(o) => o,
        Err(e) => return failed(&format!("wits failed: {e}")),
    };
    let (text, extra) = match &p.report {
        Some((file, keep)) => {
            let t = std::fs::read_to_string(file).unwrap_or_default();
            if !keep {
                let _ = std::fs::remove_file(file);
            }
            let mut extra = Vec::new();
            if *keep {
                extra.push(format!("report saved to {file}"));
            }
            let o = tail(&out.stdout, 1500);
            let e = tail(&out.stderr, 1500);
            if !o.is_empty() {
                extra.push(format!("command output (end):\n{o}"));
            }
            if !e.is_empty() {
                extra.push(format!("command and wits messages on stderr (end):\n{e}"));
            }
            (t, extra)
        }
        None => (String::from_utf8_lossy(&out.stdout).trim().to_string(), Vec::new()),
    };
    let Ok(v) = json::parse(text.trim()) else {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return failed(if why.is_empty() { "wits gave no answer" } else { why });
    };
    let mut content = vec![format!("{{\"type\":\"text\",\"text\":{}}}", esc(text.trim()))];
    if let Some(h) = params.get("arguments").get("html_file").str().filter(|h| !h.is_empty()) {
        content.push(format!("{{\"type\":\"text\",\"text\":{}}}", esc(&format!("page written to {h}; open it in a browser"))));
    }
    for x in extra {
        content.push(format!("{{\"type\":\"text\",\"text\":{}}}", esc(&x)));
    }
    format!(
        "{{\"content\":[{}],\"structuredContent\":{v},\"isError\":false}}",
        content.join(",")
    )
}

pub fn serve() -> i32 {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let running: Arc<Mutex<HashMap<String, i32>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut n = 0u64;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg = match json::parse(&line) {
            Ok(v) => v,
            Err(e) => {
                send(&out, &error(&Value::Null, -32700, &e));
                continue;
            }
        };
        match msg.get("method").str() {
            Some("tools/call") if !msg.get("id").is_null() => {
                n += 1;
                let (out, running) = (out.clone(), running.clone());
                std::thread::spawn(move || {
                    let id = msg.get("id");
                    let key = id.to_string();
                    let r = call(msg.get("params"), n, &running, &key);
                    send(&out, &result(id, &r));
                });
            }
            Some("notifications/cancelled") => {
                let key = msg.get("params").get("requestId").to_string();
                if let Some(pid) = running.lock().ok().and_then(|r| r.get(&key).copied()) {
                    unsafe { libc::kill(-pid, libc::SIGTERM) };
                }
            }
            _ => {
                if let Some(r) = reply(&msg) {
                    send(&out, &r);
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_one_line() {
        for m in ["initialize", "tools/list", "resources/list", "resources/read"] {
            let req = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{m}","params":{{"uri":"ui://wits/view.html"}}}}"#);
            let r = reply(&json::parse(&req).unwrap()).unwrap();
            assert!(!r.contains('\n'), "{m}");
            assert!(json::parse(&r).is_ok(), "{m}");
        }
    }

    #[test]
    fn tools_are_valid_json() {
        let t = json::parse(TOOLS).unwrap();
        let names: Vec<&str> = t.arr().iter().filter_map(|x| x.get("name").str()).collect();
        assert_eq!(names, ["wits_explain", "wits_overview", "wits_profile", "wits_run", "wits_diff"]);
        for x in t.arr() {
            assert_eq!(x.get("_meta").get("ui").get("resourceUri").str(), Some(VIEW));
            assert_eq!(x.get("inputSchema").get("type").str(), Some("object"));
        }
    }

    #[test]
    fn handshake() {
        let m = json::parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#).unwrap();
        let r = json::parse(&reply(&m).unwrap()).unwrap();
        assert_eq!(r.get("result").get("protocolVersion").str(), Some("2025-06-18"));
        assert_eq!(r.get("result").get("serverInfo").get("name").str(), Some("wits"));
        let m = json::parse(r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#).unwrap();
        let r = json::parse(&reply(&m).unwrap()).unwrap();
        assert_eq!(r.get("result").get("protocolVersion").str(), Some(VERSIONS[0]));
        assert_eq!(r.get("id").str(), Some("a"));
        let note = json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(reply(&note).is_none());
        let read = json::parse(r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"ui://wits/view.html"}}"#).unwrap();
        let r = json::parse(&reply(&read).unwrap()).unwrap();
        let c = &r.get("result").get("contents").arr()[0];
        assert_eq!(c.get("mimeType").str(), Some(VIEW_MIME));
        assert!(c.get("text").str().unwrap().contains("ui/initialize"));
    }

    #[test]
    fn plans() {
        let a = json::parse(r#"{"targets":["shop/web","1234"],"seconds":5,"stacks":false}"#).unwrap();
        let p = plan("wits_explain", &a, 1).unwrap();
        assert_eq!(p.args, ["--json", "-t", "5", "--no-stacks", "shop/web", "1234"]);
        let a = json::parse(r#"{"command":["cargo","test"],"repeat":5,"save":"b.json"}"#).unwrap();
        let p = plan("wits_run", &a, 1).unwrap();
        assert_eq!(p.args, ["run", "-n", "5", "-o", "b.json", "--", "cargo", "test"]);
        assert!(plan("wits_explain", &json::parse(r#"{"target":"--help"}"#).unwrap(), 1).is_err());
        assert!(plan("wits_profile", &json::parse("{}").unwrap(), 1).is_err());
    }
}
