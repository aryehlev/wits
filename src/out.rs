//! Rendering: the six-line text answer, the profile view, and the JSON form.

use crate::answer::{Answer, Cat, Row};
use crate::code::{CodeView, Frame, Hot};
use crate::json::Value;

/// Cut to `max` characters, ending with an ellipsis when cut.
pub fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
    o.push('…');
    o
}

pub fn pct(x: f64) -> String {
    format!("{:.0}%", (x * 100.0).round())
}

pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn detect() -> Style {
        let tty = unsafe { libc::isatty(1) } == 1;
        Style {
            color: tty && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    pub fn bold(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// "waiting for disk", refined for the peer case by who is at the other end.
pub fn cause_phrase(a: &Answer) -> &'static str {
    match (a.cause, a.blame.as_ref().map(|b| b.kind)) {
        (Some(Cat::Peer), Some("process")) => "waiting for another process",
        (Some(Cat::Peer), Some("remote")) => "waiting for the network",
        (Some(c), _) => c.headline(),
        (None, _) => "idle",
    }
}

pub fn headline(a: &Answer) -> String {
    let who = match &a.pod {
        Some(p) => format!("{} (pid {}, pod {p})", a.name, a.pid),
        None => format!("{} (pid {})", a.name, a.pid),
    };
    let secs = a.window_ms as f64 / 1000.0;
    match a.state {
        "stopped" => format!("{who} is {}, not slow", a.idle_what),
        "unknown" => format!(
            "{who} is blocked, but wits cannot see on what: {} ({} of the last {secs:.1}s)",
            a.idle_what,
            pct(a.idle)
        ),
        "idle" => format!(
            "{who} is idle, not slow: {} ({} of the last {secs:.1}s)",
            a.idle_what,
            pct(a.idle)
        ),
        _ => format!(
            "{who} is {}: {} of its time over the last {secs:.1}s",
            cause_phrase(a),
            pct(a.share)
        ),
    }
}

/// "71% in app::db::query (src/db.rs:42) ← app::handler (src/main.rs:10)"
pub fn where_text(h: &Hot, cat: Cat) -> String {
    let mut s = format!("{} in {}", pct(h.share), h.frame.label());
    if cat == Cat::Cpu {
        if let Some(l) = &h.leaf {
            let via = h
                .via
                .as_ref()
                .map(|v| format!(" via {}", crate::code::short_func(&v.func)))
                .unwrap_or_default();
            s = format!(
                "{} in {}{via}, called from {}",
                pct(h.share),
                crate::code::short_func(&l.func),
                h.frame.label()
            );
        }
    }
    if let Some(c) = h.callers.first() {
        s.push_str(&format!(" ← {}", c.label()));
    }
    s
}

/// The `where` line for the answer's cause, if stacks show one.
pub fn where_line(a: &Answer) -> Option<String> {
    let c = a.cause?;
    let cv = a.code.as_ref()?;
    let h = cv.get(c)?.hot.first()?;
    // An interpreter whose own functions are hidden: the C library call
    // it is in says nothing the `what` line does not.
    let hidden = cv.notes.iter().any(|n| n.contains("functions are hidden"));
    if hidden && crate::code::is_runtime(&h.frame) {
        return None;
    }
    Some(where_text(h, c))
}

/// At most seven lines.
pub fn text(a: &Answer, st: &Style) -> String {
    let mut out = vec![st.bold(&headline(a))];
    let row = |k: &str, v: &str| format!("  {} {v}", st.dim(&format!("{k:<6}")));
    if a.cause.is_some() {
        if !a.what.is_empty() {
            out.push(row("what", &a.what));
        }
        if let Some(w) = where_line(a) {
            out.push(row("where", &w));
        }
        if let Some(b) = &a.blame {
            if !b.text.is_empty() {
                out.push(row("blame", &b.text));
            }
            if let Some(u) = &b.upstream {
                upstream_lines(&u.answer, 0, &mut out);
            }
        }
        let mut also: Vec<String> = a
            .breakdown
            .iter()
            .skip(1)
            .filter(|(_, s)| *s >= 0.005)
            .map(|(c, s)| format!("{} {}", c.key(), pct(*s)))
            .collect();
        if a.idle >= 0.1 {
            also.push(if a.threads > 1 {
                format!(
                    "{} threads, {:.1} active, the rest {}",
                    a.threads, a.active, a.idle_what
                )
            } else {
                format!("idle {} of the time ({})", pct(a.idle), a.idle_what)
            });
        }
        if !also.is_empty() {
            out.push(row("also", &also.join("; ")));
        }
    }
    let mut notes = Vec::new();
    if !a.followed.is_empty() {
        let chain: Vec<String> = a
            .followed
            .iter()
            .map(|(p, n)| format!("{n} (pid {p})"))
            .collect();
        notes.push(format!(
            "{} is only waiting for this child",
            chain.join(" → ")
        ));
    }
    if a.confidence != "high" {
        notes.push(format!("confidence {}", a.confidence));
    }
    notes.extend(a.notes.iter().cloned());
    if let Some(cv) = &a.code {
        // Only notes that matter for the cause shown.
        let relevant = a.cause.is_some() && where_line(a).is_none()
            || cv.notes.iter().any(|n| n.contains("functions are hidden"));
        if relevant {
            notes.extend(cv.notes.iter().cloned());
        }
    }
    if !notes.is_empty() {
        out.push(row("note", &notes.join("; ")));
    }
    out.join("\n")
}

/// What a process on another node is itself waiting for, from its agent's
/// answer, indented under the blame line.
fn upstream_lines(v: &Value, depth: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(9 + 2 * depth);
    if let Some(s) = v.get("summary").str() {
        out.push(format!("{pad}↳ {s}"));
    }
    for k in ["what", "where"] {
        if let Some(x) = v.get(k).str().filter(|x| !x.is_empty()) {
            out.push(format!("{pad}  {k:<5} {x}"));
        }
    }
    let b = v.get("blame");
    if let Some(t) = b.get("text").str().filter(|t| !t.is_empty()) {
        out.push(format!("{pad}  blame {t}"));
    }
    let up = b.get("upstream").get("answer");
    if !up.is_null() && depth < 3 {
        upstream_lines(up, depth + 1, out);
    }
}

fn secs(ns: f64) -> String {
    let s = ns / 1e9;
    if s >= 10.0 {
        format!("{s:.0}s")
    } else if s >= 0.1 {
        format!("{s:.1}s")
    } else {
        format!("{:.0}ms", s * 1000.0)
    }
}

/// The full code view: every cause with time, its hottest code paths.
pub fn profile_text(a: &Answer, st: &Style) -> String {
    let mut out = vec![st.bold(&headline(a))];
    let Some(cv) = &a.code else {
        out.push("  no stacks were collected".into());
        return out.join("\n");
    };
    out.push(st.dim(&format!("  stacks from {}", cv.source)));
    let share = |c: Cat| a.breakdown.iter().find(|b| b.0 == c).map(|b| b.1);
    for cc in &cv.causes {
        if cc.ns < 1e6 || share(cc.cat).map(|s| s < 0.005).unwrap_or(false) {
            continue;
        }
        let head = match (cc.cat, share(cc.cat)) {
            (Cat::Idle, _) => format!("idle, not counted ({})", secs(cc.ns)),
            (c, Some(s)) => format!("{} {} ({})", c.key(), pct(s), secs(cc.ns)),
            (c, None) => format!("{} ({})", c.key(), secs(cc.ns)),
        };
        out.push(String::new());
        out.push(format!("  {}", st.bold(&head)));
        if cc.hot.is_empty() {
            out.push(st.dim("    no frames in the program's own code"));
        }
        for h in cc.hot.iter().filter(|h| h.share >= 0.005) {
            let lead = match (&h.leaf, cc.cat) {
                (Some(l), Cat::Cpu) => format!(" in {}", l.func),
                _ => String::new(),
            };
            out.push(format!("    {:>4}  {}{}", pct(h.share), h.frame.label(), st.dim(&lead)));
            if !h.callers.is_empty() {
                let c: Vec<String> = h.callers.iter().map(|f| f.label()).collect();
                out.push(st.dim(&format!("          ← {}", c.join(" ← "))));
            }
        }
    }
    let mut notes: Vec<String> = a.notes.clone();
    notes.extend(cv.notes.iter().cloned());
    if !notes.is_empty() {
        out.push(String::new());
        for n in notes {
            out.push(format!("  {} {n}", st.dim("note  ")));
        }
    }
    out.push(String::new());
    out.push(st.dim("  flame graph: wits profile --folded <pid> | flamegraph.pl > wits.svg"));
    out.join("\n")
}

fn frame_json(f: &Frame) -> String {
    let (kind, pkg) = crate::code::origin(f);
    format!(
        "{{\"func\":{},\"module\":{},\"file\":{},\"line\":{},\"kind\":{},\"package\":{}}}",
        esc(&f.func),
        esc(&f.module),
        f.file.as_deref().map(esc).unwrap_or_else(|| "null".into()),
        f.line.map(|l| l.to_string()).unwrap_or_else(|| "null".into()),
        esc(kind.key()),
        pkg.as_deref().map(esc).unwrap_or_else(|| "null".into())
    )
}

pub fn code_json(cv: &CodeView) -> String {
    let causes: Vec<String> = cv
        .causes
        .iter()
        .map(|c| {
            let hot: Vec<String> = c
                .hot
                .iter()
                .map(|h| {
                    let callers: Vec<String> = h.callers.iter().map(frame_json).collect();
                    format!(
                        "{{\"share\":{},\"seconds\":{},\"frame\":{},\"leaf\":{},\"via\":{},\"callers\":[{}]}}",
                        num(h.share),
                        num(h.ns / 1e9),
                        frame_json(&h.frame),
                        h.leaf.as_ref().map(frame_json).unwrap_or_else(|| "null".into()),
                        h.via.as_ref().map(frame_json).unwrap_or_else(|| "null".into()),
                        callers.join(",")
                    )
                })
                .collect();
            format!(
                "{{\"cause\":{},\"seconds\":{},\"hot\":[{}]}}",
                esc(c.cat.key()),
                num(c.ns / 1e9),
                hot.join(",")
            )
        })
        .collect();
    let notes: Vec<String> = cv.notes.iter().map(|n| esc(n)).collect();
    format!(
        "{{\"source\":{},\"causes\":[{}],\"notes\":[{}]}}",
        esc(cv.source),
        causes.join(","),
        notes.join(",")
    )
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn num(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.3}")
    } else {
        "0".into()
    }
}

pub fn json(a: &Answer) -> String {
    let mut f = Vec::new();
    f.push(format!("\"pid\":{}", a.pid));
    f.push(format!("\"name\":{}", esc(&a.name)));
    f.push(format!("\"cmdline\":{}", esc(&a.cmdline)));
    f.push(format!(
        "\"pod\":{}",
        a.pod.as_deref().map(esc).unwrap_or_else(|| "null".into())
    ));
    f.push(format!("\"state\":{}", esc(a.state)));
    f.push(format!(
        "\"cause\":{}",
        a.cause
            .map(|c| esc(c.key()))
            .unwrap_or_else(|| "null".into())
    ));
    f.push(format!("\"share\":{}", num(a.share)));
    f.push(format!("\"summary\":{}", esc(&headline(a))));
    f.push(format!("\"what\":{}", esc(&a.what)));
    f.push(format!(
        "\"blame\":{}",
        match &a.blame {
            Some(b) => {
                let opt = |s: &Option<String>| s.as_deref().map(esc).unwrap_or_else(|| "null".into());
                let mut extra = String::new();
                if let Some((l, r)) = b.conn {
                    extra.push_str(&format!(
                        ",\"conn\":{{\"local\":{},\"remote\":{}}}",
                        esc(&l.to_string()),
                        esc(&r.to_string())
                    ));
                }
                if let Some(u) = &b.upstream {
                    extra.push_str(&format!(
                        ",\"upstream\":{{\"node\":{},\"answer\":{}}}",
                        esc(&u.node),
                        u.answer
                    ));
                }
                format!(
                    "{{\"kind\":{},\"pid\":{},\"name\":{},\"text\":{},\"node\":{},\"pod\":{}{extra}}}",
                    esc(b.kind),
                    b.pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "null".into()),
                    opt(&b.name),
                    esc(&b.text),
                    opt(&b.node),
                    opt(&b.pod),
                )
            }
            None => "null".into(),
        }
    ));
    let bd: Vec<String> = a
        .breakdown
        .iter()
        .map(|(c, s)| format!("{}:{}", esc(c.key()), num(*s)))
        .collect();
    f.push(format!("\"breakdown\":{{{}}}", bd.join(",")));
    f.push(format!("\"active\":{}", num(a.active)));
    f.push(format!("\"idle\":{}", num(a.idle)));
    f.push(format!("\"idle_what\":{}", esc(&a.idle_what)));
    f.push(format!("\"confidence\":{}", esc(a.confidence)));
    f.push(format!("\"threads\":{}", a.threads));
    f.push(format!("\"samples\":{}", a.samples));
    f.push(format!("\"window_ms\":{}", a.window_ms));
    let fl: Vec<String> = a
        .followed
        .iter()
        .map(|(p, n)| format!("{{\"pid\":{p},\"name\":{}}}", esc(n)))
        .collect();
    f.push(format!("\"followed\":[{}]", fl.join(",")));
    let ns: Vec<String> = a.notes.iter().map(|n| esc(n)).collect();
    f.push(format!("\"notes\":[{}]", ns.join(",")));
    if let Some(w) = where_line(a) {
        f.push(format!("\"where\":{}", esc(&w)));
    }
    f.push(format!(
        "\"code\":{}",
        a.code.as_ref().map(code_json).unwrap_or_else(|| "null".into())
    ));
    format!("{{\"version\":1,{}}}", f.join(","))
}

