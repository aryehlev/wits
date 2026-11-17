//! Containers and Kubernetes pods on a Linux node, without the API server:
//!
//! - which pod and container a process belongs to, from its cgroup path and
//!   the kubelet's log directories (/var/log/pods, /var/log/containers);
//! - which process on this node owns a TCP connection, in any network
//!   namespace (every pod has its own).

use crate::net::{self, Addr, Sock};
use crate::proc;
use std::collections::HashMap;
use std::net::IpAddr;

/// Where the host's /var/log is: the same path, or under a mount such as
/// /host when wits runs in a container (WITS_HOST_ROOT).
fn host_root() -> String {
    if let Ok(r) = std::env::var("WITS_HOST_ROOT") {
        return r.trim_end_matches('/').to_string();
    }
    if std::path::Path::new("/host/var/log/pods").exists() {
        return "/host".into();
    }
    String::new()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pod {
    pub namespace: String,
    pub name: String,
    pub container: String,
}

impl std::fmt::Display for Pod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.container.is_empty() {
            write!(f, "{}/{}", self.namespace, self.name)
        } else {
            write!(f, "{}/{}/{}", self.namespace, self.name, self.container)
        }
    }
}

/// Pod UID and container id from a cgroup path, for the cgroupfs and
/// systemd drivers and the usual runtimes.
pub fn parse_cgroup(path: &str) -> Option<(String, String)> {
    let i = path.find("kubepods")?;
    let rest = &path[i..];
    let mut uid = None;
    let mut cid = String::new();
    for part in rest.split('/') {
        // "pod<uid>" or "kubepods-burstable-pod<uid_with_underscores>.slice"
        if let Some(p) = part.rfind("pod") {
            let cand = part[p + 3..].trim_end_matches(".slice").replace('_', "-");
            if cand.len() == 36 && cand.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                uid = Some(cand);
                continue;
            }
        }
        if uid.is_some() && !part.is_empty() {
            let c = part.trim_end_matches(".scope");
            let c = c.rsplit('-').next().unwrap_or(c);
            if c.len() >= 12 && c.chars().all(|ch| ch.is_ascii_hexdigit()) {
                cid = c.to_string();
            }
        }
    }
    uid.map(|u| (u, cid))
}

#[derive(Default)]
pub struct Index {
    /// pod uid -> (namespace, pod name)
    pods: HashMap<String, (String, String)>,
    /// container id -> container name
    containers: HashMap<String, String>,
    cache: HashMap<String, Option<Pod>>,
}

impl Index {
    /// Read the kubelet's log directories once.
    pub fn load() -> Index {
        let root = host_root();
        let mut ix = Index::default();
        // /var/log/pods/<namespace>_<pod>_<uid>/<container>/
        if let Ok(rd) = std::fs::read_dir(format!("{root}/var/log/pods")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                let parts: Vec<&str> = n.splitn(3, '_').collect();
                if let [ns, pod, uid] = parts.as_slice() {
                    ix.pods.insert(uid.to_string(), (ns.to_string(), pod.to_string()));
                }
            }
        }
        // /var/log/containers/<pod>_<namespace>_<container>-<id>.log
        if let Ok(rd) = std::fs::read_dir(format!("{root}/var/log/containers")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = n.strip_suffix(".log") else { continue };
                let Some((front, id)) = stem.rsplit_once('-') else { continue };
                let parts: Vec<&str> = front.splitn(3, '_').collect();
                if let [_, _, container] = parts.as_slice() {
                    ix.containers.insert(id.to_string(), container.to_string());
                }
            }
        }
        ix
    }

    /// The pod a process runs in, if any.
    pub fn pod_of(&mut self, pid: i32) -> Option<Pod> {
        let cg = proc::slurp(&format!("/proc/{pid}/cgroup"))?;
        let line = cg
            .lines()
            .find(|l| l.contains("kubepods"))?
            .splitn(3, ':')
            .nth(2)?
            .to_string();
        if let Some(p) = self.cache.get(&line) {
            return p.clone();
        }
        let p = parse_cgroup(&line).and_then(|(uid, cid)| {
            let (ns, name) = match self.pods.get(&uid) {
                Some(x) => x.clone(),
                None => from_environ(pid)?,
            };
            let container = self
                .containers
                .iter()
                .find(|(id, _)| !cid.is_empty() && (id.starts_with(&cid) || cid.starts_with(id.as_str())))
                .map(|x| x.1.clone())
                .unwrap_or_default();
            Some(Pod {
                namespace: ns,
                name,
                container,
            })
        });
        self.cache.insert(line, p.clone());
        p
    }
}

