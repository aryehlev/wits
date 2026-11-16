//! System-wide context: who else is using CPU and disk, how busy the block
//! devices are, swap activity, and the cgroup limits around a process.

use crate::proc;
use std::collections::HashMap;
use std::time::Instant;

pub struct ProcSnap {
    pub cpu_ticks: u64,
    pub rss_pages: u64,
    pub kthread: bool,
    /// (read_bytes, write_bytes); only visible for our own processes.
    pub io: Option<(u64, u64)>,
}

pub struct SysSnap {
    pub at: Instant,
    pub procs: HashMap<i32, ProcSnap>,
    /// (major, minor) -> (name, io_ticks ms)
    pub disks: HashMap<(u32, u32), (String, u64)>,
    pub pswpin: u64,
    pub pswpout: u64,
}

fn parse_io(s: &str) -> Option<(u64, u64)> {
    let mut r = None;
    let mut w = None;
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("read_bytes:") {
            r = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix("write_bytes:") {
            w = v.trim().parse().ok();
        }
    }
    Some((r?, w?))
}

pub fn parse_diskstats(s: &str) -> HashMap<(u32, u32), (String, u64)> {
    let mut m = HashMap::new();
    for l in s.lines() {
        let f: Vec<&str> = l.split_ascii_whitespace().collect();
        if f.len() < 13 {
            continue;
        }
        if let (Ok(ma), Ok(mi), Ok(t)) = (f[0].parse(), f[1].parse(), f[12].parse()) {
            m.insert((ma, mi), (f[2].to_string(), t));
        }
    }
    m
}

fn vm_field(s: &str, key: &str) -> u64 {
    s.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
        .unwrap_or(0)
}

impl SysSnap {
    pub fn take() -> SysSnap {
        let mut procs = HashMap::new();
        for pid in proc::pids() {
            if let Some(st) = proc::stat(pid) {
                let io = proc::slurp(&format!("/proc/{pid}/io")).and_then(|s| parse_io(&s));
                procs.insert(
                    pid,
                    ProcSnap {
                        kthread: st.is_kthread(),
                        cpu_ticks: st.utime + st.stime,
                        rss_pages: st.rss,
                        io,
                    },
                );
            }
        }
        let vm = proc::slurp("/proc/vmstat").unwrap_or_default();
        SysSnap {
            at: Instant::now(),
            procs,
            disks: parse_diskstats(&proc::slurp("/proc/diskstats").unwrap_or_default()),
            pswpin: vm_field(&vm, "pswpin"),
            pswpout: vm_field(&vm, "pswpout"),
        }
    }
}

/// Differences between two snapshots.
pub struct SysDelta {
    pub secs: f64,
    /// pid -> CPU seconds used in the window
    pub cpu: Vec<(i32, f64)>,
    /// pid -> (bytes read, bytes written)
    pub io: HashMap<i32, (u64, u64)>,
    /// (major, minor) -> (name, fraction of the window the device was busy)
    pub disk_util: HashMap<(u32, u32), (String, f64)>,
    pub swapped_in_pages: u64,
    pub swapped_out_pages: u64,
    pub rss_pages: HashMap<i32, u64>,
}

impl SysDelta {
    pub fn between(a: &SysSnap, b: &SysSnap) -> SysDelta {
        let secs = b.at.duration_since(a.at).as_secs_f64().max(1e-3);
        let tck = proc::clk_tck() as f64;
        let mut cpu = Vec::new();
        let mut io = HashMap::new();
        let mut rss_pages = HashMap::new();
        for (pid, pb) in &b.procs {
            if !pb.kthread {
                rss_pages.insert(*pid, pb.rss_pages);
            }
            let pa = a.procs.get(pid);
            let base = pa.map(|p| p.cpu_ticks).unwrap_or(0);
            let d = pb.cpu_ticks.saturating_sub(base) as f64 / tck;
            if d > 0.0 {
                cpu.push((*pid, d));
            }
            if let Some((r1, w1)) = pb.io {
                let (r0, w0) = pa.and_then(|p| p.io).unwrap_or((0, 0));
                let d = (r1.saturating_sub(r0), w1.saturating_sub(w0));
                if d.0 + d.1 > 0 {
                    io.insert(*pid, d);
                }
            }
        }
        cpu.sort_by(|x, y| y.1.total_cmp(&x.1));
        let mut disk_util = HashMap::new();
        for (k, (name, t1)) in &b.disks {
            let t0 = a.disks.get(k).map(|d| d.1).unwrap_or(*t1);
            let u = (t1.saturating_sub(t0) as f64 / 1000.0 / secs).min(1.0);
            disk_util.insert(*k, (name.clone(), u));
        }
        SysDelta {
            secs,
            cpu,
            io,
            disk_util,
            swapped_in_pages: b.pswpin.saturating_sub(a.pswpin),
            swapped_out_pages: b.pswpout.saturating_sub(a.pswpout),
            rss_pages,
        }
    }

