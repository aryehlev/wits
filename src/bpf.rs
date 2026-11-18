//! Just enough of the bpf(2) and perf_event_open(2) interfaces to load the
//! few small programs in ebpf.rs: maps, a bytecode assembler, program
//! loading and attaching. No libbpf, no clang, no BTF: the programs are
//! written as instructions and only read tracepoint fields whose offsets
//! the kernel publishes in tracefs.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const BPF_MAP_CREATE: i32 = 0;
const BPF_MAP_LOOKUP_ELEM: i32 = 1;
const BPF_MAP_UPDATE_ELEM: i32 = 2;
const BPF_MAP_DELETE_ELEM: i32 = 3;
const BPF_MAP_GET_NEXT_KEY: i32 = 4;
const BPF_PROG_LOAD: i32 = 5;

pub const MAP_HASH: u32 = 1;
pub const MAP_ARRAY: u32 = 2;
pub const MAP_STACK_TRACE: u32 = 7;

pub const PROG_TRACEPOINT: u32 = 5;
pub const PROG_PERF_EVENT: u32 = 7;

fn sys_bpf(cmd: i32, attr: &mut [u8]) -> io::Result<i64> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            cmd as libc::c_long,
            attr.as_mut_ptr(),
            attr.len() as libc::c_uint,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as i64)
    }
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

pub struct Map {
    fd: OwnedFd,
    pub key_size: usize,
    pub value_size: usize,
}

impl Map {
    pub fn create(kind: u32, key_size: u32, value_size: u32, max: u32, name: &str) -> io::Result<Map> {
        let mut a = [0u8; 72];
        put32(&mut a, 0, kind);
        put32(&mut a, 4, key_size);
        put32(&mut a, 8, value_size);
        put32(&mut a, 12, max);
        let n = name.as_bytes();
        let n = &n[..n.len().min(15)];
        a[28..28 + n.len()].copy_from_slice(n);
        let fd = match sys_bpf(BPF_MAP_CREATE, &mut a) {
            Ok(fd) => fd,
            // Kernels before 4.15 reject map names.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                a[28..44].fill(0);
                sys_bpf(BPF_MAP_CREATE, &mut a)?
            }
            Err(e) => return Err(e),
        };
        Ok(Map {
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            key_size: key_size as usize,
            value_size: value_size as usize,
        })
    }

    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    fn elem(&self, cmd: i32, key: *const u8, val: *mut u8, flags: u64) -> io::Result<()> {
        let mut a = [0u8; 32];
        put32(&mut a, 0, self.fd() as u32);
        put64(&mut a, 8, key as u64);
        put64(&mut a, 16, val as u64);
        put64(&mut a, 24, flags);
        sys_bpf(cmd, &mut a).map(|_| ())
    }

    pub fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        assert_eq!(key.len(), self.key_size);
        let mut v = vec![0u8; self.value_size];
        self.elem(BPF_MAP_LOOKUP_ELEM, key.as_ptr(), v.as_mut_ptr(), 0)
            .ok()
            .map(|_| v)
    }

    pub fn update(&self, key: &[u8], val: &[u8]) -> io::Result<()> {
        assert_eq!(key.len(), self.key_size);
        assert_eq!(val.len(), self.value_size);
        self.elem(BPF_MAP_UPDATE_ELEM, key.as_ptr(), val.as_ptr() as *mut u8, 0)
    }

    pub fn delete(&self, key: &[u8]) -> io::Result<()> {
        assert_eq!(key.len(), self.key_size);
        self.elem(BPF_MAP_DELETE_ELEM, key.as_ptr(), std::ptr::null_mut(), 0)
    }

    /// All keys, read with GET_NEXT_KEY. Entries added meanwhile may be missed.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur: Option<Vec<u8>> = None;
        loop {
            let mut next = vec![0u8; self.key_size];
            let kp = cur.as_ref().map(|k| k.as_ptr()).unwrap_or(std::ptr::null());
            if self
                .elem(BPF_MAP_GET_NEXT_KEY, kp, next.as_mut_ptr(), 0)
                .is_err()
            {
                break;
            }
            out.push(next.clone());
            cur = Some(next);
            if out.len() > 4_000_000 {
                break;
            }
        }
        out
    }

    pub fn entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.keys()
            .into_iter()
            .filter_map(|k| self.lookup(&k).map(|v| (k, v)))
            .collect()
    }
}