/// Pod name from the container's HOSTNAME, namespace from its service
/// account mount: for nodes without the kubelet log layout.
fn from_environ(pid: i32) -> Option<(String, String)> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let name = env
        .split(|&b| b == 0)
        .find_map(|kv| kv.strip_prefix(b"HOSTNAME="))
        .map(|v| String::from_utf8_lossy(v).into_owned())?;
    let ns = proc::slurp(&format!(
        "/proc/{pid}/root/var/run/secrets/kubernetes.io/serviceaccount/namespace"
    ))
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|| "?".into());
    Some((ns, name))
}

/// A container's processes: (pid, CPU ticks so far, threads).
type Procs = Vec<(i32, u64, i64)>;

/// What a pod target matched on this node.
pub struct PodMatch {
    /// A pod with exactly the name asked for (pod names are unique in a
    /// namespace), rather than the pods of a workload of that name.
    pub exact: bool,
    /// Each pod (its label without the container), with the processes
    /// worth explaining.
    pub pods: Vec<(String, Vec<i32>)>,
    /// Pods left out to keep the work bounded.
    pub more: usize,
}

/// The pods "namespace/name[/container]" names on this node: the pod
/// called `name`, or else the pods of the Deployment, StatefulSet,
/// DaemonSet or Job called `name`. Per pod, up to `per_pod` processes,
/// taking each container's busiest before any container's second, so a
/// busy sidecar does not crowd out the application. Pause containers are
/// left out.
pub fn match_pods(spec: &str, per_pod: usize, max_pods: usize) -> Result<PodMatch, String> {
    let parts: Vec<&str> = spec.split('/').collect();
    let (ns, name, container) = match parts.as_slice() {
        [ns, n] if !ns.is_empty() && !n.is_empty() => (*ns, *n, None),
        [ns, n, c] if !ns.is_empty() && !n.is_empty() => (*ns, *n, Some(*c)),
        _ => return Err(format!("'{spec}': give a pod as namespace/name[/container]")),
    };
    let mut ix = Index::load();
    // (pod name, container) -> its processes
    let mut found: HashMap<(String, String), Procs> = HashMap::new();
    for pid in proc::pids() {
        let Some(st) = proc::stat(pid) else { continue };
        if st.state == 'Z' || st.is_kthread() || st.comm == "pause" {
            continue;
        }
        let Some(p) = ix.pod_of(pid) else { continue };
        if p.namespace != ns || container.is_some_and(|c| c != p.container) {
            continue;
        }
        if p.name == name || is_replica(&p.name, name) {
            found
                .entry((p.name, p.container))
                .or_default()
                .push((pid, st.utime + st.stime, st.num_threads as i64));
        }
    }
    let exact = found.keys().any(|(p, _)| p == name);
    if exact {
        found.retain(|(p, _), _| p == name);
    }
    let mut pods: HashMap<String, Vec<(String, Procs)>> = HashMap::new();
    for ((pod, c), mut procs) in found {
        procs.sort_by_key(|x| std::cmp::Reverse((x.1, x.2)));
        pods.entry(pod).or_default().push((c, procs));
    }
    if pods.is_empty() {
        return Err(format!("no process of pod '{spec}' runs on this node"));
    }
    let mut out: Vec<(String, Vec<i32>)> = pods
        .into_iter()
        .map(|(pod, mut cs)| {
            cs.sort_by(|a, b| b.1[0].1.cmp(&a.1[0].1).then(a.0.cmp(&b.0)));
            let mut pick = Vec::new();
            for round in 0.. {
                let before = pick.len();
                for (_, procs) in &cs {
                    if let Some(p) = procs.get(round) {
                        if pick.len() < per_pod {
                            pick.push(p.0);
                        }
                    }
                }
                if pick.len() == before || pick.len() >= per_pod {
                    break;
                }
            }
            (format!("{ns}/{pod}"), pick)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let more = out.len().saturating_sub(max_pods);
    out.truncate(max_pods);
    Ok(PodMatch { exact, pods: out, more })
}

/// Whether `pod` is a name Kubernetes gives a pod of workload `w`:
/// w-<template hash>-<5> for a Deployment, w-<5> for a DaemonSet, Job or
/// ReplicaSet, w-<ordinal> for a StatefulSet, w-<time>-<5> for a CronJob.
/// Generated parts use Kubernetes' vowel-free alphabet, which keeps
/// `api` from matching a Deployment called `api-gateway`.
pub fn is_replica(pod: &str, w: &str) -> bool {
    let Some(rest) = pod.strip_prefix(w).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    let safe = |s: &str| s.bytes().all(|c| b"bcdfghjklmnpqrstvwxz2456789".contains(&c));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let suffix = |s: &str| s.len() == 5 && safe(s);
    match rest.split('-').collect::<Vec<_>>().as_slice() {
        [r] => suffix(r) || digits(r),
        [h, r] => suffix(r) && (6..=10).contains(&h.len()) && (safe(h) || digits(h)),
        _ => false,
    }
}

/// The processes of one pod, for a caller that wants a single process.
pub fn pod_pids(spec: &str) -> Result<Vec<i32>, String> {
    let m = match_pods(spec, 8, 1)?;
    Ok(m.pods.into_iter().flat_map(|p| p.1).collect())
}

/// "namespace/pod/container" for a process in a pod; None elsewhere, after
/// one read of its cgroup file.
pub fn pod_label(pid: i32) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    static IX: OnceLock<Mutex<Index>> = OnceLock::new();
    let cg = proc::slurp(&format!("/proc/{pid}/cgroup"))?;
    if !cg.contains("kubepods") {
        return None;
    }
    let ix = IX.get_or_init(|| Mutex::new(Index::load()));
    let mut ix = ix.lock().ok()?;
    ix.pod_of(pid).map(|p| p.to_string())
}

/// The node's name: NODE_NAME (set from the downward API in the DaemonSet),
/// else the host name.
pub fn node_name() -> String {
    std::env::var("NODE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| proc::slurp("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "?".into())
}

fn netns(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/ns/net"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

type Ep = (IpAddr, u16);

/// The addresses of the network namespace `pid` is in.
pub fn local_ips(pid: i32) -> Vec<IpAddr> {
    let mut v = Vec::new();
    if let Some(t) = proc::slurp(&format!("/proc/{pid}/net/fib_trie")) {
        let lines: Vec<&str> = t.lines().collect();
        for w in lines.windows(2) {
            if w[1].trim() == "/32 host LOCAL" {
                if let Ok(ip) = w[0].trim().trim_start_matches("|-- ").parse() {
                    v.push(ip);
                }
            }
        }
    }
    if let Some(t) = proc::slurp(&format!("/proc/{pid}/net/if_inet6")) {
        for l in t.lines() {
            let h = l.split_whitespace().next().unwrap_or("");
            if let Ok(n) = u128::from_str_radix(h, 16) {
                v.push(IpAddr::V6(std::net::Ipv6Addr::from(n)));
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn ep_eq(a: &Addr, b: Ep) -> bool {
    a.port == b.1 && a.ip() == b.0
}

/// Which process on this node, in any network namespace, holds the other
/// end of a TCP connection. `peer` is the asker's own endpoint, which the
/// other end sees as its remote address (unless it was SNATed); `dest` is
/// where the asker connected to, which may be a Service address the other
/// end never sees (DNAT). Falls back to a listener on `dest`.
pub fn tcp_owner(peer: Option<Ep>, dest: Option<Ep>) -> Option<i32> {
    let mut by_ns: HashMap<String, Vec<i32>> = HashMap::new();
    for pid in proc::pids() {
        if let Some(n) = netns(pid) {
            by_ns.entry(n).or_default().push(pid);
        }
    }
    let tables: Vec<(Vec<i32>, HashMap<u64, Sock>)> = by_ns
        .into_values()
        .map(|pids| {
            let socks = net::sockets(pids[0]);
            (pids, socks)
        })
        .collect();
    let holder = |pids: &[i32], want: &[u64]| -> Option<i32> {
        let want: Vec<String> = want.iter().map(|i| format!("socket:[{i}]")).collect();
        for &pid in pids {
            let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            for e in rd.flatten() {
                if let Ok(l) = std::fs::read_link(e.path()) {
                    if want.iter().any(|w| l.as_os_str() == w.as_str()) {
                        return Some(pid);
                    }
                }
            }
        }
        None
    };
    let find = |pass: &dyn Fn(&Addr, &Addr, bool) -> bool| -> Vec<(usize, Vec<u64>)> {
        tables
            .iter()
            .enumerate()
            .filter_map(|(i, (_, socks))| {
                let inos: Vec<u64> = socks
                    .iter()
                    .filter_map(|(ino, s)| match s {
                        Sock::Tcp {
                            local, remote, listening, ..
                        } if pass(local, remote, *listening) => Some(*ino),
                        _ => None,
                    })
                    .collect();
                (!inos.is_empty()).then_some((i, inos))
            })
            .collect()
    };
    // An established connection whose far end is the asker: exact first,
    // then through NAT on the destination side.
    if let Some(p) = peer {
        let exact = |l: &Addr, r: &Addr, lis: bool| {
            !lis && ep_eq(r, p) && dest.map(|d| ep_eq(l, d)).unwrap_or(true)
        };
        let natted = |_: &Addr, r: &Addr, lis: bool| !lis && ep_eq(r, p);
        for hits in [find(&exact), find(&natted)] {
            for (i, inos) in hits {
                if let Some(pid) = holder(&tables[i].0, &inos) {
                    return Some(pid);
                }
            }
        }
    }
    // Still connecting: a listener on the destination, in the namespace
    // that has that address (not any pod listening on the same port).
    if let Some(d) = dest {
        let lis = |l: &Addr, _: &Addr, lis: bool| {
            lis && l.port == d.1 && (l.ip() == d.0 || l.ip().is_unspecified())
        };
        for (i, inos) in find(&lis) {
            let pids = &tables[i].0;
            let bound = inos.iter().any(|ino| {
                matches!(tables[i].1.get(ino), Some(Sock::Tcp { local, .. }) if local.ip() == d.0)
            });
            if bound || local_ips(pids[0]).contains(&d.0) {
                if let Some(pid) = holder(pids, &inos) {
                    return Some(pid);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_paths() {
        let (u, c) = parse_cgroup("/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod4f1b2c3d_1111_2222_3333_444455556666.slice/cri-containerd-0123456789abcdef0123456789abcdef.scope").unwrap();
        assert_eq!(u, "4f1b2c3d-1111-2222-3333-444455556666");
        assert_eq!(c, "0123456789abcdef0123456789abcdef");
        let (u, c) = parse_cgroup("/kubepods/besteffort/pod4f1b2c3d-1111-2222-3333-444455556666/0123456789abcdef0123").unwrap();
        assert_eq!(u, "4f1b2c3d-1111-2222-3333-444455556666");
        assert_eq!(c, "0123456789abcdef0123");
        assert!(parse_cgroup("/user.slice/user-1000.slice/session-2.scope").is_none());
    }

    #[test]
    fn replicas_by_name() {
        assert!(is_replica("web-7d9f8b6c5-x2k4q", "web")); // Deployment
        assert!(is_replica("web-x2k4q", "web")); // DaemonSet, Job
        assert!(is_replica("pg-0", "pg")); // StatefulSet
        assert!(is_replica("report-28374650-b5n8w", "report")); // CronJob
        assert!(!is_replica("web-gateway-6c8d9f7b5-q9w2x", "web"));
        assert!(!is_replica("web-v2-x2k4q", "web"));
        assert!(!is_replica("webapp-x2k4q", "web"));
        assert!(!is_replica("web", "web"));
    }

    #[test]
    fn pod_display() {
        let p = Pod {
            namespace: "db".into(),
            name: "pg-0".into(),
            container: "postgres".into(),
        };
        assert_eq!(p.to_string(), "db/pg-0/postgres");
    }
}
