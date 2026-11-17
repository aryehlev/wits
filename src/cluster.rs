//! wits across machines: a node agent (`wits agent`) that answers for the
//! processes on its node, and the client side that follows blame from a
//! process here to the process on another node that it is waiting for.
//!
//! There is no central server. Every agent knows its peers (a list, or a
//! DNS name that resolves to all of them, such as a headless Service), and
//! asks all of them "who holds the other end of this connection?". Only the
//! owner's agent answers yes, and it explains that process the same way.

use crate::answer::{Answer, Blame, Upstream};
use crate::http;
use crate::json::{self, Value};
use crate::out::{self, esc};
use crate::{k8s, linux};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

pub const PORT: u16 = 7477;
/// How many agents a question may pass through.
const MAX_HOPS: u32 = 3;

#[derive(Clone)]
pub struct Config {
    /// Peer agents, as given: "host:port" entries, each of which may
    /// resolve to many addresses.
    pub peers: Vec<String>,
    pub token: Option<String>,
}

impl Config {
    /// From --peers, else WITS_PEERS; the token from WITS_TOKEN or the file
    /// named by WITS_TOKEN_FILE.
    pub fn load(peers: Option<&str>) -> Config {
        let list = peers
            .map(str::to_string)
            .or_else(|| std::env::var("WITS_PEERS").ok())
            .unwrap_or_default();
        let peers = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.rsplit_once(':').map(|(_, p)| p.parse::<u16>().is_ok()).unwrap_or(false)
                    && !s.ends_with(']')
                {
                    s.to_string()
                } else {
                    format!("{s}:{PORT}")
                }
            })
            .collect();
        let token = std::env::var("WITS_TOKEN")
            .ok()
            .or_else(|| {
                std::env::var("WITS_TOKEN_FILE")
                    .ok()
                    .and_then(|f| std::fs::read_to_string(f).ok())
            })
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Config { peers, token }
    }

    /// Every peer address, resolved now (a headless Service changes as
    /// nodes come and go). Own addresses are left out.
    fn addrs(&self) -> (Vec<(SocketAddr, String)>, Vec<String>) {
        let mut out = Vec::new();
        let mut errs = Vec::new();
        let own = own_ips();
        for p in &self.peers {
            match p.to_socket_addrs() {
                Ok(it) => {
                    for a in it {
                        if !own.contains(&a.ip()) && !out.iter().any(|(b, _)| *b == a) {
                            out.push((a, p.clone()));
                        }
                    }
                }
                Err(e) => errs.push(format!("{p}: {e}")),
            }
        }
        (out, errs)
    }
}

/// This machine's addresses in wits's own network namespace.
fn own_ips() -> Vec<IpAddr> {
    k8s::local_ips(std::process::id() as i32)
}

/// Ask every peer at once; return the first answer `accept` takes, and the
/// errors from the others.
fn ask_all(
    cfg: &Config,
    path: &str,
    timeout: Duration,
    accept: fn(&Value) -> bool,
) -> (Option<(String, Value)>, usize, Vec<String>) {
    let (mut got, n, errs) = ask(cfg, path, timeout, accept, true);
    (got.pop(), n, errs)
}

/// Ask every peer at once; the answers `accept` takes (only the first one
/// when `first`), how many peers were asked, and the errors.
fn ask(
    cfg: &Config,
    path: &str,
    timeout: Duration,
    accept: fn(&Value) -> bool,
    first: bool,
) -> (Vec<(String, Value)>, usize, Vec<String>) {
    let (addrs, mut errs) = cfg.addrs();
    let n = addrs.len();
    let (tx, rx) = mpsc::channel();
    for (a, host) in addrs {
        let (tx, path, token) = (tx.clone(), path.to_string(), cfg.token.clone());
        std::thread::spawn(move || {
            let r = http::get_addr(a, &host, &path, token.as_deref(), timeout).and_then(
                |(code, body)| match code {
                    200 => Ok(body),
                    401 => Err(format!("{a}: the token was refused")),
                    c => Err(format!("{a}: HTTP {c}")),
                },
            );
            let _ = tx.send(r);
        });
    }
    drop(tx);
    let mut got = Vec::new();
    for r in rx.iter().take(n) {
        match r {
            Ok(body) => {
                if let Ok(v) = json::parse(&body) {
                    if accept(&v) {
                        got.push((body, v));
                    }
                }
            }
            Err(e) => errs.push(e),
        }
        if first && !got.is_empty() {
            break;
        }
    }
    (got, n, errs)
}

/// Every other node's processes for a pod or workload target.
pub fn group_remote(
    cfg: &Config,
    target: &str,
    secs: f64,
    stacks: bool,
) -> (Vec<crate::group::Remote>, Vec<String>) {
    let path = format!(
        "/v1/group?target={}&secs={secs}&stacks={}",
        http::encode(target),
        stacks as u8
    );
    let timeout = Duration::from_secs_f64(secs + 15.0);
    let (got, _, errs) = ask(cfg, &path, timeout, |v| v.get("found").bool() == Some(true), false);
    (
        got.iter().map(|(_, v)| crate::group::remote_of(target, v)).collect(),
        errs,
    )
}

