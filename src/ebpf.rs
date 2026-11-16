//! Stack collection with eBPF, for root (or CAP_BPF + CAP_PERFMON):
//!
//! - on CPU: a CPU-clock perf event on every CPU samples the running
//!   thread's user and kernel stacks;
//! - off CPU: the sched_switch tracepoint records the stack of each thread
//!   of interest as it leaves the CPU, and the time until it comes back,
//!   with the state it left in (runnable, sleeping, uninterruptible);
//! - sched_process_fork adds the children of a watched process, so a whole
//!   process tree (`wits run`) is covered.
//!
//! The kernel walks stacks by frame pointer; code built without them shows
//! short stacks, and wits says so.

use crate::bpf::*;

const MAX_DEPTH: u32 = 127;

pub struct Session {
    /// Which pids to profile; the fork program adds children to it.
    _targets: Map,
    stacks: Map,
    oncpu: Map,
    start: Map,
    offcpu: Map,
    _progs: Vec<Prog>,
    _events: Vec<PerfEvent>,
    pub freq: u64,
    pub started_ns: u64,
    depth: usize,
}

/// One stack and what it cost.
#[derive(Debug, Clone)]
pub struct RawStack {
    pub tgid: i32,
    pub tid: i32,
    pub user: Vec<u64>,
    pub kernel: Vec<u64>,
    /// None: on CPU. Some(state): off CPU, having left in that task state
    /// (0 runnable, 1 sleeping, 2 uninterruptible...).
    pub off_state: Option<u32>,
    pub ns: u64,
    /// arm64 link register at the sample (0 if unknown): the caller of a
    /// leaf function.
    pub lr: u64,
}

pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn raise_memlock() {
    // Kernels before 5.11 charge BPF maps against RLIMIT_MEMLOCK.
    let r = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &r) };
}

/// Jump to `out` unless the current process is one we watch (or we watch
/// everything: key 0). Expects the tgid in R8; clobbers R1-R5 and stack
/// slot -4.
fn filter(a: &mut Asm, targets: &Map, out: &'static str, hit: &'static str) {
    a.stx(Sz::W, R10, -4, R8)
        .ld_map(R1, targets)
        .stack_ptr(R2, -4)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Ne, R0, 0, hit)
        .st(Sz::W, R10, -4, 0)
        .ld_map(R1, targets)
        .stack_ptr(R2, -4)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Eq, R0, 0, out)
        .label(hit);
}

/// On-CPU sampler (perf_event program).
fn prog_oncpu(targets: &Map, stacks: &Map, oncpu: &Map) -> Vec<Insn> {
    let mut a = Asm::new();
    a.mov(R6, R1)
        .call(FN_GET_CURRENT_PID_TGID)
        .mov(R7, R0)
        .mov32(R7, R7)
        .mov(R8, R0)
        .rsh_imm(R8, 32)
        // the idle task
        .jmp_imm(Cond::Eq, R7, 0, "out");
    filter(&mut a, targets, "out", "hit");
    // key {tgid, tid, ustack, kstack, lr} at -24
    a.stx(Sz::W, R10, -24, R8)
        .stx(Sz::W, R10, -20, R7)
        .mov(R1, R6)
        .ld_map(R2, stacks)
        .mov_imm(R3, F_USER_STACK)
        .call(FN_GET_STACKID)
        .stx(Sz::W, R10, -16, R0)
        .mov(R1, R6)
        .ld_map(R2, stacks)
        .mov_imm(R3, 0)
        .call(FN_GET_STACKID)
        .stx(Sz::W, R10, -12, R0);
    // On arm64 a leaf function keeps its return address in the link
    // register, which the frame-pointer walk never sees: record it.
    if cfg!(target_arch = "aarch64") {
        a.ldx(Sz::Dw, R1, R6, 30 * 8).stx(Sz::Dw, R10, -8, R1);
    } else {
        a.st(Sz::Dw, R10, -8, 0);
    }
    a.ld_map(R1, oncpu)
        .stack_ptr(R2, -24)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Eq, R0, 0, "insert")
        .mov_imm(R1, 1)
        .xadd64(R0, 0, R1)
        .ja("out")
        .label("insert")
        .st(Sz::Dw, R10, -32, 1)
        .ld_map(R1, oncpu)
        .stack_ptr(R2, -24)
        .stack_ptr(R3, -32)
        .mov_imm(R4, 1) // BPF_NOEXIST
        .call(FN_MAP_UPDATE)
        .label("out")
        .mov_imm(R0, 0)
        .exit();
    a.finish()
}

