//! A process's descriptors, and who holds the other end of a pipe or socket.
//! Only processes the user may inspect are searched (their own, or all as
//! root).

use super::ffi::{self, Pipe, SockAddr, Socket, Vnode, FD_KQUEUE, FD_PIPE, FD_SOCKET, FD_VNODE};

#[derive(Clone, Debug)]
pub enum Kind {
    Pipe(Pipe),
    Socket(Socket),
    File(Vnode),
    Fifo(Vnode),
    Tty(Vnode),
    Dev(Vnode),
    Kqueue,
    Other,
}

#[derive(Clone, Debug)]
pub struct Fd {
    pub fd: i32,
    pub kind: Kind,
}

const S_IFMT: u16 = 0o170000;
const S_IFIFO: u16 = 0o010000;
const S_IFCHR: u16 = 0o020000;
const S_IFREG: u16 = 0o100000;

pub fn list(pid: i32) -> Option<Vec<Fd>> {
    let raw = ffi::fds(pid)?;
    Some(
        raw.into_iter()
            .take(4096)
            .map(|f| Fd {
                fd: f.fd,
                kind: match f.kind {
                    FD_PIPE => ffi::pipe(pid, f.fd).map(Kind::Pipe).unwrap_or(Kind::Other),
                    FD_SOCKET => ffi::socket(pid, f.fd)
                        .map(Kind::Socket)
                        .unwrap_or(Kind::Other),
                    FD_VNODE => match ffi::vnode(pid, f.fd) {
                        Some(v) => match v.mode & S_IFMT {
                            S_IFIFO => Kind::Fifo(v),
                            S_IFCHR if v.path.starts_with("/dev/tty") || v.path == "/dev/console" => {
                                Kind::Tty(v)
                            }
                            S_IFCHR => Kind::Dev(v),
                            S_IFREG => Kind::File(v),
                            _ => Kind::Dev(v),
                        },
                        None => Kind::Other,
                    },
                    FD_KQUEUE => Kind::Kqueue,
                    _ => Kind::Other,
                },
            })
            .collect(),
    )
}

fn flags(k: &Kind) -> u32 {
    match k {
        Kind::Pipe(p) => p.flags,
        Kind::Socket(s) => s.flags,
        Kind::File(v) | Kind::Fifo(v) | Kind::Tty(v) | Kind::Dev(v) => v.flags,
        _ => 0,
    }
}

pub fn readable(f: &Fd) -> bool {
    flags(&f.kind) & ffi::FREAD != 0
}
pub fn writable(f: &Fd) -> bool {
    flags(&f.kind) & ffi::FWRITE != 0
}

pub fn port_name(port: u16) -> Option<&'static str> {
    Some(match port {
        22 => "ssh",
        53 => "dns",
        80 => "http",
        443 => "https",
        3306 => "mysql",
        5432 => "postgres",
        6379 => "redis",
        9092 => "kafka",
        11211 => "memcached",
        27017 => "mongodb",
        _ => return None,
    })
}

/// A connected stream (or datagram socket with a peer address).
pub fn connected(s: &Socket) -> bool {
    match s.kind {
        // TSI_S_ESTABLISHED and later states, but not LISTEN/CLOSED
        2 => s.tcp_state >= 2 && !s.listening(),
        3 => s.unix_peer_pcb != 0 && !s.listening(),
        1 => matches!(s.remote, SockAddr::In { port, .. } if port != 0),
        _ => false,
    }
}

pub fn sock_desc(s: &Socket) -> String {
    match s.kind {
        2 if s.listening() => match &s.local {
            SockAddr::In { port, .. } => format!("TCP listener on port {port}"),
            _ => "a TCP listener".into(),
        },
        2 => {
            let svc = match &s.remote {
                SockAddr::In { port, .. } => port_name(*port).map(|n| format!(" ({n})")).unwrap_or_default(),
                _ => String::new(),
            };
            format!("TCP {} → {}{svc}", s.local, s.remote)
        }
        1 => match &s.remote {
            SockAddr::In { port: 0, .. } => match &s.local {
                SockAddr::In { port, .. } => format!("UDP port {port}"),
                _ => "a UDP socket".into(),
            },
            r => format!("UDP {} → {r}", s.local),
        },
        3 => match (&s.local, &s.remote) {
            (SockAddr::Unix(p), _) if !p.is_empty() => format!("Unix socket {p}"),
            (_, SockAddr::Unix(p)) if !p.is_empty() => format!("Unix socket to {p}"),
            _ => "a Unix socket".into(),
        },
        _ => "a socket".into(),
    }
}

