//! Socket identification: /proc/<pid>/net tables for addresses, and the
//! sock_diag netlink interface (which needs no privileges) for TCP timing
//! and for the far end of Unix sockets.

use crate::proc;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addr {
    pub raw: [u8; 16],
    pub v6: bool,
    pub port: u16,
}

impl Addr {
    pub fn ip(&self) -> IpAddr {
        if self.v6 {
            let a = Ipv6Addr::from(self.raw);
            match a.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(a),
            }
        } else {
            IpAddr::V4(Ipv4Addr::new(
                self.raw[0],
                self.raw[1],
                self.raw[2],
                self.raw[3],
            ))
        }
    }

    pub fn is_unset(&self) -> bool {
        self.port == 0 && self.raw.iter().all(|&b| b == 0)
    }
}

impl std::fmt::Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip() {
            IpAddr::V4(a) => write!(f, "{a}:{}", self.port),
            IpAddr::V6(a) => write!(f, "[{a}]:{}", self.port),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sock {
    Tcp {
        local: Addr,
        remote: Addr,
        listening: bool,
        connecting: bool,
    },
    Udp {
        local: Addr,
        remote: Addr,
    },
    Unix {
        path: String,
        stream: bool,
        listening: bool,
        connected: bool,
    },
}

/// "0100007F:1F90" or the 32-hex-digit IPv6 form. Each 8-digit group is a
/// native-endian u32 holding four network-order bytes.
pub fn parse_addr(s: &str) -> Option<Addr> {
    let (a, p) = s.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    let mut raw = [0u8; 16];
    if a.len() != 8 && a.len() != 32 {
        return None;
    }
    for (i, chunk) in a.as_bytes().chunks(8).enumerate() {
        let w = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        raw[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
    }
    Some(Addr {
        raw,
        v6: a.len() == 32,
        port,
    })
}

fn parse_inet(table: &str, tcp: bool, out: &mut HashMap<u64, Sock>) {
    for l in table.lines().skip(1) {
        let f: Vec<&str> = l.split_ascii_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let (Some(local), Some(remote), Ok(st), Ok(ino)) = (
            parse_addr(f[1]),
            parse_addr(f[2]),
            u8::from_str_radix(f[3], 16),
            f[9].parse::<u64>(),
        ) else {
            continue;
        };
        if ino == 0 {
            continue;
        }
        out.insert(
            ino,
            if tcp {
                Sock::Tcp {
                    local,
                    remote,
                    listening: st == 0x0a,
                    connecting: st == 0x02,
                }
            } else {
                Sock::Udp { local, remote }
            },
        );
    }
}

fn parse_unix(table: &str, out: &mut HashMap<u64, Sock>) {
    for l in table.lines().skip(1) {
        let f: Vec<&str> = l.split_ascii_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        let (Ok(flags), Ok(ty), Ok(st), Ok(ino)) = (
            u32::from_str_radix(f[3], 16),
            u32::from_str_radix(f[4], 16),
            u32::from_str_radix(f[5], 16),
            f[6].parse::<u64>(),
        ) else {
            continue;
        };
        out.insert(
            ino,
            Sock::Unix {
                path: f.get(7).map(|s| s.to_string()).unwrap_or_default(),
                stream: ty != 2,
                listening: flags & 0x10000 != 0,
                connected: st == 3,
            },
        );
    }
}

/// All sockets in the network namespace of `pid`, by inode.
pub fn sockets(pid: i32) -> HashMap<u64, Sock> {
    let mut m = HashMap::new();
    let rd = |f: &str| proc::slurp(&format!("/proc/{pid}/net/{f}")).unwrap_or_default();
    parse_inet(&rd("tcp"), true, &mut m);
    parse_inet(&rd("tcp6"), true, &mut m);
    parse_inet(&rd("udp"), false, &mut m);
    parse_inet(&rd("udp6"), false, &mut m);
    parse_unix(&rd("unix"), &mut m);
    m
}

pub fn same_netns(pid: i32) -> bool {
    let a = std::fs::read_link(format!("/proc/{pid}/ns/net"));
    let b = std::fs::read_link("/proc/self/ns/net");
    matches!((a, b), (Ok(a), Ok(b)) if a == b)
}

pub fn port_name(port: u16) -> Option<&'static str> {
    Some(match port {
        22 => "ssh",
        53 => "dns",
        80 => "http",
        443 => "https",
        2049 => "nfs",
        3306 => "mysql",
        5432 => "postgres",
        5672 => "amqp",
        6379 => "redis",
        9092 => "kafka",
        9200 => "elasticsearch",
        11211 => "memcached",
        27017 => "mongodb",
        _ => return None,
    })
}

// ---- sock_diag -----------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct TcpInfo {
    pub retransmits: u8,
    /// Milliseconds since this side last sent / received payload.
    pub last_sent_ms: u32,
    pub last_recv_ms: u32,
}

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

fn diag(req_body: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        );
        if fd < 0 {
            return None;
        }
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 200_000,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        let mut msg = Vec::with_capacity(16 + req_body.len());
        msg.extend_from_slice(&((16 + req_body.len()) as u32).to_ne_bytes());
        msg.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        msg.extend_from_slice(&(libc::NLM_F_REQUEST as u16).to_ne_bytes());
        msg.extend_from_slice(&1u32.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        msg.extend_from_slice(req_body);
        let mut dst: libc::sockaddr_nl = std::mem::zeroed();
        dst.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let sent = libc::sendto(
            fd,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
            0,
            &dst as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        let mut buf = vec![0u8; 8192];
        let n = if sent < 0 {
            -1
        } else {
            libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
        };
        libc::close(fd);
        if n < 16 {
            return None;
        }
        buf.truncate(n as usize);
        let len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let ty = u16::from_ne_bytes([buf[4], buf[5]]);
        if ty == NLMSG_ERROR || ty == NLMSG_DONE || len < 16 || len > buf.len() {
            return None;
        }
        Some(buf[16..len].to_vec())
    }
}