/// Off-CPU accounting (sched_switch tracepoint program). With `lr`, also
/// record the user link register from the task's saved registers (arm64,
/// kernel 5.15+): the caller of a frameless system call wrapper.
fn prog_offcpu(
    targets: &Map,
    stacks: &Map,
    start: &Map,
    offcpu: &Map,
    prev_state_off: i16,
    next_pid_off: i16,
    lr: bool,
) -> Vec<Insn> {
    let mut a = Asm::new();
    a.mov(R6, R1)
        .call(FN_KTIME_GET_NS)
        .mov(R9, R0)
        // Part 1: the current task is the one leaving the CPU.
        .call(FN_GET_CURRENT_PID_TGID)
        .mov(R7, R0)
        .mov32(R7, R7)
        .mov(R8, R0)
        .rsh_imm(R8, 32)
        .jmp_imm(Cond::Eq, R7, 0, "next");
    filter(&mut a, targets, "next", "hit");
    // start value {ts u64, ustack, kstack, tgid, state, lr u64} at -40
    a.stx(Sz::Dw, R10, -40, R9)
        .mov(R1, R6)
        .ld_map(R2, stacks)
        .mov_imm(R3, F_USER_STACK)
        .call(FN_GET_STACKID)
        .stx(Sz::W, R10, -32, R0)
        .mov(R1, R6)
        .ld_map(R2, stacks)
        .mov_imm(R3, 0)
        .call(FN_GET_STACKID)
        .stx(Sz::W, R10, -28, R0)
        .stx(Sz::W, R10, -24, R8)
        .ldx(Sz::W, R1, R6, prev_state_off)
        .stx(Sz::W, R10, -20, R1)
        .st(Sz::Dw, R10, -16, 0);
    if lr {
        a.call(FN_GET_CURRENT_TASK_BTF)
            .mov(R1, R0)
            .call(FN_TASK_PT_REGS)
            .mov(R3, R0)
            .add_imm(R3, 30 * 8)
            .stack_ptr(R1, -16)
            .mov_imm(R2, 8)
            .call(FN_PROBE_READ_KERNEL);
    }
    a.stx(Sz::W, R10, -4, R7)
        .ld_map(R1, start)
        .stack_ptr(R2, -4)
        .stack_ptr(R3, -40)
        .mov_imm(R4, 0)
        .call(FN_MAP_UPDATE)
        // Part 2: the task coming onto the CPU ends its off-CPU span.
        .label("next")
        .ldx(Sz::W, R1, R6, next_pid_off)
        .stx(Sz::W, R10, -4, R1)
        .ld_map(R1, start)
        .stack_ptr(R2, -4)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Eq, R0, 0, "out")
        .mov(R7, R0)
        .ldx(Sz::Dw, R1, R7, 0)
        .mov(R8, R9)
        .sub(R8, R1)
        // key {tgid, tid, ustack, kstack, state, pad, lr u64} at -72
        .ldx(Sz::W, R1, R7, 16)
        .stx(Sz::W, R10, -72, R1)
        .ldx(Sz::W, R1, R10, -4)
        .stx(Sz::W, R10, -68, R1)
        .ldx(Sz::W, R1, R7, 8)
        .stx(Sz::W, R10, -64, R1)
        .ldx(Sz::W, R1, R7, 12)
        .stx(Sz::W, R10, -60, R1)
        .ldx(Sz::W, R1, R7, 20)
        .stx(Sz::W, R10, -56, R1)
        .st(Sz::W, R10, -52, 0)
        .ldx(Sz::Dw, R1, R7, 24)
        .stx(Sz::Dw, R10, -48, R1)
        .ld_map(R1, start)
        .stack_ptr(R2, -4)
        .call(FN_MAP_DELETE)
        .ld_map(R1, offcpu)
        .stack_ptr(R2, -72)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Eq, R0, 0, "insert")
        .xadd64(R0, 0, R8)
        .mov_imm(R1, 1)
        .xadd64(R0, 8, R1)
        .ja("out")
        .label("insert")
        .stx(Sz::Dw, R10, -88, R8)
        .st(Sz::Dw, R10, -80, 1)
        .ld_map(R1, offcpu)
        .stack_ptr(R2, -72)
        .stack_ptr(R3, -88)
        .mov_imm(R4, 1)
        .call(FN_MAP_UPDATE)
        .label("out")
        .mov_imm(R0, 0)
        .exit();
    a.finish()
}