// ---- assembler ----------------------------------------------------------

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Insn {
    code: u8,
    regs: u8,
    off: i16,
    imm: i32,
}

pub const R0: u8 = 0;
pub const R1: u8 = 1;
pub const R2: u8 = 2;
pub const R3: u8 = 3;
pub const R4: u8 = 4;
pub const R6: u8 = 6;
pub const R7: u8 = 7;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;

pub const FN_MAP_LOOKUP: i32 = 1;
pub const FN_MAP_UPDATE: i32 = 2;
pub const FN_MAP_DELETE: i32 = 3;
pub const FN_KTIME_GET_NS: i32 = 5;
pub const FN_GET_CURRENT_PID_TGID: i32 = 14;
pub const FN_GET_STACKID: i32 = 27;
pub const FN_PROBE_READ_KERNEL: i32 = 113;
pub const FN_GET_CURRENT_TASK_BTF: i32 = 158;
pub const FN_TASK_PT_REGS: i32 = 175;

pub const F_USER_STACK: i32 = 1 << 8;

/// Size of a load or store.
#[derive(Clone, Copy)]
#[allow(dead_code)] // the whole instruction set, used or not
pub enum Sz {
    W = 0x00,
    H = 0x08,
    B = 0x10,
    Dw = 0x18,
}

/// Jump conditions (BPF_JMP op bits).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Cond {
    Eq = 0x10,
    /// dst & imm != 0
    Set = 0x40,
    Ne = 0x50,
    Sgt = 0x60,
    Slt = 0xc0,
}

fn ins(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn {
        code,
        regs: (src << 4) | (dst & 0x0f),
        off,
        imm,
    }
}

