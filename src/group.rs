//! Several processes at once: the replicas of a Deployment, the
//! containers of a pod, or several targets given together. Each is
//! explained over the same window, on this node and, through the agents,
//! on the others; the busiest is explained in full.

use crate::answer::{Answer, Opts};
use crate::cluster::{self, Config};
use crate::json::{self, Value};
use crate::out::{self, esc, pct, shorten, Style};
use crate::explain::analyze_many;
use crate::{k8s, linux};
use std::collections::HashMap;
use std::time::Duration;

/// Pods per node, and processes per pod, that are explained at most.
pub const MAX_PODS: usize = 16;
const PER_POD_ONE: usize = 8;
const PER_POD_MANY: usize = 3;

pub struct Item {
    pub node: String,
    /// The pod, without the container: rows are per pod when there are
    /// several pods.
    pub pod: Option<String>,
    pub answer: Value,
    pub text: String,
    /// The answer itself when it was made here, to follow its blame.
    local: Option<Answer>,
}

impl Item {
    /// Busiest first: the share of time spent not idle, and among equals
    /// the share not on CPU (a replica that is stuck before one that is
    /// computing).
    fn rank(&self) -> (f64, f64) {
        let a = &self.answer;
        let active = a.get("active").num().unwrap_or(0.0);
        let cpu = a.get("breakdown").get("cpu").num().unwrap_or(0.0);
        (active, 1.0 - cpu)
    }
}