fn why(r: &Row) -> String {
    match &r.answer {
        Some(a) if a.cause.is_some() => {
            let b = a
                .blame
                .as_ref()
                .filter(|b| b.kind == "process" || b.kind == "cgroup")
                .map(|b| format!(" ← {}", b.text))
                .unwrap_or_default();
            format!("{}: {}{b}", cause_phrase(a), a.what)
        }
        Some(a) => format!("idle now: {}", a.idle_what),
        None => "exited before wits could look".into(),
    }
}

/// The terminal's width, or 200 when output is not a terminal.
pub fn term_width() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 40 {
            ws.ws_col as usize
        } else {
            200
        }
    }
}

pub fn overview_text(rows: &[Row], window_ms: u64, st: &Style) -> String {
    let secs = window_ms as f64 / 1000.0;
    if rows.is_empty() {
        return format!(
            "Nothing on this machine was waiting or working hard in the last {secs:.1}s.\nRun `wits <pid|name>` to ask about one process."
        );
    }
    let waiting = rows.iter().filter(|r| r.waiting >= 0.01).count();
    let title = if waiting == 0 {
        format!("Nothing is waiting right now; the busiest processes over {secs:.1}s:")
    } else {
        format!("Processes waiting most right now ({secs:.1}s sample):")
    };
    let width = term_width();
    let mut out = vec![
        st.bold(&title),
        st.dim("    pid  name         waiting  why"),
    ];
    for r in rows {
        let head = format!(
            "{:>7}  {:<15} {:>4}  ",
            r.pid,
            shorten(&r.name, 15),
            pct(r.waiting)
        );
        let room = width.saturating_sub(head.chars().count()).max(20);
        out.push(format!("{head}{}", shorten(&why(r), room)));
    }
    out.push(st.dim("Run `wits <pid>` for the full answer on one of them."));
    out.join("\n")
}