/// A program under construction, with named labels for forward jumps.
#[derive(Default)]
pub struct Asm {
    pub insns: Vec<Insn>,
    labels: std::collections::HashMap<&'static str, usize>,
    fixups: Vec<(usize, &'static str)>,
}

impl Asm {
    pub fn new() -> Asm {
        Asm::default()
    }
    fn push(&mut self, i: Insn) -> &mut Self {
        self.insns.push(i);
        self
    }
    /// dst = imm (64-bit move of a sign-extended 32-bit value)
    pub fn mov_imm(&mut self, dst: u8, imm: i32) -> &mut Self {
        self.push(ins(0xb7, dst, 0, 0, imm))
    }
    pub fn mov(&mut self, dst: u8, src: u8) -> &mut Self {
        self.push(ins(0xbf, dst, src, 0, 0))
    }
    /// dst = (u32) src: zero-extends.
    pub fn mov32(&mut self, dst: u8, src: u8) -> &mut Self {
        self.push(ins(0xbc, dst, src, 0, 0))
    }
    pub fn add_imm(&mut self, dst: u8, imm: i32) -> &mut Self {
        self.push(ins(0x07, dst, 0, 0, imm))
    }
    pub fn sub(&mut self, dst: u8, src: u8) -> &mut Self {
        self.push(ins(0x1f, dst, src, 0, 0))
    }
    pub fn rsh_imm(&mut self, dst: u8, imm: i32) -> &mut Self {
        self.push(ins(0x77, dst, 0, 0, imm))
    }
    /// dst = *(size *)(src + off)
    pub fn ldx(&mut self, sz: Sz, dst: u8, src: u8, off: i16) -> &mut Self {
        self.push(ins(0x61 | sz as u8, dst, src, off, 0))
    }
    /// *(size *)(dst + off) = src
    pub fn stx(&mut self, sz: Sz, dst: u8, off: i16, src: u8) -> &mut Self {
        self.push(ins(0x63 | sz as u8, dst, src, off, 0))
    }
    /// *(size *)(dst + off) = imm
    pub fn st(&mut self, sz: Sz, dst: u8, off: i16, imm: i32) -> &mut Self {
        self.push(ins(0x62 | sz as u8, dst, 0, off, imm))
    }
    /// lock *(u64 *)(dst + off) += src
    pub fn xadd64(&mut self, dst: u8, off: i16, src: u8) -> &mut Self {
        self.push(ins(0xdb, dst, src, off, 0))
    }
    /// dst = the map's file descriptor (a two-slot instruction).
    pub fn ld_map(&mut self, dst: u8, map: &Map) -> &mut Self {
        self.push(ins(0x18, dst, 1, 0, map.fd()));
        self.push(ins(0, 0, 0, 0, 0))
    }
    pub fn call(&mut self, f: i32) -> &mut Self {
        self.push(ins(0x85, 0, 0, 0, f))
    }
    pub fn exit(&mut self) -> &mut Self {
        self.push(ins(0x95, 0, 0, 0, 0))
    }
    pub fn label(&mut self, name: &'static str) -> &mut Self {
        self.labels.insert(name, self.insns.len());
        self
    }
    /// if (dst <cond> imm) goto label
    pub fn jmp_imm(&mut self, c: Cond, dst: u8, imm: i32, to: &'static str) -> &mut Self {
        self.fixups.push((self.insns.len(), to));
        self.push(ins(0x05 | c as u8, dst, 0, 0, imm))
    }
    pub fn ja(&mut self, to: &'static str) -> &mut Self {
        self.fixups.push((self.insns.len(), to));
        self.push(ins(0x05, 0, 0, 0, 0))
    }
    /// r2 = r10 + off: a pointer to the stack, for helper arguments.
    pub fn stack_ptr(&mut self, dst: u8, off: i16) -> &mut Self {
        self.mov(dst, R10).add_imm(dst, off as i32)
    }
    pub fn finish(mut self) -> Vec<Insn> {
        for (at, name) in std::mem::take(&mut self.fixups) {
            let to = *self
                .labels
                .get(name)
                .unwrap_or_else(|| panic!("bpf: no label {name}"));
            self.insns[at].off = (to as i64 - at as i64 - 1) as i16;
        }
        self.insns
    }
}

pub struct Prog {
    fd: OwnedFd,
}

impl Prog {
    pub fn load(kind: u32, insns: &[Insn], name: &str) -> Result<Prog, String> {
        let license = b"Dual MIT/GPL\0";
        let mut log = vec![0u8; 1 << 16];
        let attempt = |log_level: u32, log: &mut [u8]| {
            let mut a = [0u8; 72];
            put32(&mut a, 0, kind);
            put32(&mut a, 4, insns.len() as u32);
            put64(&mut a, 8, insns.as_ptr() as u64);
            put64(&mut a, 16, license.as_ptr() as u64);
            put32(&mut a, 24, log_level);
            put32(&mut a, 28, if log_level > 0 { log.len() as u32 } else { 0 });
            put64(&mut a, 32, if log_level > 0 { log.as_mut_ptr() as u64 } else { 0 });
            let n = name.as_bytes();
            let n = &n[..n.len().min(15)];
            a[48..48 + n.len()].copy_from_slice(n);
            sys_bpf(BPF_PROG_LOAD, &mut a)
        };
        match attempt(0, &mut log) {
            Ok(fd) => Ok(Prog {
                fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            }),
            Err(e) => {
                let _ = attempt(1, &mut log);
                let end = log.iter().position(|&c| c == 0).unwrap_or(log.len());
                let text = String::from_utf8_lossy(&log[..end]);
                let tail: Vec<&str> = text.lines().rev().take(6).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                Err(format!("{e}: {}", tail.join(" | ")))
            }
        }
    }
}

// ---- perf events ----------------------------------------------------------

pub const PERF_TYPE_SOFTWARE: u32 = 1;
pub const PERF_TYPE_TRACEPOINT: u32 = 2;
pub const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;

const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_SET_BPF: libc::c_ulong = 0x4004_2408;
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 8;

pub struct PerfEvent {
    fd: OwnedFd,
}

impl PerfEvent {
    /// A per-CPU event: a tracepoint (`config` is its id) or the CPU clock
    /// sampled at `freq` Hz.
    pub fn open(kind: u32, config: u64, freq: Option<u64>, cpu: i32) -> io::Result<PerfEvent> {
        let mut a = [0u8; 112];
        put32(&mut a, 0, kind);
        put32(&mut a, 4, 112);
        put64(&mut a, 8, config);
        let mut flags: u64 = 1; // disabled until the program is attached
        match freq {
            Some(f) => {
                put64(&mut a, 16, f);
                flags |= 1 << 10; // freq
            }
            None => put64(&mut a, 16, 1), // sample_period
        }
        put64(&mut a, 40, flags);
        put32(&mut a, 48, 1); // wakeup_events
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                a.as_ptr(),
                -1 as libc::pid_t,
                cpu as libc::c_int,
                -1 as libc::c_int,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PerfEvent {
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
        })
    }

