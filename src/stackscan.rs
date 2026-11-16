//! The user stack of a thread that is blocked, read without stopping it and
//! without root: /proc/<pid>/task/<tid>/syscall gives its stack pointer and
//! program counter, /proc/<pid>/mem its stack. The frame-pointer chain is
//! found in that memory and followed; every return address must follow a
//! call instruction, which rules out most stale values.
//!
//! The thread is blocked in the kernel, so its stack does not change while
//! it is read. Needs the same access as reading its system call (your own
//! processes, or root; with ptrace_scope=1 only your descendants).

use crate::symbolize::Mapping;
use std::fs::File;
use std::os::unix::fs::FileExt;

/// How much stack to read: frames with large local buffers push their
/// frame records far from the stack pointer.
const SCAN: usize = 256 * 1024;

/// sp and pc from /proc/<pid>/task/<tid>/syscall ("nr a1..a6 sp pc", or
/// "-1 sp pc" outside a system call, or "running").
pub fn sp_pc(s: &str) -> Option<(u64, u64)> {
    let f: Vec<&str> = s.split_whitespace().collect();
    if f.len() < 3 || f[0] == "running" {
        return None;
    }
    let hex = |x: &str| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok();
    Some((hex(f[f.len() - 2])?, hex(f[f.len() - 1])?))
}

pub struct Scanner<'a> {
    mem: &'a File,
    maps: &'a [Mapping],
    code_cache: std::collections::HashMap<u64, bool>,
}

impl<'a> Scanner<'a> {
    pub fn new(mem: &'a File, maps: &'a [Mapping]) -> Scanner<'a> {
        Scanner {
            mem,
            maps,
            code_cache: std::collections::HashMap::new(),
        }
    }

    fn read(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let mut b = vec![0u8; len];
        let n = self.mem.read_at(&mut b, addr).ok()?;
        b.truncate(n);
        (n > 0).then_some(b)
    }

    fn exec(&self, a: u64) -> bool {
        self.maps.iter().any(|m| m.exec && m.start <= a && a < m.end)
    }

    /// `a` is a plausible return address: in code, right after a call.
    fn is_ret(&mut self, a: u64) -> bool {
        if a < 0x1000 || !self.exec(a) {
            return false;
        }
        if let Some(&v) = self.code_cache.get(&a) {
            return v;
        }
        let v = self.read(a.saturating_sub(8), 8).map(|b| after_call(&b)).unwrap_or(false);
        self.code_cache.insert(a, v);
        v
    }

    /// Return addresses, innermost first, starting with `pc`.
    pub fn stack(&mut self, sp: u64, pc: u64) -> Vec<u64> {
        let mut out = vec![pc];
        let top = self
            .maps
            .iter()
            .find(|m| m.start <= sp && sp < m.end)
            .map(|m| m.end)
            .unwrap_or(sp + SCAN as u64);
        let len = ((top - sp) as usize).min(SCAN);
        let Some(buf) = self.read(sp, len) else {
            return out;
        };
        let words: Vec<u64> = buf
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_ne_bytes(*c))
            .collect();
        let n = words.len();
        let at = |a: u64| -> Option<usize> {
            (a >= sp && a.is_multiple_of(8) && ((a - sp) / 8) as usize + 1 < n).then(|| ((a - sp) / 8) as usize)
        };
        // Follow the chain from a candidate frame record at word i.
        let chain_from = |s: &mut Self, i: usize| -> Vec<u64> {
            let mut v = Vec::new();
            let mut i = i;
            loop {
                let ret = words[i + 1];
                if !s.is_ret(ret) {
                    break;
                }
                v.push(ret);
                let next = words[i];
                match at(next) {
                    Some(j) if j > i => i = j,
                    _ => break,
                }
                if v.len() >= 128 {
                    break;
                }
            }
            v
        };
        let mut best: (usize, Vec<u64>) = (0, Vec::new());
        for i in 0..n.saturating_sub(1) {
            let fp = words[i];
            if at(fp).map(|j| j <= i).unwrap_or(true) {
                continue;
            }
            let c = chain_from(self, i);
            if c.len() >= 3 || (c.len() >= 2 && words[at(fp).unwrap()] == 0) {
                best = (i, c);
                break;
            }
            if c.len() > best.1.len() {
                best = (i, c);
            }
        }
        // Return addresses stored below the first frame record belong to
        // frames that keep no record (a leaf system call wrapper on x86-64
        // leaves its caller's address at the stack pointer).
        let first = best.0;
        for w in words.iter().take(first.min(8)) {
            if self.is_ret(*w) {
                out.push(*w);
            }
        }
        out.extend(best.1);
        out
    }
}

/// The eight bytes before a return address end with a call instruction.
fn after_call(b: &[u8]) -> bool {
    if b.len() < 8 {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let insn = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        // BL, BLR, BLRAA/BLRAB(Z)
        insn & 0xfc00_0000 == 0x9400_0000
            || insn & 0xffff_fc1f == 0xd63f_0000
            || insn & 0xfeff_f800 == 0xd63f_0800
    }
    #[cfg(target_arch = "x86_64")]
    {
        // call rel32
        if b[3] == 0xe8 {
            return true;
        }
        // call r/m: ff /2, two to seven bytes long
        (1..=6).any(|k| b[8 - k - 1] == 0xff && (b[8 - k] >> 3) & 7 == 2)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = b;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syscall_line() {
        assert_eq!(
            sp_pc("0 0x3 0x7ffd 0x10 0x0 0x0 0x0 0x7ffc1000 0x7f001234"),
            Some((0x7ffc1000, 0x7f001234))
        );
        assert_eq!(sp_pc("-1 0x7ffc1000 0x7f001234"), Some((0x7ffc1000, 0x7f001234)));
        assert_eq!(sp_pc("running"), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn call_forms() {
        assert!(after_call(&[0, 0, 0, 0xe8, 1, 2, 3, 4]));
        assert!(after_call(&[0, 0, 0, 0, 0, 0, 0xff, 0xd0]));
        // call *disp32(%rip)
        assert!(after_call(&[0, 0, 0xff, 0x15, 1, 2, 3, 4]));
        assert!(!after_call(&[0x90; 8]));
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn call_forms() {
        let bl = 0x9400_0010u32.to_le_bytes();
        assert!(after_call(&[0, 0, 0, 0, bl[0], bl[1], bl[2], bl[3]]));
        let blr = 0xd63f_0020u32.to_le_bytes();
        assert!(after_call(&[0, 0, 0, 0, blr[0], blr[1], blr[2], blr[3]]));
        let nop = 0xd503_201fu32.to_le_bytes();
        assert!(!after_call(&[0, 0, 0, 0, nop[0], nop[1], nop[2], nop[3]]));
    }
}
