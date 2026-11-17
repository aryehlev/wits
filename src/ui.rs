//! One self-contained HTML page that shows anything wits reports (an
//! answer, several pods, a run, a diff, the overview), with a flame graph
//! per cause when stacks are included. Written to a file by `--html`, and
//! served to MCP hosts that show tool results as pages (MCP Apps).
//!
//! For deeper work on the stacks, `wits profile --folded` feeds the usual
//! tools (flamegraph.pl, inferno, speedscope).

/// The page without data: it waits for an MCP host to send a tool result.
pub const PAGE: &str = include_str!("ui.html");

const MARK: &str = "__WITS_DATA__";

/// The page with a wits JSON document embedded in it.
pub fn page(json: &str) -> String {
    // `<` only occurs inside JSON strings, where < means the same; this
    // keeps "</script>" in a process name from ending the script.
    PAGE.replacen(MARK, &json.replace('<', "\\u003c"), 1)
}

/// Write the page for `json` to `path`, and say where it is on stderr.
pub fn write(path: &str, json: &str) {
    match std::fs::write(path, page(json)) {
        Ok(()) => eprintln!("wits: wrote {path}; open it in a browser"),
        Err(e) => crate::die(&format!("cannot write {path}: {e}"), 1),
    }
}

/// A JSON object with one more field.
pub fn with_field(json: &str, key: &str, value: &str) -> String {
    match json.trim_end().strip_suffix('}') {
        Some(head) if head.trim_end().ends_with('{') => format!("{head}\"{key}\":{value}}}"),
        Some(head) => format!("{head},\"{key}\":{value}}}"),
        None => json.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_data_safely() {
        let p = page(r#"{"name":"</script><b>x"}"#);
        assert!(!p.contains("</script><b>"));
        assert!(p.contains(r#"{"name":"\u003c/script>\u003cb>x"}"#));
        assert!(!p.contains(MARK));
        assert!(PAGE.contains(MARK));
    }

    #[test]
    fn adds_fields() {
        assert_eq!(with_field(r#"{"a":1}"#, "b", "2"), r#"{"a":1,"b":2}"#);
        assert_eq!(with_field("{}", "b", "2"), r#"{"b":2}"#);
    }
}