    pub fn busiest_disk(&self) -> Option<(&str, f64)> {
        self.disk_util
            .values()
            .filter(|(n, _)| !n.starts_with("loop") && !n.starts_with("ram"))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(n, u)| (n.as_str(), *u))
    }
}

pub fn mem_total_bytes() -> u64 {
    proc::slurp("/proc/meminfo")
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("MemTotal:")?.split_ascii_whitespace().next()?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024
}

/// (tasks runnable right now, online CPUs)
pub fn load_now() -> (u32, u32) {
    let running = proc::slurp("/proc/loadavg")
        .and_then(|s| s.split_ascii_whitespace().nth(3)?.split('/').next()?.parse::<u32>().ok())
        .unwrap_or(0);
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    (running.saturating_sub(1), if cpus > 0 { cpus as u32 } else { 1 })
}

// ---- cgroups -------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CgLevel {
    /// cgroup path as the process sees it, e.g. "/user.slice/foo.scope"
    pub name: String,
    pub dir: String,
    pub v2: bool,
}

/// Directories to inspect for one controller, from the process's own cgroup
/// up to (not including) the root.
pub fn cgroup_chain(pid: i32, controller: &str) -> Vec<CgLevel> {
    let Some(cg) = proc::slurp(&format!("/proc/{pid}/cgroup")) else {
        return Vec::new();
    };
    let mounts = proc::slurp("/proc/self/mountinfo").unwrap_or_default();
    let mut v1_path = None;
    let mut v2_path = None;
    for l in cg.lines() {
        let mut it = l.splitn(3, ':');
        let (Some(_), Some(ctl), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if ctl.is_empty() {
            v2_path = Some(path.to_string());
        } else if ctl.split(',').any(|c| c == controller) {
            v1_path = Some(path.to_string());
        }
    }
    let mut v1_mount = None;
    let mut v2_mount = None;
    for l in mounts.lines() {
        let Some((left, right)) = l.split_once(" - ") else {
            continue;
        };
        let lf: Vec<&str> = left.split(' ').collect();
        let rf: Vec<&str> = right.split(' ').collect();
        if lf.len() < 5 || rf.len() < 3 {
            continue;
        }
        if rf[0] == "cgroup2" && v2_mount.is_none() {
            v2_mount = Some((lf[4].to_string(), lf[3].to_string()));
        } else if rf[0] == "cgroup" && rf[2].split(',').any(|o| o == controller) && v1_mount.is_none() {
            v1_mount = Some((lf[4].to_string(), lf[3].to_string()));
        }
    }
    let (path, (mount, root), v2) = match (v1_path, v1_mount, v2_path, v2_mount) {
        (Some(p), Some(m), _, _) => (p, m, false),
        (_, _, Some(p), Some(m)) => (p, m, true),
        _ => return Vec::new(),
    };
    // The mount may expose only a subtree (cgroup namespaces, bind mounts).
    let rel = if root != "/" {
        path.strip_prefix(root.as_str()).unwrap_or(&path).to_string()
    } else {
        path
    };
    let mut out = Vec::new();
    let mut cur = rel.trim_end_matches('/').to_string();
    while !cur.is_empty() && cur != "/" {
        out.push(CgLevel { name: cur.clone(), dir: format!("{mount}{cur}"), v2 });
        match cur.rfind('/') {
            Some(i) => cur.truncate(i),
            None => break,
        }
    }
    out
}

fn kv(s: &str, key: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// Nanoseconds each cgroup level has spent throttled so far.
pub fn throttled_ns(chain: &[CgLevel]) -> Vec<u64> {
    chain
        .iter()
        .map(|c| {
            let s = proc::slurp(&format!("{}/cpu.stat", c.dir)).unwrap_or_default();
            if c.v2 {
                kv(&s, "throttled_usec").unwrap_or(0) * 1000
            } else {
                kv(&s, "throttled_time").unwrap_or(0)
            }
        })
        .collect()
}

/// CPU limit of a cgroup, in CPUs.
pub fn cpu_limit(c: &CgLevel) -> Option<f64> {
    if c.v2 {
        let s = proc::slurp(&format!("{}/cpu.max", c.dir))?;
        let mut it = s.split_ascii_whitespace();
        let q: f64 = it.next()?.parse().ok()?;
        let p: f64 = it.next()?.parse().ok()?;
        Some(q / p)
    } else {
        let q: f64 = proc::slurp(&format!("{}/cpu.cfs_quota_us", c.dir))?.trim().parse().ok()?;
        let p: f64 = proc::slurp(&format!("{}/cpu.cfs_period_us", c.dir))?.trim().parse().ok()?;
        if q <= 0.0 || p <= 0.0 {
            None
        } else {
            Some(q / p)
        }
    }
}

/// How many times each cgroup level has hit a memory limit so far.
pub fn mem_limit_hits(chain: &[CgLevel]) -> Vec<u64> {
    chain
        .iter()
        .map(|c| {
            if c.v2 {
                let s = proc::slurp(&format!("{}/memory.events", c.dir)).unwrap_or_default();
                kv(&s, "high").unwrap_or(0) + kv(&s, "max").unwrap_or(0)
            } else {
                proc::slurp(&format!("{}/memory.failcnt", c.dir))
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0)
            }
        })
        .collect()
}

/// (current usage, limit) in bytes for a memory cgroup, if it has a limit.
pub fn mem_limit(c: &CgLevel) -> Option<(u64, u64)> {
    let rd = |f: &str| -> Option<u64> { proc::slurp(&format!("{}/{}", c.dir, f))?.trim().parse().ok() };
    if c.v2 {
        let lim = [rd("memory.high"), rd("memory.max")].into_iter().flatten().min()?;
        Some((rd("memory.current")?, lim))
    } else {
        let lim = rd("memory.limit_in_bytes")?;
        if lim > (1 << 60) {
            return None;
        }
        Some((rd("memory.usage_in_bytes")?, lim))
    }
}

// ---- file locks ----------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct LockLine {
    pub waiting: bool,
    pub kind: String,
    pub pid: i32,
    pub major: u32,
    pub minor: u32,
    pub ino: u64,
}