/// Explain a target on one named node, following its blame onward.
pub fn explain_on(
    cfg: &Config,
    node: &str,
    target: &str,
    secs: f64,
    stacks: bool,
) -> Result<(String, Value), String> {
    let path = format!(
        "/v1/explain?target={}&node={}&secs={secs}&stacks={}&hops=1",
        http::encode(target),
        http::encode(node),
        stacks as u8
    );
    let timeout = Duration::from_secs_f64(secs * f64::from(MAX_HOPS) + 15.0);
    match ask_all(cfg, &path, timeout, |v| v.get("found").bool() == Some(true)) {
        (Some(x), _, _) => Ok(x),
        (None, _, errs) => Err(if errs.is_empty() {
            format!("the agent on {node} could not explain {target}")
        } else {
            errs.join("; ")
        }),
    }
}

/// If the answer blames a connection to another machine, find out from the
/// peers which process is at the other end and what it is waiting for.
pub fn follow(ans: &mut Answer, cfg: &Config, secs: f64, stacks: bool, hops: u32) {
    if cfg.peers.is_empty() || hops >= MAX_HOPS {
        return;
    }
    let Some(b) = ans.blame.as_mut() else { return };
    let Some((l, r)) = b.conn else { return };
    if b.kind != "remote" {
        return;
    }
    let path = format!(
        "/v1/follow?lip={}&lport={}&rip={}&rport={}&secs={secs}&stacks={}&hops={}",
        http::encode(&l.ip().to_string()),
        l.port(),
        http::encode(&r.ip().to_string()),
        r.port(),
        stacks as u8,
        hops + 1
    );
    let timeout = Duration::from_secs_f64(secs * f64::from(MAX_HOPS - hops) + 15.0);
    let (got, asked, errs) = ask_all(cfg, &path, timeout, |v| v.get("found").bool() == Some(true));
    match got {
        Some((_, v)) => {
            let node = v.get("node").str().unwrap_or("?").to_string();
            let a = v.get("answer").clone();
            let pid = a.get("pid").int().map(|p| p as i32);
            let name = a.get("name").str().map(str::to_string);
            let pod = a.get("pod").str().map(str::to_string);
            let who = match (&pod, &name, pid) {
                (Some(p), Some(n), Some(pid)) => format!("pid {pid} ({n}, pod {p})"),
                (None, Some(n), Some(pid)) => format!("pid {pid} ({n})"),
                _ => "a process".into(),
            };
            *b = Blame {
                kind: "process",
                pid,
                name,
                text: format!("{who} on node {node}"),
                node: Some(node.clone()),
                pod,
                conn: b.conn,
                upstream: Some(Box::new(Upstream { node, answer: a })),
            };
        }
        None => {
            let why = if asked == 0 {
                "no peer agents could be reached".to_string()
            } else {
                format!("none of {asked} wits agents holds the other end")
            };
            b.text.push_str(&format!(" ({why})"));
            if !errs.is_empty() {
                ans.notes.push(format!("could not ask: {}", errs.join("; ")));
            }
        }
    }
}

/// `wits <ns/pod>` for a pod on another node: whichever agent has it answers.
pub fn explain_remote(cfg: &Config, target: &str, secs: f64, stacks: bool) -> Result<(String, Value), String> {
    if cfg.peers.is_empty() {
        return Err("no peers to ask (set WITS_PEERS or --peers)".into());
    }
    let path = format!(
        "/v1/explain?target={}&secs={secs}&stacks={}&hops=1",
        http::encode(target),
        stacks as u8
    );
    let timeout = Duration::from_secs_f64(secs * f64::from(MAX_HOPS) + 15.0);
    let (got, asked, errs) = ask_all(cfg, &path, timeout, |v| v.get("found").bool() == Some(true));
    match got {
        Some((body, v)) => Ok((body, v)),
        None if !errs.is_empty() => Err(format!(
            "'{target}' is not on this node, and {} of {asked} peers could not be asked: {}",
            errs.len(),
            errs.join("; ")
        )),
        None => Err(format!("'{target}' runs on none of the {asked} nodes with a wits agent")),
    }
}

fn param<'a>(r: &'a http::Request, k: &str) -> Option<&'a str> {
    r.query.get(k).map(String::as_str)
}

fn opts_of(r: &http::Request) -> (f64, bool, u32) {
    let secs = param(r, "secs")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .unwrap_or(2.0)
        .clamp(0.2, 30.0);
    let stacks = param(r, "stacks") != Some("0");
    let hops = param(r, "hops").and_then(|h| h.parse().ok()).unwrap_or(1);
    (secs, stacks, hops)
}

/// Explain one pid for a peer, following its own blame onward.
fn explain_pid(pid: i32, cfg: &Config, secs: f64, stacks: bool, hops: u32) -> Option<(String, String)> {
    let opts = linux::Opts {
        dur: Duration::from_secs_f64(secs),
        follow: true,
        light: false,
        stacks,
    };
    let (mut ans, _) = linux::analyze_full(pid, opts)?;
    follow(&mut ans, cfg, secs, stacks, hops);
    let plain = out::Style { color: false };
    Some((out::json(&ans), out::text(&ans, &plain)))
}