/// What the targets matched here.
pub struct Local {
    pub items: Vec<Item>,
    /// Some target named a workload rather than one pod or process.
    pub workload: bool,
    /// Targets that may also (or only) have processes on other nodes.
    pub elsewhere: Vec<String>,
    /// Targets that are a pod found here by its exact name: the other
    /// nodes cannot have it.
    pub exact: Vec<String>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

/// One node's items for one target, from its agent.
pub struct Remote {
    pub target: String,
    pub node: String,
    /// The target is a pod there, by its exact name.
    pub exact: bool,
    pub items: Vec<(Option<String>, Value, String)>,
    pub notes: Vec<String>,
}

/// Explain every process the targets name on this node.
pub fn local(targets: &[String], opts: Opts, st: &Style) -> Local {
    let mut want: Vec<(i32, Option<String>)> = Vec::new();
    let mut res = Local {
        items: Vec::new(),
        workload: false,
        elsewhere: Vec::new(),
        exact: Vec::new(),
        notes: Vec::new(),
        errors: Vec::new(),
    };
    for t in targets {
        if let Ok(p) = t.parse::<i32>() {
            match linux::check(p) {
                Ok(()) => want.push((p, None)),
                Err(e) => res.errors.push(e),
            }
        } else if t.contains('/') && !t.starts_with('/') {
            match k8s::match_pods(t, PER_POD_ONE, MAX_PODS) {
                Ok(m) => {
                    if m.exact {
                        res.exact.push(t.clone());
                    } else {
                        res.workload = true;
                        res.elsewhere.push(t.clone());
                    }
                    if m.more > 0 {
                        res.notes.push(format!(
                            "{} more pods of '{t}' on this node were not explained",
                            m.more
                        ));
                    }
                    for (pod, pids) in m.pods {
                        want.extend(pids.into_iter().map(|p| (p, Some(pod.clone()))));
                    }
                }
                Err(e) => {
                    res.elsewhere.push(t.clone());
                    res.errors.push(e);
                }
            }
        } else {
            match linux::resolve(t) {
                Ok((p, _)) => want.push((p, None)),
                Err(e) => res.errors.push(e),
            }
        }
    }
    // With several pods, a few processes from each (every container's
    // busiest first), so the replicas are compared, not their workers.
    let pods: Vec<&String> = {
        let mut v: Vec<&String> = want.iter().filter_map(|w| w.1.as_ref()).collect();
        v.dedup();
        v
    };
    if pods.len() > 1 {
        let mut n: HashMap<String, usize> = HashMap::new();
        want.retain(|(_, pod)| match pod {
            Some(p) => {
                let c = n.entry(p.clone()).or_default();
                *c += 1;
                *c <= PER_POD_MANY
            }
            None => true,
        });
    }
    let pids: Vec<i32> = want.iter().map(|w| w.0).collect();
    let node = k8s::node_name();
    for ((_, pod), r) in want.into_iter().zip(analyze_many(&pids, opts)) {
        let Some((a, _)) = r else { continue };
        let answer = json::parse(&out::json(&a)).unwrap_or(Value::Null);
        res.items.push(Item {
            node: node.clone(),
            pod,
            text: out::text(&a, st),
            answer,
            local: Some(a),
        });
    }
    res
}

/// One row per pod when there are several pods: the pod's busiest process.
fn per_pod(items: Vec<Item>) -> Vec<Item> {
    let pods: std::collections::HashSet<(&str, Option<&str>)> = items
        .iter()
        .map(|i| (i.node.as_str(), i.pod.as_deref()))
        .collect();
    if pods.len() <= 1 {
        return items;
    }
    let mut best: Vec<Item> = Vec::new();
    for it in items {
        let same = best
            .iter()
            .position(|b| it.pod.is_some() && b.pod == it.pod && b.node == it.node);
        match same {
            Some(i) if best[i].rank() >= it.rank() => {}
            Some(i) => best[i] = it,
            None => best.push(it),
        }
    }
    best
}

fn sort(items: &mut [Item]) {
    items.sort_by(|a, b| {
        let (x, y) = (a.rank(), b.rank());
        y.0.total_cmp(&x.0).then(y.1.total_cmp(&x.1))
    });
}

/// The agent side of `/v1/group`: this node's items for one target.
pub fn serve(target: &str, secs: f64, stacks: bool, node: &str) -> String {
    let opts = Opts {
        dur: Duration::from_secs_f64(secs),
        follow: true,
        light: false,
        stacks,
    };
    let plain = Style { color: false };
    let l = local(&[target.to_string()], opts, &plain);
    if l.items.is_empty() {
        return format!("{{\"found\":false,\"node\":{}}}", esc(node));
    }
    let items: Vec<String> = l
        .items
        .iter()
        .map(|i| {
            format!(
                "{{\"pod\":{},\"answer\":{},\"text\":{}}}",
                i.pod.as_deref().map(esc).unwrap_or_else(|| "null".into()),
                i.answer,
                esc(&i.text)
            )
        })
        .collect();
    let notes: Vec<String> = l.notes.iter().map(|n| esc(n)).collect();
    format!(
        "{{\"found\":true,\"node\":{},\"exact\":{},\"items\":[{}],\"notes\":[{}]}}",
        esc(node),
        !l.workload,
        items.join(","),
        notes.join(",")
    )
}

/// Read one agent's reply to `/v1/group`.
pub fn remote_of(target: &str, v: &Value) -> Remote {
    Remote {
        target: target.to_string(),
        node: v.get("node").str().unwrap_or("?").to_string(),
        exact: v.get("exact").bool().unwrap_or(false),
        items: v
            .get("items")
            .arr()
            .iter()
            .map(|i| {
                (
                    i.get("pod").str().map(str::to_string),
                    i.get("answer").clone(),
                    i.get("text").str().unwrap_or("").to_string(),
                )
            })
            .collect(),
        notes: v.get("notes").arr().iter().filter_map(|n| n.str().map(str::to_string)).collect(),
    }
}

/// `wits <target> <target>...`, or one pod or workload target.
pub fn explain(targets: &[String], opts: Opts, peers: Option<&str>, json_out: bool, html: Option<&str>) {
    let st = Style::detect();
    let cfg = Config::load(peers);
    let secs = opts.dur.as_secs_f64();
    // Ask the other nodes about every pod or workload target while this
    // node is being watched.
    let asking = {
        let cfg = cfg.clone();
        let ask: Vec<String> = targets
            .iter()
            .filter(|t| t.contains('/') && !t.starts_with('/'))
            .cloned()
            .collect();
        std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut errs = Vec::new();
            if !cfg.peers.is_empty() {
                for t in ask {
                    let (r, e) = cluster::group_remote(&cfg, &t, secs, opts.stacks);
                    got.extend(r);
                    errs.extend(e);
                }
            }
            (got, errs)
        })
    };
    let mut l = local(targets, opts, &st);
    let (remote, remote_errs): (Vec<Remote>, Vec<String>) = asking.join().unwrap_or_default();
    // A pod is on one node: where it was found by name, ignore other
    // nodes' pods that only look like its replicas.
    let exact_remote: Vec<&str> = remote.iter().filter(|r| r.exact).map(|r| r.target.as_str()).collect();
    let mut workload = l.workload;
    let mut found_remote = false;
    for r in &remote {
        if l.exact.contains(&r.target) || (!r.exact && exact_remote.contains(&r.target.as_str())) {
            continue;
        }
        workload |= !r.exact;
        found_remote |= !r.items.is_empty();
        l.notes.extend(r.notes.iter().map(|n| format!("{}: {n}", r.node)));
        for (pod, answer, text) in &r.items {
            l.items.push(Item {
                node: r.node.clone(),
                pod: pod.clone(),
                answer: answer.clone(),
                text: text.clone(),
                local: None,
            });
        }
    }
    if found_remote {
        // "not on this node" is no longer news.
        l.errors.clear();
    }
    if l.items.is_empty() {
        let mut why = l.errors;
        why.extend(remote_errs);
        if why.is_empty() {
            why.push("nothing to explain".into());
        }
        crate::die(&why.join("; "), 1);
    }
    let mut items = per_pod(l.items);
    sort(&mut items);
    // Follow the busiest one's blame across nodes.
    if let Some(top) = items.first_mut() {
        if let Some(a) = top.local.as_mut() {
            cluster::follow(a, &cfg, secs, opts.stacks, 0);
            top.answer = json::parse(&out::json(a)).unwrap_or(Value::Null);
            top.text = out::text(a, &st);
        } else if top.answer.get("blame").get("kind").str() == Some("remote") {
            if let Some(pid) = top.answer.get("pid").int() {
                if let Ok((_, v)) =
                    cluster::explain_on(&cfg, &top.node, &pid.to_string(), secs, opts.stacks)
                {
                    top.answer = v.get("answer").clone();
                    top.text = v.get("text").str().unwrap_or_default().to_string();
                }
            }
        }
    }
    let mut notes = l.notes;
    notes.extend(remote_errs.into_iter().map(|e| format!("not asked: {e}")));
    if items.len() == 1 && !workload && targets.len() == 1 {
        let it = &items[0];
        if let Some(p) = html {
            crate::ui::write(p, &it.answer.to_string());
        }
        if json_out {
            println!("{}", it.answer);
        } else {
            println!("{}", it.text);
            if it.local.is_none() {
                println!("{}", st.dim(&format!("  (answered by the wits agent on node {})", it.node)));
            }
        }
        return;
    }
    let j = group_json(targets, &items, &notes, opts.dur);
    if let Some(p) = html {
        crate::ui::write(p, &j);
    }
    if json_out {
        println!("{j}");
    } else {
        println!("{}", group_text(targets, &items, &notes, opts.dur, &st));
    }
}