pub fn parse_locks(s: &str) -> Vec<LockLine> {
    let mut out = Vec::new();
    for l in s.lines() {
        let mut f: Vec<&str> = l.split_ascii_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        f.remove(0);
        let waiting = f[0] == "->";
        if waiting {
            f.remove(0);
        }
        if f.len() < 5 {
            continue;
        }
        let ids: Vec<&str> = f[4].split(':').collect();
        if ids.len() != 3 {
            continue;
        }
        let (Ok(pid), Ok(major), Ok(minor), Ok(ino)) = (
            f[3].parse(),
            u32::from_str_radix(ids[0], 16),
            u32::from_str_radix(ids[1], 16),
            ids[2].parse(),
        ) else {
            continue;
        };
        out.push(LockLine { waiting, kind: f[0].to_string(), pid, major, minor, ino });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks() {
        let s = "1: FLOCK  ADVISORY  WRITE 174 fe:00:679940 0 EOF\n1: -> FLOCK  ADVISORY  WRITE 200 fe:00:679940 0 EOF\n2: POSIX  ADVISORY  READ 9 08:01:12 0 99\n";
        let v = parse_locks(s);
        assert_eq!(v.len(), 3);
        assert_eq!(
            v[0],
            LockLine { waiting: false, kind: "FLOCK".into(), pid: 174, major: 0xfe, minor: 0, ino: 679940 }
        );
        assert!(v[1].waiting);
        assert_eq!(v[1].pid, 200);
        assert_eq!((v[2].major, v[2].minor, v[2].ino), (8, 1, 12));
    }

    #[test]
    fn diskstats() {
        let s = " 254       0 vda 100 0 2000 50 200 0 4000 70 0 1234 120 0 0 0 0\n   7       0 loop0 1 2 3\n";
        let m = parse_diskstats(s);
        assert_eq!(m.get(&(254, 0)), Some(&("vda".to_string(), 1234)));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn io() {
        assert_eq!(parse_io("rchar: 1\nread_bytes: 4096\nwrite_bytes: 8192\n"), Some((4096, 8192)));
        assert_eq!(parse_io(""), None);
    }
}
