//! Addresses to function names, files and lines, on Linux.
//!
//! - Native code (C, C++, Rust, Go, Zig, Swift...): ELF symbols and DWARF,
//!   through blazesym, from the binary as the process sees it
//!   (/proc/<pid>/root), with inlined functions expanded.
//! - JIT-compiled code (Node.js, Java, .NET, Python 3.12+, Ruby YJIT,
//!   Erlang, Julia...): the runtime's /tmp/perf-<pid>.map, read through the
//!   process's root so containers work.
//! - The kernel: /proc/kallsyms.
//!
//! Mappings are snapshotted while processes are alive, so stacks of
//! processes that exited during a run can still be named.

use crate::code::Frame;
use blazesym::symbolize::source::{Elf, Kernel, Source};
use blazesym::symbolize::{Input, Sym, Symbolized, Symbolizer as Bz};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub off: u64,
    pub exec: bool,
    pub path: String,
}

pub fn parse_maps(s: &str) -> Vec<Mapping> {
    let mut v = Vec::new();
    for l in s.lines() {
        let mut it = l.splitn(6, ' ');
        let (Some(range), Some(perm), Some(off)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let _dev = it.next();
        let _ino = it.next();
        let path = it.next().unwrap_or("").trim().to_string();
        let Some((a, b)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(off)) = (
            u64::from_str_radix(a, 16),
            u64::from_str_radix(b, 16),
            u64::from_str_radix(off, 16),
        ) else {
            continue;
        };
        v.push(Mapping {
            start,
            end,
            off,
            exec: perm.as_bytes().get(2) == Some(&b'x'),
            path,
        });
    }
    v
}

/// One line of a perf map: start, size, name.
#[derive(Clone, Debug)]
struct JitSym {
    start: u64,
    end: u64,
    name: String,
}

fn parse_perf_map(s: &str) -> Vec<JitSym> {
    let mut v: Vec<JitSym> = s
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ' ');
            let start = u64::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
            let size = u64::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
            Some(JitSym {
                start,
                end: start + size,
                name: it.next().unwrap_or("").trim().to_string(),
            })
        })
        .collect();
    v.sort_by_key(|j| j.start);
    v
}

/// Make a JIT symbol readable: "JS:*fib /app/fib.js:3:13", "py::fib:/app/x.py",
/// "block in run@/app/x.rb:10" become function, file and line.
pub fn jit_frame(name: &str) -> Frame {
    let mut f = Frame {
        func: name.to_string(),
        module: "[jit]".into(),
        ..Frame::default()
    };
    // CPython 3.12+: py::<qualname>:<file>
    if let Some(rest) = name.strip_prefix("py::") {
        if let Some((func, file)) = rest.split_once(':') {
            f.func = func.to_string();
            f.file = Some(file.to_string());
            f.module = "[python]".into();
        }
        return f;
    }
    // V8: <Kind>:<marker><name> <script>:<line>:<col>
    const V8: &[&str] = &[
        "JS:", "LazyCompile:", "Function:", "Script:", "Eval:", "InterpretedFunction:",
    ];
    if let Some(k) = V8.iter().find(|k| name.starts_with(**k)) {
        let rest = name[k.len()..].trim_start_matches(['*', '~', '^', '+', ' ']);
        // The script follows the first space that starts a path.
        let split = rest
            .match_indices(' ')
            .map(|(i, _)| i)
            .find(|&i| {
                let r = &rest[i + 1..];
                r.starts_with('/') || r.starts_with("node:") || r.contains("://")
            })
            .or_else(|| rest.rfind(' '));
        let (func, loc) = match split {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        f.func = if func.is_empty() {
            "(anonymous)".into()
        } else {
            func.to_string()
        };
        f.module = "[js]".into();
        if let Some(loc) = loc {
            let parts: Vec<&str> = loc.rsplitn(3, ':').collect();
            // "file:line:col" -> [col, line, file]
            match parts.as_slice() {
                [_, line, file] if line.parse::<u32>().is_ok() => {
                    f.file = Some(file.to_string());
                    f.line = line.parse().ok();
                }
                [line, file] if line.parse::<u32>().is_ok() => {
                    f.file = Some(file.to_string());
                    f.line = line.parse().ok();
                }
                _ => f.file = Some(loc.to_string()),
            }
        }
        return f;
    }
    // Ruby YJIT: [JIT] name@file:line
    let r = name.strip_prefix("[JIT] ").unwrap_or(name);
    if let Some((func, loc)) = r.rsplit_once('@') {
        if let Some((file, line)) = loc.rsplit_once(':') {
            if let Ok(line) = line.parse() {
                f.func = func.to_string();
                f.file = Some(file.to_string());
                f.line = Some(line);
                f.module = "[ruby]".into();
                return f;
            }
        }
    }
    f
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).trim_end_matches(" (deleted)").to_string()
}

