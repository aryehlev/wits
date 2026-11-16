//! Thin readers for the /proc files wits depends on. Every reader returns
//! `None` on any failure: processes vanish mid-read all the time.

use std::fs;
use std::io::Read;

pub fn slurp(path: &str) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut v = Vec::with_capacity(512);
    f.read_to_end(&mut v).ok()?;
    Some(String::from_utf8_lossy(&v).into_owned())
}

/// Like `slurp`, but tells a permission failure apart from a vanished file.
pub fn slurp_perm(path: &str) -> Result<String, bool> {
    let denied = |e: &std::io::Error| {
        e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(libc::EPERM)
    };
    let mut f = fs::File::open(path).map_err(|e| denied(&e))?;
    let mut v = Vec::with_capacity(256);
    f.read_to_end(&mut v).map_err(|e| denied(&e))?;
    Ok(String::from_utf8_lossy(&v).into_owned())
}

pub fn clk_tck() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

pub fn page_size() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as u64
    } else {
        4096
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub flags: u64,
    pub majflt: u64,
    pub utime: u64,
    pub stime: u64,
    pub nice: i64,
    pub num_threads: u64,
    pub starttime: u64,
    pub rss: u64,
    /// CPU the task last ran on.
    pub processor: u32,
}

impl Stat {
    pub fn is_kthread(&self) -> bool {
        self.flags & 0x0020_0000 != 0
    }
}

/// Parse a /proc/<pid>/stat line. The command name may contain spaces and
/// parentheses, so it is delimited by the first '(' and the last ')'.
pub fn parse_stat(s: &str) -> Option<Stat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = s[open + 1..close].to_string();
    let rest: Vec<&str> = s[close + 1..].split_ascii_whitespace().collect();
    // rest[0] is field 3 (state); field n lives at rest[n - 3].
    let num = |n: usize| rest.get(n - 3).and_then(|x| x.parse::<u64>().ok());
    Some(Stat {
        comm,
        state: rest.first()?.chars().next()?,
        ppid: rest.get(1)?.parse().ok()?,
        flags: num(9)?,
        majflt: num(12)?,
        utime: num(14)?,
        stime: num(15)?,
        nice: rest.get(16)?.parse().ok()?,
        num_threads: num(20)?,
        starttime: num(22)?,
        rss: num(24).unwrap_or(0),
        processor: num(39).unwrap_or(0) as u32,
    })
}

pub fn stat(pid: i32) -> Option<Stat> {
    parse_stat(&slurp(&format!("/proc/{pid}/stat"))?)
}

pub fn task_stat(pid: i32, tid: i32) -> Option<Stat> {
    parse_stat(&slurp(&format!("/proc/{pid}/task/{tid}/stat"))?)
}

/// (on-CPU ns, runnable-but-waiting ns)
pub fn parse_schedstat(s: &str) -> Option<(u64, u64)> {
    let mut it = s.split_ascii_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

pub fn task_schedstat(pid: i32, tid: i32) -> Option<(u64, u64)> {
    parse_schedstat(&slurp(&format!("/proc/{pid}/task/{tid}/schedstat"))?)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Syscall {
    /// On CPU or runnable.
    Running,
    /// Blocked outside any system call (page fault, signal stop).
    Outside,
    In { nr: i64, args: [u64; 6] },
}

pub fn parse_syscall(s: &str) -> Option<Syscall> {
    let s = s.trim();
    if s.starts_with("running") {
        return Some(Syscall::Running);
    }
    let mut it = s.split_ascii_whitespace();
    let nr: i64 = it.next()?.parse().ok()?;
    if nr < 0 {
        return Some(Syscall::Outside);
    }
    let mut args = [0u64; 6];
    for a in args.iter_mut() {
        let t = it.next()?;
        *a = u64::from_str_radix(t.trim_start_matches("0x"), 16).ok()?;
    }
    Some(Syscall::In { nr, args })
}

pub fn pids() -> Vec<i32> {
    numeric_entries("/proc")
}

pub fn tids(pid: i32) -> Vec<i32> {
    numeric_entries(&format!("/proc/{pid}/task"))
}

fn numeric_entries(dir: &str) -> Vec<i32> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Some(n) = e.file_name().to_str().and_then(|s| s.parse().ok()) {
                v.push(n);
            }
        }
    }
    v
}