    pub fn attach(&self, prog: &Prog) -> io::Result<()> {
        let r = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                PERF_EVENT_IOC_SET_BPF as _,
                prog.fd.as_raw_fd(),
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), PERF_EVENT_IOC_ENABLE as _, 0) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Online CPUs, from /sys/devices/system/cpu/online ("0-3,5").
pub fn online_cpus() -> Vec<i32> {
    let s = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    let v = crate::proc::parse_cpu_list(s.trim());
    if v.is_empty() {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1);
        (0..n as i32).collect()
    } else {
        v.into_iter().map(|c| c as i32).collect()
    }
}

/// Where tracefs is, mounting it if we may (inside a privileged container
/// it often is not mounted).
pub fn tracefs() -> Option<String> {
    for d in ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"] {
        if std::path::Path::new(&format!("{d}/events")).exists() {
            return Some(d.to_string());
        }
    }
    let ok = unsafe {
        libc::mount(
            c"nodev".as_ptr(),
            c"/sys/kernel/tracing".as_ptr(),
            c"tracefs".as_ptr(),
            0,
            std::ptr::null(),
        )
    } == 0;
    ok.then(|| "/sys/kernel/tracing".to_string())
}

/// A tracepoint's id and the offsets of the named fields in its record.
pub fn tracepoint(group: &str, name: &str, fields: &[&str]) -> Option<(u64, Vec<i16>)> {
    let base = format!("{}/events/{group}/{name}", tracefs()?);
    let id = std::fs::read_to_string(format!("{base}/id")).ok()?.trim().parse().ok()?;
    let fmt = std::fs::read_to_string(format!("{base}/format")).ok()?;
    let mut offs = Vec::new();
    for f in fields {
        offs.push(field_offset(&fmt, f)?);
    }
    Some((id, offs))
}

pub fn field_offset(fmt: &str, field: &str) -> Option<i16> {
    for l in fmt.lines() {
        let l = l.trim();
        let Some(decl) = l.strip_prefix("field:") else {
            continue;
        };
        let decl = decl.split(';').next()?;
        let ident = decl
            .split_whitespace()
            .last()?
            .split('[')
            .next()?;
        if ident == field {
            let off = l.split("offset:").nth(1)?.split(';').next()?;
            return off.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracepoint_format_offsets() {
        let fmt = "\tfield:char prev_comm[16];\toffset:8;\tsize:16;\tsigned:0;\n\
                   \tfield:pid_t prev_pid;\toffset:24;\tsize:4;\tsigned:1;\n\
                   \tfield:long prev_state;\toffset:32;\tsize:8;\tsigned:1;\n\
                   \tfield:pid_t next_pid;\toffset:56;\tsize:4;\tsigned:1;\n";
        assert_eq!(field_offset(fmt, "prev_state"), Some(32));
        assert_eq!(field_offset(fmt, "next_pid"), Some(56));
        assert_eq!(field_offset(fmt, "prev_comm"), Some(8));
        assert_eq!(field_offset(fmt, "nope"), None);
    }

    #[test]
    fn jumps_are_relative() {
        let mut a = Asm::new();
        a.jmp_imm(Cond::Eq, R0, 0, "out")
            .mov_imm(R0, 1)
            .label("out")
            .mov_imm(R0, 0)
            .exit();
        let v = a.finish();
        assert_eq!(v[0].off, 1);
        assert_eq!(v[0].code, 0x15);
    }
}