/// The pid as the process itself sees it (its pid namespace).
fn ns_pid(pid: i32) -> i32 {
    crate::proc::status_field(pid, "NSpid")
        .and_then(|s| s.split_whitespace().last().and_then(|x| x.parse().ok()))
        .unwrap_or(pid)
}

pub struct Symbolizer {
    bz: Bz,
    maps: HashMap<i32, Vec<Mapping>>,
    jit: HashMap<i32, Vec<JitSym>>,
    roots: HashMap<i32, String>,
    kernel: HashMap<u64, Frame>,
    cache: HashMap<(String, u64), Vec<Frame>>,
}

impl Default for Symbolizer {
    fn default() -> Self {
        Self::new()
    }
}

fn sym_frames(sym: &Sym, module: &str) -> Vec<Frame> {
    let loc = |ci: Option<&blazesym::symbolize::CodeInfo>| -> (Option<String>, Option<u32>) {
        match ci {
            Some(c) => (Some(c.to_path().to_string_lossy().into_owned()), c.line),
            None => (None, None),
        }
    };
    let mut out = Vec::new();
    // Innermost inlined function first.
    for f in sym.inlined.iter().rev() {
        let (file, line) = loc(f.code_info.as_ref());
        out.push(Frame {
            func: f.name.to_string(),
            module: module.to_string(),
            file,
            line,
            kernel: false,
        });
    }
    let (file, line) = loc(sym.code_info.as_deref());
    out.push(Frame {
        func: sym.name.to_string(),
        module: module.to_string(),
        file,
        line,
        kernel: false,
    });
    out
}