pub fn cmdline(pid: i32) -> String {
    let raw = slurp(&format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let s: String = raw
        .split('\0')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

pub fn comm(pid: i32) -> Option<String> {
    Some(slurp(&format!("/proc/{pid}/comm"))?.trim_end().to_string())
}

/// "name" plus a shortened command line, for naming a process in output.
pub fn label(pid: i32) -> String {
    let c = comm(pid).unwrap_or_else(|| "?".into());
    let cl = cmdline(pid);
    if cl.is_empty() || cl == c {
        return c;
    }
    // Keep it readable: "/usr/bin/python3 /srv/app/run.py" -> "python3 …/run.py".
    let words: Vec<String> = cl
        .split(' ')
        .enumerate()
        .map(|(i, w)| {
            let base = w.rsplit('/').next().unwrap_or(w);
            if i == 0 {
                base.to_string()
            } else if w.starts_with('/') && w.matches('/').count() > 2 && !base.is_empty() {
                format!("…/{base}")
            } else {
                w.to_string()
            }
        })
        .collect();
    shorten(&words.join(" "), 60)
}

pub fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

pub fn status_field(pid: i32, key: &str) -> Option<String> {
    let s = slurp(&format!("/proc/{pid}/status"))?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(v) = rest.strip_prefix(':') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Parse a CPU list such as "0-3,8,10-11".
pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut v = Vec::new();
    for part in s.trim().split(',') {
        let mut it = part.splitn(2, '-');
        let a = it.next().and_then(|x| x.trim().parse::<u32>().ok());
        let b = it.next().and_then(|x| x.trim().parse::<u32>().ok());
        match (a, b) {
            (Some(a), Some(b)) if b >= a && b - a < 4096 => v.extend(a..=b),
            (Some(a), None) => v.push(a),
            _ => {}
        }
    }
    v
}

pub fn children(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut have = false;
    for tid in tids(pid) {
        if let Some(s) = slurp(&format!("/proc/{pid}/task/{tid}/children")) {
            have = true;
            out.extend(s.split_ascii_whitespace().filter_map(|x| x.parse::<i32>().ok()));
        }
    }
    if !have {
        for p in pids() {
            if stat(p).map(|s| s.ppid == pid).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_nasty_comm() {
        let line = "4242 (a b) c)) S 1 4242 4242 0 -1 4194560 100 0 7 0 12 34 0 0 20 5 3 0 9999 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0";
        let s = parse_stat(line).unwrap();
        assert_eq!(s.comm, "a b) c)");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.majflt, 7);
        assert_eq!((s.utime, s.stime), (12, 34));
        assert_eq!(s.nice, 5);
        assert_eq!(s.num_threads, 3);
        assert_eq!(s.starttime, 9999);
        assert_eq!(s.rss, 250);
        assert_eq!(s.processor, 2);
        assert!(!s.is_kthread());
    }

    #[test]
    fn stat_garbage_is_none() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("1 (x) S").is_none());
    }

    #[test]
    fn syscall_forms() {
        assert_eq!(parse_syscall("running\n"), Some(Syscall::Running));
        assert_eq!(parse_syscall("-1 0x7ffd 0x7f00\n"), Some(Syscall::Outside));
        match parse_syscall("0 0x3 0x7f168eb17000 0x20000 0x22 0x0 0x0 0x7ffd 0x7f16\n") {
            Some(Syscall::In { nr: 0, args }) => {
                assert_eq!(args[0], 3);
                assert_eq!(args[2], 0x20000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(parse_cpu_list("0-2,5\n"), vec![0, 1, 2, 5]);
        assert_eq!(parse_cpu_list("7"), vec![7]);
        assert!(parse_cpu_list("").is_empty());
    }

    #[test]
    fn schedstat() {
        assert_eq!(parse_schedstat("1094486 77 1\n"), Some((1094486, 77)));
    }

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(300 * 1024 * 1024), "300 MB");
    }
}