/// Follow forks (sched_process_fork tracepoint program).
fn prog_fork(targets: &Map, child_pid_off: i16) -> Vec<Insn> {
    let mut a = Asm::new();
    a.mov(R6, R1)
        .call(FN_GET_CURRENT_PID_TGID)
        .rsh_imm(R0, 32)
        .stx(Sz::W, R10, -4, R0)
        .ld_map(R1, targets)
        .stack_ptr(R2, -4)
        .call(FN_MAP_LOOKUP)
        .jmp_imm(Cond::Eq, R0, 0, "out")
        .ldx(Sz::W, R1, R6, child_pid_off)
        .stx(Sz::W, R10, -8, R1)
        .st(Sz::W, R10, -12, 1)
        .ld_map(R1, targets)
        .stack_ptr(R2, -8)
        .stack_ptr(R3, -12)
        .mov_imm(R4, 0)
        .call(FN_MAP_UPDATE)
        .label("out")
        .mov_imm(R0, 0)
        .exit();
    a.finish()
}

fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[o..o + 8]);
    u64::from_ne_bytes(x)
}

/// Why eBPF is not available, in words for the user; None if it might be.
pub fn unavailable() -> Option<String> {
    if unsafe { libc::geteuid() } != 0 {
        return Some("needs root (sudo) for eBPF".into());
    }
    // eBPF sees host pids; inside a PID namespace /proc shows others.
    let nested = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("NSpid:"))
                .map(|l| l.split_whitespace().count() > 2)
        })
        .unwrap_or(false);
    if nested {
        return Some("eBPF needs the host PID namespace (in Kubernetes: hostPID: true)".into());
    }
    None
}

impl Session {
    /// Profile these processes (and, with `follow_forks`, their children).
    pub fn start(pids: &[i32], follow_forks: bool, freq: u64) -> Result<Session, String> {
        if let Some(why) = unavailable() {
            return Err(why);
        }
        raise_memlock();
        let depth = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_stack")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(MAX_DEPTH)
            .clamp(16, MAX_DEPTH);
        let e = |what: &'static str| move |err: std::io::Error| format!("eBPF {what}: {err}");
        let targets = Map::create(MAP_HASH, 4, 4, 65536, "wits_targets").map_err(e("map"))?;
        let stacks =
            Map::create(MAP_STACK_TRACE, 4, depth * 8, 16384, "wits_stacks").map_err(e("map"))?;
        let oncpu = Map::create(MAP_HASH, 24, 8, 65536, "wits_oncpu").map_err(e("map"))?;
        let start = Map::create(MAP_HASH, 4, 32, 65536, "wits_start").map_err(e("map"))?;
        let offcpu = Map::create(MAP_HASH, 32, 16, 65536, "wits_offcpu").map_err(e("map"))?;
        pids.iter()
            .try_for_each(|pid| targets.update(&(*pid as u32).to_ne_bytes(), &1u32.to_ne_bytes()))
            .map_err(e("map"))?;

        let (sw_id, sw_off) = tracepoint("sched", "sched_switch", &["prev_state", "next_pid"])
            .ok_or("eBPF: cannot read the sched_switch tracepoint format (is tracefs available?)")?;
        let p_on = Prog::load(PROG_PERF_EVENT, &prog_oncpu(&targets, &stacks, &oncpu), "wits_oncpu")
            .map_err(|m| format!("eBPF load: {m}"))?;
        let off = |lr| prog_offcpu(&targets, &stacks, &start, &offcpu, sw_off[0], sw_off[1], lr);
        let p_off = if cfg!(target_arch = "aarch64") {
            Prog::load(PROG_TRACEPOINT, &off(true), "wits_offcpu")
                .or_else(|_| Prog::load(PROG_TRACEPOINT, &off(false), "wits_offcpu"))
        } else {
            Prog::load(PROG_TRACEPOINT, &off(false), "wits_offcpu")
        }
        .map_err(|m| format!("eBPF load: {m}"))?;
        let fork = if follow_forks {
            let (id, off) = tracepoint("sched", "sched_process_fork", &["child_pid"])
                .ok_or("eBPF: cannot read the sched_process_fork tracepoint format")?;
            let p = Prog::load(PROG_TRACEPOINT, &prog_fork(&targets, off[0]), "wits_fork")
                .map_err(|m| format!("eBPF load: {m}"))?;
            Some((id, p))
        } else {
            None
        };

        let mut events = Vec::new();
        let started_ns = monotonic_ns();
        let cpus = online_cpus();
        // A tracepoint runs its programs on every CPU: one event is enough
        // (and attaching the same program twice is refused).
        let first = cpus.first().copied().unwrap_or(0);
        let ev = PerfEvent::open(PERF_TYPE_TRACEPOINT, sw_id, None, first).map_err(e("perf"))?;
        ev.attach(&p_off).map_err(e("attach"))?;
        events.push(ev);
        if let Some((id, p)) = &fork {
            let ev = PerfEvent::open(PERF_TYPE_TRACEPOINT, *id, None, first).map_err(e("perf"))?;
            ev.attach(p).map_err(e("attach"))?;
            events.push(ev);
        }
        for cpu in cpus {
            let ev = PerfEvent::open(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, Some(freq), cpu)
                .map_err(e("perf"))?;
            ev.attach(&p_on).map_err(e("attach"))?;
            events.push(ev);
        }
        let mut progs = vec![p_on, p_off];
        progs.extend(fork.map(|f| f.1));
        Ok(Session {
            _targets: targets,
            stacks,
            oncpu,
            start,
            offcpu,
            _progs: progs,
            _events: events,
            freq,
            started_ns,
            depth: depth as usize,
        })
    }