pub fn desc(f: &Fd) -> String {
    match &f.kind {
        Kind::Pipe(_) => "a pipe".into(),
        Kind::Fifo(v) => format!("FIFO {}", v.path),
        Kind::Tty(v) => format!("terminal {}", v.path),
        Kind::Socket(s) => sock_desc(s),
        Kind::File(v) | Kind::Dev(v) => v.path.clone(),
        Kind::Kqueue => "a kqueue".into(),
        Kind::Other => "a descriptor".into(),
    }
}

/// Processes the user can inspect, other than `exclude`.
pub fn visible_pids(exclude: i32) -> Vec<i32> {
    let me = unsafe { libc::getuid() };
    ffi::pids()
        .into_iter()
        .filter(|&p| p != exclude && p != std::process::id() as i32)
        .filter(|&p| me == 0 || ffi::short(p).map(|s| s.uid == me).unwrap_or(false))
        .collect()
}

/// Processes holding the pipe end `handle` (open for writing if `write`).
pub fn pipe_holders(handle: u64, exclude: i32, write: Option<bool>) -> Vec<i32> {
    let mut out = Vec::new();
    for p in visible_pids(exclude) {
        let Some(v) = ffi::fds(p) else { continue };
        for f in v.iter().filter(|f| f.kind == FD_PIPE) {
            if let Some(pi) = ffi::pipe(p, f.fd) {
                let ok = match write {
                    Some(true) => pi.flags & ffi::FWRITE != 0,
                    Some(false) => pi.flags & ffi::FREAD != 0,
                    None => true,
                };
                if pi.handle == handle && ok {
                    out.push(p);
                    break;
                }
            }
        }
    }
    out
}

/// Processes that have the same FIFO or file open (by device and inode).
pub fn file_holders(dev: u32, ino: u64, exclude: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for p in visible_pids(exclude) {
        let Some(v) = ffi::fds(p) else { continue };
        if v.iter()
            .filter(|f| f.kind == FD_VNODE)
            .any(|f| ffi::vnode(p, f.fd).map(|n| n.dev == dev && n.ino == ino).unwrap_or(false))
        {
            out.push(p);
        }
    }
    out
}

/// The process at the other end of a connected socket: for TCP, the one
/// whose socket is our mirror image, else the one listening on the port;
/// for Unix, the one owning the peer socket.
pub fn socket_peer(s: &Socket, exclude: i32) -> Vec<i32> {
    let mut mirror = Vec::new();
    let mut listener = Vec::new();
    for p in visible_pids(exclude) {
        let Some(v) = ffi::fds(p) else { continue };
        for f in v.iter().filter(|f| f.kind == FD_SOCKET) {
            let Some(o) = ffi::socket(p, f.fd) else { continue };
            match s.kind {
                3 if o.kind == 3 && o.pcb != 0 && o.pcb == s.unix_peer_pcb => {
                    mirror.push(p);
                }
                2 if o.kind == 2 => {
                    if o.local == s.remote && o.remote == s.local {
                        mirror.push(p);
                    } else if o.listening() {
                        if let (SockAddr::In { port: lp, .. }, SockAddr::In { port: rp, .. }) = (&o.local, &s.remote) {
                            if lp == rp {
                                listener.push(p);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    mirror.sort_unstable();
    mirror.dedup();
    if mirror.is_empty() {
        listener.sort_unstable();
        listener.dedup();
        listener
    } else {
        mirror
    }
}

/// Is this address one of ours (loopback, or a local interface)?
pub fn is_local(a: &SockAddr) -> bool {
    match a {
        SockAddr::In { ip, .. } => ip.is_loopback() || local_ips().contains(ip),
        _ => true,
    }
}

fn local_ips() -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return out;
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let s = &*(a as *const libc::sockaddr_in);
                        out.push(std::net::IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(
                            s.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let s = &*(a as *const libc::sockaddr_in6);
                        out.push(std::net::IpAddr::V6(std::net::Ipv6Addr::from(s.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out
}