/// Walk netlink attributes, returning the payload of the first of `want`.
fn attr(mut b: &[u8], want: u16) -> Option<&[u8]> {
    while b.len() >= 4 {
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_ne_bytes([b[2], b[3]]);
        if len < 4 || len > b.len() {
            return None;
        }
        if ty == want {
            return Some(&b[4..len]);
        }
        let step = (len + 3) & !3;
        if step >= b.len() {
            return None;
        }
        b = &b[step..];
    }
    None
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Ask the kernel for the timing of one TCP connection in our own network
/// namespace.
pub fn tcp_info(local: &Addr, remote: &Addr) -> Option<TcpInfo> {
    let mut r = Vec::with_capacity(56);
    r.push(if local.v6 {
        libc::AF_INET6
    } else {
        libc::AF_INET
    } as u8);
    r.push(libc::IPPROTO_TCP as u8);
    r.push(1 << 1); // INET_DIAG_INFO
    r.push(0);
    r.extend_from_slice(&u32::MAX.to_ne_bytes()); // all states
    r.extend_from_slice(&local.port.to_be_bytes());
    r.extend_from_slice(&remote.port.to_be_bytes());
    r.extend_from_slice(&local.raw);
    r.extend_from_slice(&remote.raw);
    r.extend_from_slice(&0u32.to_ne_bytes()); // any interface
    r.extend_from_slice(&[0xff; 8]); // no cookie
    let resp = diag(&r)?;
    let info = attr(resp.get(72..)?, 2)?;
    Some(TcpInfo {
        retransmits: *info.get(2)?,
        last_sent_ms: u32_at(info, 44)?,
        last_recv_ms: u32_at(info, 52)?,
    })
}

/// Inode of the socket connected to Unix socket `ino`.
pub fn unix_peer(ino: u64) -> Option<u64> {
    let mut r = Vec::with_capacity(24);
    r.push(libc::AF_UNIX as u8);
    r.push(0);
    r.extend_from_slice(&0u16.to_ne_bytes());
    r.extend_from_slice(&u32::MAX.to_ne_bytes()); // all states
    r.extend_from_slice(&(ino as u32).to_ne_bytes());
    r.extend_from_slice(&0x4u32.to_ne_bytes()); // UDIAG_SHOW_PEER
    r.extend_from_slice(&[0xff; 8]); // no cookie
    let resp = diag(&r)?;
    let peer = attr(resp.get(16..)?, 2)?; // UNIX_DIAG_PEER
    u32_at(peer, 0).map(|v| v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_addr() {
        let a = parse_addr("0100007F:1538").unwrap();
        if cfg!(target_endian = "little") {
            assert_eq!(a.to_string(), "127.0.0.1:5432");
        }
        assert!(!a.is_unset());
        assert!(parse_addr("00000000:0000").unwrap().is_unset());
    }

    #[test]
    fn v6_addr() {
        let a = parse_addr("00000000000000000000000001000000:0050").unwrap();
        if cfg!(target_endian = "little") {
            assert_eq!(a.to_string(), "[::1]:80");
        }
        let m = parse_addr("0000000000000000FFFF00000100007F:01BB").unwrap();
        if cfg!(target_endian = "little") {
            assert_eq!(m.to_string(), "127.0.0.1:443");
        }
    }

    #[test]
    fn tcp_table() {
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4455 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:C350 0100007F:1F90 01 00000000:00000000 00:00000000 00000000     0        0 4460 1 0000000000000000 20 4 30 10 -1\n";
        let mut m = HashMap::new();
        parse_inet(t, true, &mut m);
        assert!(matches!(
            m.get(&4455),
            Some(Sock::Tcp {
                listening: true,
                ..
            })
        ));
        match m.get(&4460) {
            Some(Sock::Tcp {
                remote,
                listening: false,
                connecting: false,
                ..
            }) => assert_eq!(remote.port, 8080),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn unix_table() {
        let t = "Num       RefCount Protocol Flags    Type St Inode Path\n0000000000000000: 00000002 00000000 00010000 0001 01 777 /run/x.sock\n0000000000000000: 00000003 00000000 00000000 0001 03 778\n";
        let mut m = HashMap::new();
        parse_unix(t, &mut m);
        assert_eq!(
            m.get(&777),
            Some(&Sock::Unix {
                path: "/run/x.sock".into(),
                stream: true,
                listening: true,
                connected: false
            })
        );
        assert!(matches!(
            m.get(&778),
            Some(Sock::Unix {
                connected: true,
                ..
            })
        ));
    }

    #[test]
    fn attrs() {
        // two attributes: type 1 (len 5, padded), type 2 (u32)
        let b = [5, 0, 1, 0, 9, 0, 0, 0, 8, 0, 2, 0, 7, 0, 0, 0];
        assert_eq!(attr(&b, 2), Some(&[7u8, 0, 0, 0][..]));
        assert_eq!(attr(&b, 3), None);
    }
}