impl Symbolizer {
    pub fn new() -> Symbolizer {
        Symbolizer {
            bz: Bz::builder()
                .enable_code_info(true)
                .enable_inlined_fns(true)
                .enable_demangling(true)
                .build(),
            maps: HashMap::new(),
            jit: HashMap::new(),
            roots: HashMap::new(),
            kernel: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    /// Remember how `pid` is laid out in memory, and its JIT symbols, while
    /// it is alive. Cheap enough to call every few hundred milliseconds.
    pub fn snapshot(&mut self, pid: i32) {
        if let Some(s) = crate::proc::slurp(&format!("/proc/{pid}/maps")) {
            let m = parse_maps(&s);
            if !m.is_empty() {
                self.maps.insert(pid, m);
                self.roots
                    .entry(pid)
                    .or_insert_with(|| format!("/proc/{pid}/root"));
            }
        }
        let nspid = ns_pid(pid);
        for p in [
            format!("/proc/{pid}/root/tmp/perf-{nspid}.map"),
            format!("/tmp/perf-{pid}.map"),
        ] {
            if let Some(s) = crate::proc::slurp(&p) {
                let v = parse_perf_map(&s);
                if !v.is_empty() {
                    self.jit.insert(pid, v);
                    break;
                }
            }
        }
    }

    pub fn has_jit(&self, pid: i32) -> bool {
        self.jit.contains_key(&pid)
    }

    pub fn mappings(&self, pid: i32) -> &[Mapping] {
        self.maps.get(&pid).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn file_for(&self, pid: i32, m: &Mapping) -> String {
        if let Some(root) = self.roots.get(&pid) {
            let p = format!("{root}{}", m.path.trim_end_matches(" (deleted)"));
            if Path::new(&p).exists() {
                return p;
            }
            let mf = format!("/proc/{pid}/map_files/{:x}-{:x}", m.start, m.end);
            if Path::new(&mf).exists() {
                return mf;
            }
        }
        m.path.clone()
    }

    /// Symbolize one user stack (leaf first). Return addresses are moved
    /// back into the call instruction so lines name the call site.
    pub fn user(&mut self, pid: i32, ips: &[u64]) -> Vec<Frame> {
        if !self.maps.contains_key(&pid) {
            self.snapshot(pid);
        }
        let mut out = Vec::new();
        for (i, &ip) in ips.iter().enumerate() {
            let addr = if i == 0 { ip } else { ip.saturating_sub(1) };
            out.extend(self.one(pid, addr));
        }
        out
    }

    fn one(&mut self, pid: i32, addr: u64) -> Vec<Frame> {
        let m = self
            .maps
            .get(&pid)
            .and_then(|v| v.iter().find(|m| m.start <= addr && addr < m.end))
            .cloned();
        let jit = |s: &Self| -> Option<Frame> {
            let v = s.jit.get(&pid)?;
            let i = v.partition_point(|j| j.start <= addr);
            let j = v.get(i.checked_sub(1)?)?;
            (addr < j.end).then(|| jit_frame(&j.name))
        };
        let Some(m) = m else {
            return vec![jit(self).unwrap_or_else(|| Frame::unknown(""))];
        };
        if m.path.starts_with('[') || m.path.is_empty() || !m.path.starts_with('/') {
            if let Some(f) = jit(self) {
                return vec![f];
            }
            let name = if m.path.is_empty() { "anon" } else { &m.path };
            return vec![Frame::unknown(name.trim_matches(['[', ']']))];
        }
        let file = self.file_for(pid, &m);
        let off = addr - m.start + m.off;
        let key = (file.clone(), off);
        if let Some(f) = self.cache.get(&key) {
            return f.clone();
        }
        let module = basename(&m.path);
        let src = Source::Elf(Elf::new(&file));
        let frames = match self.bz.symbolize_single(&src, Input::FileOffset(off)) {
            Ok(Symbolized::Sym(sym)) => sym_frames(&sym, &module),
            _ => match jit(self) {
                Some(f) => vec![f],
                None => vec![Frame::unknown(&module)],
            },
        };
        self.cache.insert(key, frames.clone());
        frames
    }

    pub fn kernel(&mut self, ips: &[u64]) -> Vec<Frame> {
        let missing: Vec<u64> = ips
            .iter()
            .copied()
            .filter(|a| !self.kernel.contains_key(a))
            .collect();
        if !missing.is_empty() {
            let src = Source::Kernel(Kernel::default());
            let got = self
                .bz
                .symbolize(&src, Input::AbsAddr(&missing))
                .unwrap_or_default();
            for (i, a) in missing.iter().enumerate() {
                let func = match got.get(i) {
                    Some(Symbolized::Sym(s)) => s.name.to_string(),
                    _ => format!("{a:#x}"),
                };
                self.kernel.insert(
                    *a,
                    Frame {
                        func,
                        module: "[kernel]".into(),
                        kernel: true,
                        ..Frame::default()
                    },
                );
            }
        }
        ips.iter().map(|a| self.kernel[a].clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_lines() {
        let m = parse_maps(
            "55d0c0a00000-55d0c0a2a000 r-xp 00002000 08:01 1234 /usr/bin/app\n\
             7ffd1000-7ffd3000 r-xp 00000000 00:00 0 [vdso]\n\
             7f00-8f00 rwxp 00000000 00:00 0 \n",
        );
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].start, 0x55d0c0a00000);
        assert_eq!(m[0].off, 0x2000);
        assert!(m[0].exec);
        assert_eq!(m[0].path, "/usr/bin/app");
        assert_eq!(m[1].path, "[vdso]");
        assert_eq!(m[2].path, "");
    }

    #[test]
    fn jit_names() {
        let f = jit_frame("JS:*fib /app/fib.js:3:13");
        assert_eq!((f.func.as_str(), f.file.as_deref(), f.line), ("fib", Some("/app/fib.js"), Some(3)));
        let f = jit_frame("LazyCompile:~handler /srv/a b/x.js:10");
        assert_eq!((f.func.as_str(), f.file.as_deref(), f.line), ("handler", Some("/srv/a b/x.js"), Some(10)));
        let f = jit_frame("py::Parser.parse:/app/parse.py");
        assert_eq!((f.func.as_str(), f.file.as_deref()), ("Parser.parse", Some("/app/parse.py")));
        let f = jit_frame("[JIT] block in run@/app/x.rb:10");
        assert_eq!((f.func.as_str(), f.line), ("block in run", Some(10)));
        let f = jit_frame("java.lang.String::hashCode");
        assert_eq!(f.func, "java.lang.String::hashCode");
    }

    #[test]
    fn perf_map_lookup() {
        let v = parse_perf_map("1000 10 a\n2000 20 b c\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].name, "b c");
        assert_eq!(v[1].end, 0x2020);
    }
}
