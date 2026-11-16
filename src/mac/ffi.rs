//! Thin readers over libproc and sysctl. Struct layouts that the libc crate
//! does not carry are read at fixed offsets: they are part of the stable
//! macOS ABI and identical on arm64 and x86-64 (see sys/proc_info.h,
//! sys/resource.h). Every reader returns `None` on failure: processes come
//! and go, and other users' processes are mostly closed to us.

use std::ffi::c_void;

const PROC_PIDLISTFDS: i32 = 1;
const PROC_PIDTASKINFO: i32 = 4;
const PROC_PIDTHREADID64INFO: i32 = 15;
const PROC_PIDT_SHORTBSDINFO: i32 = 13;
/// Not in the SDK header, but in XNU since 10.11: unique thread ids, the
/// same ids `sample` prints as Thread_<id>.
const PROC_PIDLISTTHREADIDS: i32 = 28;

const PROC_PIDFDVNODEPATHINFO: i32 = 2;
const PROC_PIDFDSOCKETINFO: i32 = 3;
const PROC_PIDFDPIPEINFO: i32 = 6;

pub const FD_VNODE: u32 = 1;
pub const FD_SOCKET: u32 = 2;
pub const FD_KQUEUE: u32 = 5;
pub const FD_PIPE: u32 = 6;

const RUSAGE_INFO_V6: i32 = 6;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_ne_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}
fn i32_at(b: &[u8], o: usize) -> i32 {
    i32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_ne_bytes(b[o..o + 8].try_into().unwrap())
}
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn pidinfo(pid: i32, flavor: i32, arg: u64, size: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; size];
    let n = unsafe {
        libc::proc_pidinfo(pid, flavor, arg, b.as_mut_ptr() as *mut c_void, size as i32)
    };
    if n <= 0 {
        return None;
    }
    b.truncate(n as usize);
    Some(b)
}

pub fn pids() -> Vec<i32> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v = vec![0i32; n as usize + 64];
    let n = unsafe {
        libc::proc_listallpids(v.as_mut_ptr() as *mut c_void, (v.len() * 4) as i32)
    };
    v.truncate(n.max(0) as usize);
    v.retain(|&p| p > 0);
    v
}

/// Live children of `ppid` (proc_listchildpids returns a count of pids).
pub fn children(ppid: i32) -> Vec<i32> {
    let mut v = vec![0i32; 256];
    loop {
        let n = unsafe {
            libc::proc_listchildpids(ppid, v.as_mut_ptr() as *mut c_void, (v.len() * 4) as i32)
        };
        if n <= 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < v.len() || v.len() >= 65536 {
            v.truncate(n.min(v.len()));
            v.retain(|&p| p > 0);
            return v;
        }
        v.resize(v.len() * 4, 0);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Short {
    pub ppid: i32,
    /// SIDL 1, SRUN 2, SSLEEP 3, SSTOP 4, SZOMB 5
    pub status: u32,
    pub comm: String,
    pub uid: u32,
}

/// Works for every process, whoever owns it.
pub fn short(pid: i32) -> Option<Short> {
    let b = pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 0, 64)?;
    if b.len() < 64 {
        return None;
    }
    Some(Short {
        ppid: u32_at(&b, 4) as i32,
        status: u32_at(&b, 12),
        comm: cstr(&b[16..32]),
        uid: u32_at(&b, 36),
    })
}

/// When the process started (seconds, microseconds); own processes.
pub fn start_time(pid: i32) -> Option<(u64, u64)> {
    let b = pidinfo(pid, 3, 0, 136)?;
    (b.len() >= 136).then(|| (u64_at(&b, 120), u64_at(&b, 128)))
}

pub fn path(pid: i32) -> Option<String> {
    let mut b = vec![0u8; 4096];
    let n = unsafe { libc::proc_pidpath(pid, b.as_mut_ptr() as *mut c_void, b.len() as u32) };
    (n > 0).then(|| cstr(&b[..n as usize]))
}

/// The command line (own processes, or all as root).
pub fn cmdline(pid: i32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let r = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    };
    if r != 0 || size < 8 {
        return None;
    }
    let mut b = vec![0u8; size];
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            b.as_mut_ptr() as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    b.truncate(size);
    parse_procargs(&b)
}