    fn stack(&self, id: i32, cache: &mut std::collections::HashMap<i32, Vec<u64>>) -> Vec<u64> {
        if id < 0 {
            return Vec::new();
        }
        cache
            .entry(id)
            .or_insert_with(|| {
                self.stacks
                    .lookup(&id.to_ne_bytes())
                    .map(|b| {
                        (0..self.depth)
                            .map(|i| u64_at(&b, i * 8))
                            .take_while(|&ip| ip != 0)
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .clone()
    }

    /// Everything recorded so far. Threads still off CPU are counted up to
    /// now, from when they left (or from the start of the session).
    pub fn collect(&self) -> Vec<RawStack> {
        let mut cache = std::collections::HashMap::new();
        let mut out = Vec::new();
        for (k, v) in self.oncpu.entries() {
            let (us, ks) = (i32_at(&k, 8), i32_at(&k, 12));
            out.push(RawStack {
                tgid: i32_at(&k, 0),
                tid: i32_at(&k, 4),
                user: self.stack(us, &mut cache),
                kernel: self.stack(ks, &mut cache),
                off_state: None,
                ns: u64_at(&v, 0) * 1_000_000_000 / self.freq.max(1),
                lr: u64_at(&k, 16),
            });
        }
        for (k, v) in self.offcpu.entries() {
            let (us, ks) = (i32_at(&k, 8), i32_at(&k, 12));
            out.push(RawStack {
                tgid: i32_at(&k, 0),
                tid: i32_at(&k, 4),
                user: self.stack(us, &mut cache),
                kernel: self.stack(ks, &mut cache),
                off_state: Some(i32_at(&k, 16) as u32),
                ns: u64_at(&v, 0),
                lr: u64_at(&k, 24),
            });
        }
        let now = monotonic_ns();
        for (k, v) in self.start.entries() {
            let ts = u64_at(&v, 0).max(self.started_ns);
            let (us, ks) = (i32_at(&v, 8), i32_at(&v, 12));
            out.push(RawStack {
                tgid: i32_at(&v, 16),
                tid: i32_at(&k, 0),
                user: self.stack(us, &mut cache),
                kernel: self.stack(ks, &mut cache),
                off_state: Some(i32_at(&v, 20) as u32),
                ns: now.saturating_sub(ts),
                lr: u64_at(&v, 24),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The programs assemble with every jump resolved; loading them needs
    /// root and is exercised by cases/profile-*.sh.
    #[test]
    fn programs_assemble() {
        // Map fds are only embedded, never used, when assembling.
        let fake = || Map::create(MAP_HASH, 4, 4, 1, "t");
        if let (Ok(t), Ok(s), Ok(o), Ok(st), Ok(off)) = (fake(), fake(), fake(), fake(), fake()) {
            assert!(prog_oncpu(&t, &s, &o).len() > 20);
            assert!(prog_offcpu(&t, &s, &st, &off, 32, 56, true).len() > 40);
            assert!(prog_fork(&t, 44).len() > 10);
        }
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    /// Run as root: `sudo cargo test -- --ignored ebpf_programs_load`.
    #[test]
    #[ignore]
    fn ebpf_programs_load() {
        let t = Map::create(MAP_HASH, 4, 4, 16, "t").unwrap();
        let s = Map::create(MAP_STACK_TRACE, 4, 127 * 8, 16, "s").unwrap();
        let o = Map::create(MAP_HASH, 24, 8, 16, "o").unwrap();
        let st = Map::create(MAP_HASH, 4, 32, 16, "st").unwrap();
        let off = Map::create(MAP_HASH, 32, 16, 16, "off").unwrap();
        Prog::load(PROG_PERF_EVENT, &prog_oncpu(&t, &s, &o), "on").unwrap();
        Prog::load(PROG_TRACEPOINT, &prog_offcpu(&t, &s, &st, &off, 32, 56, false), "off").unwrap();
        Prog::load(PROG_TRACEPOINT, &prog_fork(&t, 44), "fork").unwrap();
        if cfg!(target_arch = "aarch64") {
            Prog::load(PROG_TRACEPOINT, &prog_offcpu(&t, &s, &st, &off, 32, 56, true), "off_lr").unwrap();
        }
    }
}