fn title(targets: &[String], items: &[Item], dur: Duration) -> String {
    let pods = items.iter().filter(|i| i.pod.is_some()).count();
    let mut nodes: Vec<&str> = items.iter().map(|i| i.node.as_str()).collect();
    nodes.sort();
    nodes.dedup();
    let what = if pods == items.len() && pods > 1 && per_pod_rows(items) {
        format!("{pods} pods")
    } else {
        format!("{} processes", items.len())
    };
    let on = if nodes.len() > 1 {
        format!(" on {} nodes", nodes.len())
    } else {
        String::new()
    };
    format!(
        "{}: {what}{on}, busiest first ({:.1}s)",
        targets.join(" "),
        dur.as_secs_f64()
    )
}

/// Whether each row is a different pod.
fn per_pod_rows(items: &[Item]) -> bool {
    let mut seen = std::collections::HashSet::new();
    items.iter().all(|i| seen.insert((i.node.as_str(), i.pod.as_deref())))
}

fn now(a: &Value) -> String {
    match a.get("cause").str() {
        Some(c) => format!("{c} {}", pct(a.get("share").num().unwrap_or(0.0))),
        None => a.get("state").str().unwrap_or("idle").to_string(),
    }
}

fn why(a: &Value) -> String {
    if a.get("cause").is_null() {
        return a.get("idle_what").str().unwrap_or("").to_string();
    }
    let what = a.get("what").str().unwrap_or("");
    let b = a.get("blame");
    match b.get("kind").str() {
        Some("process") | Some("cgroup") => {
            format!("{what} ← {}", b.get("text").str().unwrap_or(""))
        }
        _ => what.to_string(),
    }
}