pub fn parse_procargs(b: &[u8]) -> Option<String> {
    if b.len() < 4 {
        return None;
    }
    let argc = i32_at(b, 0).max(0) as usize;
    let mut i = 4;
    // the executable path, then NUL padding
    while i < b.len() && b[i] != 0 {
        i += 1;
    }
    while i < b.len() && b[i] == 0 {
        i += 1;
    }
    let mut args = Vec::new();
    while args.len() < argc && i < b.len() {
        let s = i;
        while i < b.len() && b[i] != 0 {
            i += 1;
        }
        args.push(String::from_utf8_lossy(&b[s..i]).into_owned());
        i += 1;
    }
    (!args.is_empty()).then(|| args.join(" "))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Task {
    pub user_ns: u64,
    pub system_ns: u64,
}

pub fn task(pid: i32) -> Option<Task> {
    let t: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let b = pidinfo(pid, PROC_PIDTASKINFO, 0, std::mem::size_of_val(&t))?;
    if b.len() < 96 {
        return None;
    }
    // In Mach time units, like rusage (nanoseconds only on Intel).
    Some(Task {
        user_ns: mach_ns(u64_at(&b, 16)),
        system_ns: mach_ns(u64_at(&b, 24)),
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub user_ns: u64,
    pub system_ns: u64,
    pub disk_read: u64,
    pub disk_written: u64,
    /// Time the process's threads were runnable but queued for a CPU.
    pub runnable_ns: u64,
    /// Physical footprint in bytes (what Activity Monitor calls Memory).
    pub footprint: u64,
}

/// Mach absolute time units per nanosecond differ on Apple silicon: the
/// rusage times are in Mach units there.
fn mach_ns(t: u64) -> u64 {
    static TB: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let &(numer, denom) = TB.get_or_init(|| {
        let mut tb = Timebase { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer, tb.denom)
    });
    if denom == 0 {
        return t;
    }
    (t as u128 * numer as u128 / denom as u128) as u64
}

// Declared here: the libc crate marks its Mach bindings deprecated.
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn mach_host_self() -> u32;
    fn mach_task_self() -> u32;
}

pub fn usage(pid: i32) -> Option<Usage> {
    let mut b = vec![0u8; 464];
    let r = unsafe { libc::proc_pid_rusage(pid, RUSAGE_INFO_V6, b.as_mut_ptr() as *mut _) };
    if r != 0 {
        return None;
    }
    let user_ns = mach_ns(u64_at(&b, 16));
    let system_ns = mach_ns(u64_at(&b, 24));
    // ri_runnable_time counts time running too: what is left is the time
    // runnable but queued.
    let runnable = mach_ns(u64_at(&b, 288));
    Some(Usage {
        user_ns,
        system_ns,
        disk_read: u64_at(&b, 144),
        disk_written: u64_at(&b, 152),
        runnable_ns: runnable.saturating_sub(user_ns + system_ns),
        footprint: u64_at(&b, 72),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Thread {
    pub user_ns: u64,
    pub system_ns: u64,
    /// 1 running (or runnable), 2 stopped, 3 waiting, 4 uninterruptible, 5 halted
    pub state: i32,
    pub name: String,
    pub curpri: i32,
}

pub fn thread_ids(pid: i32) -> Vec<u64> {
    let Some(b) = pidinfo(pid, PROC_PIDLISTTHREADIDS, 0, 8 * 4096) else {
        return Vec::new();
    };
    b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes(*c))
        .filter(|&t| t != 0)
        .collect()
}

pub fn thread(pid: i32, id: u64) -> Option<Thread> {
    let b = pidinfo(pid, PROC_PIDTHREADID64INFO, id, 112)?;
    if b.len() < 112 {
        return None;
    }
    Some(Thread {
        user_ns: u64_at(&b, 0),
        system_ns: u64_at(&b, 8),
        state: i32_at(&b, 24),
        curpri: i32_at(&b, 36),
        name: cstr(&b[48..112]),
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Fd {
    pub fd: i32,
    pub kind: u32,
}

pub fn fds(pid: i32) -> Option<Vec<Fd>> {
    let b = pidinfo(pid, PROC_PIDLISTFDS, 0, 8 * 8192)?;
    Some(
        b.as_chunks::<8>()
            .0
            .iter()
            .map(|c| Fd {
                fd: i32_at(c, 0),
                kind: u32_at(c, 4),
            })
            .collect(),
    )
}

fn fdinfo(pid: i32, fd: i32, flavor: i32, size: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; size];
    let n = unsafe {
        libc::proc_pidfdinfo(pid, fd, flavor, b.as_mut_ptr() as *mut c_void, size as i32)
    };
    (n as usize >= size).then_some(b)
}

/// FREAD / FWRITE from the file's open flags.
pub const FREAD: u32 = 1;
pub const FWRITE: u32 = 2;

#[derive(Clone, Debug)]
pub struct Vnode {
    pub path: String,
    pub flags: u32,
    pub mode: u16,
    pub dev: u32,
    pub ino: u64,
}

pub fn vnode(pid: i32, fd: i32) -> Option<Vnode> {
    let b = fdinfo(pid, fd, PROC_PIDFDVNODEPATHINFO, 1200)?;
    Some(Vnode {
        flags: u32_at(&b, 0),
        dev: u32_at(&b, 24),
        mode: u16_at(&b, 28),
        ino: u64_at(&b, 32),
        path: cstr(&b[176..176 + 1024]),
    })
}

#[derive(Clone, Debug)]
pub struct Pipe {
    pub flags: u32,
    pub handle: u64,
    pub peer: u64,
    /// Bytes waiting in the pipe.
    pub queued: u64,
}

pub fn pipe(pid: i32, fd: i32) -> Option<Pipe> {
    let b = fdinfo(pid, fd, PROC_PIDFDPIPEINFO, 184)?;
    Some(Pipe {
        flags: u32_at(&b, 0),
        queued: u64_at(&b, 112),
        handle: u64_at(&b, 160),
        peer: u64_at(&b, 168),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum SockAddr {
    In { ip: std::net::IpAddr, port: u16 },
    Unix(String),
    None,
}

impl std::fmt::Display for SockAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SockAddr::In {
                ip: std::net::IpAddr::V6(v6),
                port,
            } => write!(f, "[{v6}]:{port}"),
            SockAddr::In { ip, port } => write!(f, "{ip}:{port}"),
            SockAddr::Unix(p) => write!(f, "{p}"),
            SockAddr::None => write!(f, "?"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Socket {
    pub flags: u32,
    /// SOCKINFO_IN 1, SOCKINFO_TCP 2, SOCKINFO_UN 3
    pub kind: i32,
    pub pcb: u64,
    pub local: SockAddr,
    pub remote: SockAddr,
    /// TCP state (TSI_S_*: 1 listen, 4 established...), -1 if not TCP.
    pub tcp_state: i32,
    /// Bytes in the receive and send buffers.
    pub rcv: u32,
    pub snd: u32,
    /// For a Unix socket, the pcb of the socket at the other end.
    pub unix_peer_pcb: u64,
    pub so_options: u32,
}

impl Socket {
    pub fn listening(&self) -> bool {
        // TSI_S_LISTEN, or SO_ACCEPTCONN
        self.tcp_state == 1 || self.so_options & 0x2 != 0
    }
}

pub fn socket(pid: i32, fd: i32) -> Option<Socket> {
    let b = fdinfo(pid, fd, PROC_PIDFDSOCKETINFO, 792)?;
    let kind = i32_at(&b, 256);
    let mut s = Socket {
        flags: u32_at(&b, 0),
        kind,
        pcb: u64_at(&b, 168),
        local: SockAddr::None,
        remote: SockAddr::None,
        tcp_state: -1,
        rcv: u32_at(&b, 208),
        snd: u32_at(&b, 232),
        unix_peer_pcb: 0,
        so_options: u32_at(&b, 188),
    };
    match kind {
        1 | 2 => {
            // in_sockinfo at 264: fport, lport, ..., vflag at +24, faddr at +32, laddr at +48
            let base = 264;
            let port = |o: usize| u16::from_be((u32_at(&b, o) & 0xffff) as u16);
            let vflag = b[base + 24];
            let addr = |o: usize| -> std::net::IpAddr {
                if vflag & 0x2 != 0 && vflag & 0x1 == 0 {
                    let mut a = [0u8; 16];
                    a.copy_from_slice(&b[o..o + 16]);
                    std::net::IpAddr::V6(std::net::Ipv6Addr::from(a))
                } else {
                    let a = [b[o + 12], b[o + 13], b[o + 14], b[o + 15]];
                    std::net::IpAddr::V4(std::net::Ipv4Addr::from(a))
                }
            };
            s.remote = SockAddr::In {
                ip: addr(base + 32),
                port: port(base),
            };
            s.local = SockAddr::In {
                ip: addr(base + 48),
                port: port(base + 4),
            };
            if kind == 2 {
                s.tcp_state = i32_at(&b, 344);
            }
        }
        3 => {
            // un_sockinfo at 264: conn_so, conn_pcb, addr (sockaddr_un, 255), caddr
            s.unix_peer_pcb = u64_at(&b, 272);
            let path = cstr(&b[280 + 2..280 + 255]);
            let cpath = cstr(&b[535 + 2..535 + 255]);
            s.local = SockAddr::Unix(path);
            s.remote = SockAddr::Unix(cpath);
        }
        _ => {}
    }
    Some(s)
}

/// System-wide counters from the VM: swap-ins and page-ins so far.
pub fn vm_swapins_pageins() -> Option<(u64, u64)> {
    let mut st: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let r = unsafe {
        libc::host_statistics64(
            mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut st as *mut _ as *mut i32,
            &mut count,
        )
    };
    (r == 0).then_some((st.swapins as u64, st.pageins as u64))
}

/// kern.memorystatus_vm_pressure_level: 1 normal, 2 warning, 4 critical.
pub fn memory_pressure() -> Option<i32> {
    let mut v: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    let r = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            &mut v as *mut i32 as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0).then_some(v)
}

pub fn ncpu() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total CPU ticks (user+system+nice, and idle) over all CPUs so far.
pub fn cpu_ticks() -> Option<(u64, u64)> {
    let mut n: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut cnt: libc::mach_msg_type_number_t = 0;
    let r = unsafe {
        libc::host_processor_info(
            mach_host_self(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut n,
            &mut info,
            &mut cnt,
        )
    };
    if r != 0 || info.is_null() {
        return None;
    }
    let s = unsafe { std::slice::from_raw_parts(info, cnt as usize) };
    let (mut busy, mut idle) = (0u64, 0u64);
    for c in s.as_chunks::<4>().0 {
        // user, system, idle, nice
        busy += c[0] as u64 + c[1] as u64 + c[3] as u64;
        idle += c[2] as u64;
    }
    unsafe {
        libc::vm_deallocate(
            mach_task_self(),
            info as libc::vm_address_t,
            cnt as libc::vm_size_t * 4,
        );
    }
    Some((busy, idle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs() {
        let mut b = 3i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/bin/zsh\0\0\0zsh\0-c\0echo hi\0PATH=/bin\0");
        assert_eq!(parse_procargs(&b).as_deref(), Some("zsh -c echo hi"));
    }

    #[test]
    fn reads_own_process() {
        let me = std::process::id() as i32;
        let s = short(me).expect("short info");
        assert_ne!(s.comm, "");
        assert!(task(me).is_some());
        let ids = thread_ids(me);
        assert!(!ids.is_empty());
        assert!(thread(me, ids[0]).is_some());
        assert!(usage(me).is_some());
        assert!(fds(me).map(|v| !v.is_empty()).unwrap_or(false));
    }
}
