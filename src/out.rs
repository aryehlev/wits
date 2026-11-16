//! Rendering: the six-line text answer and the JSON form.

use crate::explain::{Answer, Cat};
use crate::proc::shorten;
use crate::top::Row;

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
    fn bold(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
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
    let who = format!("{} (pid {})", a.name, a.pid);
    let secs = a.window_ms as f64 / 1000.0;
    match a.state {
        "stopped" => format!("{who} is {}, not slow", a.idle_what),
        "unknown" => format!(
            "{who} is blocked, but wits cannot see on what ({} of the last {secs:.1}s)",
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

/// At most six lines.
pub fn text(a: &Answer, st: &Style) -> String {
    let mut out = vec![st.bold(&headline(a))];
    let row = |k: &str, v: &str| format!("  {} {v}", st.dim(&format!("{k:<6}")));
    if a.cause.is_some() {
        if !a.what.is_empty() {
            out.push(row("what", &a.what));
        }
        if let Some(b) = &a.blame {
            if !b.text.is_empty() {
                out.push(row("blame", &b.text));
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
    if !notes.is_empty() {
        out.push(row("note", &notes.join("; ")));
    }
    out.join("\n")
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

fn num(x: f64) -> String {
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
            Some(b) => format!(
                "{{\"kind\":{},\"pid\":{},\"name\":{},\"text\":{}}}",
                esc(b.kind),
                b.pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "null".into()),
                b.name.as_deref().map(esc).unwrap_or_else(|| "null".into()),
                esc(&b.text)
            ),
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
    let width = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 40 {
            ws.ws_col as usize
        } else {
            200
        }
    };
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
    use crate::explain::Blame;

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
        }
    }

    #[test]
    fn text_is_at_most_six_lines() {
        let t = text(&answer(), &Style { color: false });
        assert!(t.lines().count() <= 6, "{t}");
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
