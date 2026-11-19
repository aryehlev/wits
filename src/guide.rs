//! The guides: what usually makes a program in each language slow, and what
//! usually fixes it. They are the files of the skill
//! (skills/why-is-this-slow/guides), built into the binary so that
//! `wits guide` and the MCP server say the same to agents without the skill.

pub struct Guide {
    /// The topic: a language as `code.lang` names it, or "general".
    pub key: &'static str,
    pub title: &'static str,
    /// Other names people ask by.
    also: &'static [&'static str],
    pub text: &'static str,
}

macro_rules! guide {
    ($key:literal, $title:literal, $also:expr) => {
        Guide {
            key: $key,
            title: $title,
            also: $also,
            text: include_str!(concat!("../skills/why-is-this-slow/guides/", $key, ".md")),
        }
    };
}

pub const GUIDES: &[Guide] = &[
    guide!("general", "any language: what each cause usually comes down to", &["any", "all", "common"]),
    guide!("python", "Python", &["py", "python3", "cpython"]),
    guide!("node", "Node.js: JavaScript and TypeScript", &["nodejs", "node.js", "javascript", "js", "typescript", "ts", "deno"]),
    guide!("go", "Go", &["golang"]),
    guide!("rust", "Rust", &["rs"]),
    guide!("jvm", "the JVM: Java, Kotlin, Scala, Clojure", &["java", "kotlin", "scala", "clojure"]),
    guide!("dotnet", ".NET: C#, F#", &[".net", "csharp", "c#", "cs", "fsharp", "f#"]),
    guide!("ruby", "Ruby and Rails", &["rb", "rails"]),
    guide!("beam", "Erlang and Elixir", &["erlang", "elixir", "erl", "otp"]),
    guide!("c", "C and C++", &["c++", "cpp", "cxx", "cc"]),
];

/// The guide for a topic, by its key or another name, in any case.
pub fn find(topic: &str) -> Option<&'static Guide> {
    let t = topic.trim().to_ascii_lowercase();
    GUIDES.iter().find(|g| g.key == t || g.also.contains(&t.as_str()))
}

/// The topics, one per line.
pub fn list() -> String {
    GUIDES
        .iter()
        .map(|g| format!("  {:<8} {}", g.key, g.title))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `wits guide [topic]`: print a guide, or the topics.
pub fn run(args: &[String]) -> i32 {
    match args {
        [] => {
            println!("wits guide <topic>: what usually makes it slow, and what usually fixes it\n\n{}", list());
            0
        }
        [t] => match find(t) {
            Some(g) => {
                print!("{}", g.text);
                0
            }
            None => {
                eprintln!("wits: no guide for '{t}'. The topics:\n{}", list());
                2
            }
        },
        _ => {
            eprintln!("wits: give one topic (try `wits guide`)");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;

    #[test]
    fn every_language_has_a_guide() {
        for l in Lang::ALL {
            let g = find(l.key()).unwrap_or_else(|| panic!("no guide for {}", l.key()));
            assert_eq!(g.key, l.key());
        }
        assert_eq!(find("TypeScript").unwrap().key, "node");
        assert_eq!(find("c++").unwrap().key, "c");
        assert_eq!(find(" Kotlin ").unwrap().key, "jvm");
        assert!(find("cobol").is_none());
    }

    #[test]
    fn guides_are_whole() {
        let mut names = std::collections::HashSet::new();
        for g in GUIDES {
            assert!(g.text.starts_with("# "), "{}: starts with its title", g.key);
            assert!(g.text.ends_with('\n') && g.text.len() > 2000, "{}", g.key);
            for n in std::iter::once(&g.key).chain(g.also) {
                assert!(names.insert(*n), "{n} names two guides");
            }
        }
    }

    /// A guide names hints by their id; each id it names for its language
    /// must be one wits can give.
    #[test]
    fn guides_name_real_hints() {
        for l in Lang::ALL {
            let g = find(l.key()).unwrap();
            for id in crate::hint::ids(l) {
                assert!(g.text.contains(&format!("`{id}`")), "{}.md does not mention the hint `{id}`", g.key);
            }
        }
    }
}