fn found(node: &str, answer: &str, text: &str) -> String {
    format!(
        "{{\"found\":true,\"node\":{},\"answer\":{answer},\"text\":{}}}",
        esc(node),
        esc(text)
    )
}

fn handle(r: &http::Request, cfg: &Config, node: &str) -> (u16, String) {
    if let Some(t) = &cfg.token {
        let ok = r
            .headers
            .get("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|h| constant_eq(h.trim().as_bytes(), t.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return (401, "{\"error\":\"token\"}".into());
        }
    }
    let not_found = || (200, format!("{{\"found\":false,\"node\":{}}}", esc(node)));
    match r.path.as_str() {
        "/v1/health" => (
            200,
            format!(
                "{{\"wits\":{},\"node\":{}}}",
                esc(env!("CARGO_PKG_VERSION")),
                esc(node)
            ),
        ),
        // Who holds the other end of the asker's connection?
        "/v1/follow" => {
            let ep = |ip: &str, port: &str| -> Option<(IpAddr, u16)> {
                Some((param(r, ip)?.parse().ok()?, param(r, port)?.parse().ok()?))
            };
            let (peer, dest) = (ep("lip", "lport"), ep("rip", "rport"));
            if peer.is_none() && dest.is_none() {
                return (400, "{\"error\":\"lip/lport or rip/rport\"}".into());
            }
            let (secs, stacks, hops) = opts_of(r);
            let Some(pid) = k8s::tcp_owner(peer, dest) else {
                return not_found();
            };
            match explain_pid(pid, cfg, secs, stacks, hops) {
                Some((j, t)) => (200, found(node, &j, &t)),
                None => not_found(),
            }
        }
        // This node's processes for a pod or workload target.
        "/v1/group" => {
            let Some(t) = param(r, "target") else {
                return (400, "{\"error\":\"target\"}".into());
            };
            if !t.contains('/') {
                return (400, "{\"error\":\"target must be namespace/name\"}".into());
            }
            let (secs, stacks, _) = opts_of(r);
            (200, crate::group::serve(t, secs, stacks, node))
        }
        // Explain a pid, a name or a pod on this node (only on the node
        // named, when one is: pids mean different processes elsewhere).
        "/v1/explain" => {
            let Some(t) = param(r, "target") else {
                return (400, "{\"error\":\"target\"}".into());
            };
            if param(r, "node").is_some_and(|n| n != node) {
                return not_found();
            }
            let (secs, stacks, hops) = opts_of(r);
            let pid = match t.parse::<i32>() {
                Ok(p) if p > 0 => linux::check(p).ok().map(|_| p),
                _ if t.contains('/') => k8s::pod_pids(t).ok().map(|v| v[0]),
                _ => linux::resolve(t).ok().map(|x| x.0),
            };
            let Some(pid) = pid else { return not_found() };
            match explain_pid(pid, cfg, secs, stacks, hops) {
                Some((j, t)) => (200, found(node, &j, &t)),
                None => not_found(),
            }
        }
        _ => (404, "{\"error\":\"not found\"}".into()),
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `wits agent`: answer peers' questions about this node.
pub fn agent(listen: Option<&str>, peers: Option<&str>) -> i32 {
    let cfg = Config::load(peers);
    let addr = listen
        .map(str::to_string)
        .or_else(|| std::env::var("WITS_LISTEN").ok())
        .unwrap_or_else(|| format!("127.0.0.1:{PORT}"));
    let loopback = addr
        .to_socket_addrs()
        .map(|mut it| it.all(|a| a.ip().is_loopback()))
        .unwrap_or(false);
    if cfg.token.is_none() && !loopback {
        crate::die(
            &format!("refusing to listen on {addr} without a token: anyone who can reach it could read this node's processes. Set WITS_TOKEN or WITS_TOKEN_FILE"),
            2,
        );
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("wits agent: not root, so it sees only this user's processes and no eBPF stacks");
    }
    let node = k8s::node_name();
    eprintln!(
        "wits agent {} on node {node}, listening on {addr}; peers: {}",
        env!("CARGO_PKG_VERSION"),
        if cfg.peers.is_empty() { "none".to_string() } else { cfg.peers.join(", ") }
    );
    let cfg = std::sync::Arc::new(cfg);
    let r = http::serve(&addr, 8, move |req| handle(req, &cfg, &node));
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("wits agent: {addr}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_get_the_default_port() {
        std::env::remove_var("WITS_PEERS");
        let c = Config::load(Some("a, b:9000,[::1]:7, 10.0.0.1"));
        assert_eq!(c.peers, vec!["a:7477", "b:9000", "[::1]:7", "10.0.0.1:7477"]);
    }

    #[test]
    fn tokens_compare() {
        assert!(constant_eq(b"abc", b"abc"));
        assert!(!constant_eq(b"abc", b"abd"));
        assert!(!constant_eq(b"abc", b"ab"));
    }
}
