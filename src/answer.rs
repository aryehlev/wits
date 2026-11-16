//! The answer wits gives, shared by every platform: the cause, its share of
//! the process's time, the specific object, and who is to blame.

use crate::code::{CodeView, Stacks};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Copy)]
pub struct Opts {
    pub dur: Duration,
    pub follow: bool,
    /// Skip system-wide scans and blame lookups (used when describing a
    /// blamed process in passing).
    pub light: bool,
    /// Collect stacks to say where in the code the time goes.
    pub stacks: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cat {
    Cpu,
    CpuWait,
    Disk,
    Memory,
    Lock,
    Peer,
    Idle,
}

pub const CAUSES: [Cat; 6] = [
    Cat::Cpu,
    Cat::CpuWait,
    Cat::Disk,
    Cat::Memory,
    Cat::Lock,
    Cat::Peer,
];

impl Cat {
    pub fn key(self) -> &'static str {
        match self {
            Cat::Cpu => "cpu",
            Cat::CpuWait => "cpu-wait",
            Cat::Disk => "disk",
            Cat::Memory => "memory",
            Cat::Lock => "lock",
            Cat::Peer => "peer",
            Cat::Idle => "idle",
        }
    }

    /// "<name> is ..."
    pub fn headline(self) -> &'static str {
        match self {
            Cat::Cpu => "busy on CPU, not blocked",
            Cat::CpuWait => "waiting for a CPU",
            Cat::Disk => "waiting for disk",
            Cat::Memory => "short of memory",
            Cat::Lock => "waiting for a lock",
            Cat::Peer => "waiting for someone else",
            Cat::Idle => "idle",
        }
    }

    pub fn state(self) -> &'static str {
        match self {
            Cat::Cpu => "working",
            Cat::Peer => "waiting",
            Cat::Idle => "idle",
            _ => "stalled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Blame {
    /// "self" | "process" | "thread" | "cgroup" | "remote" | "device" | "unknown"
    pub kind: &'static str,
    pub pid: Option<i32>,
    pub name: Option<String>,
    pub text: String,
    /// The node the blamed process runs on, when it is another node.
    pub node: Option<String>,
    /// "namespace/pod/container", when the blamed process is in a pod.
    pub pod: Option<String>,
    /// The TCP connection waited on (local, remote), for asking other nodes.
    pub conn: Option<(std::net::SocketAddr, std::net::SocketAddr)>,
    /// What the blamed process is itself waiting for, as explained by the
    /// wits agent on its node.
    pub upstream: Option<Box<Upstream>>,
}

/// An answer from another node's wits agent.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub node: String,
    /// Its answer, as JSON.
    pub answer: crate::json::Value,
}

impl Blame {
    pub fn new(kind: &'static str, text: String) -> Blame {
        Blame {
            kind,
            text,
            ..Blame::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub pid: i32,
    pub name: String,
    pub cmdline: String,
    /// Parents wits walked through because they were only waiting for a child.
    pub followed: Vec<(i32, String)>,
    pub window_ms: u64,
    pub threads: usize,
    pub samples: u32,
    /// "working" | "stalled" | "waiting" | "idle" | "stopped" | "unknown"
    pub state: &'static str,
    pub cause: Option<Cat>,
    /// Share of the process's non-idle time taken by the cause (0..1).
    pub share: f64,
    /// Non-idle thread time divided by the window: 1.0 is one busy thread.
    pub active: f64,
    pub what: String,
    pub blame: Option<Blame>,
    /// Share of non-idle time for each of the six causes.
    pub breakdown: Vec<(Cat, f64)>,
    /// Fraction of all thread time that was idle, and what the idle time was.
    pub idle: f64,
    pub idle_what: String,
    pub confidence: &'static str,
    pub notes: Vec<String>,
    /// Where in the program the time went, when stacks could be collected.
    pub code: Option<CodeView>,
    /// "namespace/pod/container" when the process runs in a Kubernetes pod.
    pub pod: Option<String>,
}

/// One line of the `wits` overview.
pub struct Row {
    pub pid: i32,
    pub name: String,
    /// Time spent waiting for something (not idle, not on CPU), as a fraction
    /// of the window, summed over threads.
    pub waiting: f64,
    pub answer: Option<Answer>,
}

/// One process seen while watching a process tree.
#[derive(Clone, Debug)]
pub struct TreeProc {
    /// 0 for the "(short-lived processes)" entry, which is not one process.
    pub pid: i32,
    pub name: String,
    /// CPU time (user + system) seen while watched.
    pub cpu_ns: f64,
    /// Thread time spent in each cause (and `Cat::Idle`).
    pub by_cat: HashMap<Cat, f64>,
}

/// What a whole process tree did, from start to exit (`wits run`).
#[derive(Debug, Default)]
pub struct Tree {
    /// Thread time per cause, summed over every thread of every process.
    /// `Cat::Idle` holds time asleep or waiting for a child in the tree.
    pub by_cat: HashMap<Cat, f64>,
    pub procs: Vec<TreeProc>,
    pub stacks: Option<Stacks>,
    pub notes: Vec<String>,
}