fn group_text(targets: &[String], items: &[Item], notes: &[String], dur: Duration, st: &Style) -> String {
    let width = out::term_width();
    let mut nodes: Vec<&str> = items.iter().map(|i| i.node.as_str()).collect();
    nodes.sort();
    nodes.dedup();
    let show_node = nodes.len() > 1;
    // The namespace is said once, in the title, when all share it.
    let ns = |p: &str| p.split('/').next().unwrap_or("").to_string();
    let labels: Vec<String> = items
        .iter()
        .map(|i| match i.answer.get("pod").str() {
            Some(p) => p.to_string(),
            None => "-".into(),
        })
        .collect();
    let one_ns = {
        let mut v: Vec<String> = labels.iter().filter(|l| *l != "-").map(|l| ns(l)).collect();
        v.dedup();
        v.len() == 1
    };
    let label = |l: &str| -> String {
        if one_ns && l != "-" {
            l.split_once('/').map(|x| x.1.to_string()).unwrap_or_else(|| l.to_string())
        } else {
            l.to_string()
        }
    };
    let lw = labels.iter().map(|l| label(l).chars().count()).max().unwrap_or(3).clamp(3, 40);
    let nw = nodes.iter().map(|n| n.chars().count()).max().unwrap_or(4).clamp(4, 20);
    let mut out = vec![st.bold(&title(targets, items, dur))];
    let mut head = format!("  {:<lw$}  ", "pod");
    if show_node {
        head.push_str(&format!("{:<nw$}  ", "node"));
    }
    head.push_str(&format!("{:<22} {:<10} why", "process", "now"));
    out.push(st.dim(&head));
    for (i, l) in items.iter().zip(&labels) {
        let a = &i.answer;
        let proc = format!(
            "{} ({})",
            a.get("name").str().unwrap_or("?"),
            a.get("pid").int().unwrap_or(0)
        );
        let mut row = format!("  {:<lw$}  ", shorten(&label(l), lw));
        if show_node {
            row.push_str(&format!("{:<nw$}  ", shorten(&i.node, nw)));
        }
        row.push_str(&format!("{:<22} {:<10} ", shorten(&proc, 22), now(a)));
        let room = width.saturating_sub(row.chars().count()).max(20);
        row.push_str(&shorten(&why(a), room));
        out.push(row);
    }
    for n in notes {
        out.push(format!("  {} {n}", st.dim("note  ")));
    }
    if let Some(top) = items.first() {
        out.push(String::new());
        let who = match (top.answer.get("pod").str(), show_node) {
            (Some(p), true) => format!("{} on {}", label(p), top.node),
            (Some(p), false) => label(p),
            (None, _) => format!("pid {}", top.answer.get("pid").int().unwrap_or(0)),
        };
        out.push(st.dim(&format!("The busiest, {who}:")));
        out.push(top.text.clone());
    }
    out.join("\n")
}

fn group_json(targets: &[String], items: &[Item], notes: &[String], dur: Duration) -> String {
    let t: Vec<String> = targets.iter().map(|t| esc(t)).collect();
    let it: Vec<String> = items
        .iter()
        .map(|i| format!("{{\"node\":{},\"answer\":{}}}", esc(&i.node), i.answer))
        .collect();
    let n: Vec<String> = notes.iter().map(|x| esc(x)).collect();
    let summary = match items.first() {
        Some(top) => format!(
            "{}; the busiest: {}",
            title(targets, items, dur),
            top.answer.get("summary").str().unwrap_or("")
        ),
        None => title(targets, items, dur),
    };
    format!(
        "{{\"version\":1,\"kind\":\"group\",\"targets\":[{}],\"summary\":{},\"window_ms\":{},\"items\":[{}],\"notes\":[{}]}}",
        t.join(","),
        esc(&summary),
        dur.as_millis(),
        it.join(","),
        n.join(",")
    )
}