pub fn overview_json(rows: &[Row], window_ms: u64) -> String {
    let items: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{{\"pid\":{},\"name\":{},\"waiting\":{},\"answer\":{}}}",
                r.pid,
                esc(&r.name),
                num(r.waiting),
                r.answer.as_ref().map(json).unwrap_or_else(|| "null".into())
            )
        })
        .collect();
    format!(
        "{{\"version\":1,\"window_ms\":{window_ms},\"processes\":[{}]}}",
        items.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::Blame;

    fn answer() -> Answer {
        Answer {
            pid: 42,
            name: "db \"x\"".into(),
            cmdline: "db --flag".into(),
            followed: vec![(7, "sh".into())],
            window_ms: 2000,
            threads: 1,
            samples: 200,
            state: "stalled",
            cause: Some(Cat::Disk),
            share: 0.87,
            active: 1.0,
            what: "fsync() on /data/wal".into(),
            blame: Some(Blame {
                kind: "process",
                pid: Some(9),
                name: Some("rsync".into()),
                text: "pid 9 (rsync)".into(),
                ..Blame::default()
            }),
            breakdown: vec![
                (Cat::Disk, 0.87),
                (Cat::Cpu, 0.09),
                (Cat::CpuWait, 0.04),
                (Cat::Lock, 0.0),
            ],
            idle: 0.0,
            idle_what: "idle".into(),
            confidence: "medium",
            notes: vec!["n1".into()],
            code: None,
            pod: None,
        }
    }

    #[test]
    fn text_is_at_most_seven_lines() {
        let t = text(&answer(), &Style { color: false });
        assert!(t.lines().count() <= 7, "{t}");
        assert!(t.starts_with(
            "db \"x\" (pid 42) is waiting for disk: 87% of its time over the last 2.0s"
        ));
        assert!(t.contains("also   cpu 9%; cpu-wait 4%"));
        assert!(t.contains("confidence medium"));
    }

    #[test]
    fn json_is_escaped_and_complete() {
        let j = json(&answer());
        assert!(j.contains("\"name\":\"db \\\"x\\\"\""));
        assert!(j.contains("\"cause\":\"disk\""));
        assert!(j.contains("\"blame\":{\"kind\":\"process\",\"pid\":9"));
        assert!(j.contains("\"breakdown\":{\"disk\":0.870"));
        assert_eq!(j.matches('{').count(), j.matches('}').count());
    }

    #[test]
    fn idle_headline() {
        let mut a = answer();
        a.cause = None;
        a.state = "idle";
        a.idle = 1.0;
        a.idle_what = "asleep on its own timer".into();
        assert_eq!(
            text(&a, &Style { color: false }).lines().next().unwrap(),
            "db \"x\" (pid 42) is idle, not slow: asleep on its own timer (100% of the last 2.0s)"
        );
    }
}
