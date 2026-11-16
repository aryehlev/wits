//! A minimal HTTP/1.1 server and client: enough for wits agents to ask
//! each other questions. One request per connection, JSON bodies.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct Request {
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
}

pub fn encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(kv), String::new()),
        })
        .collect()
}

fn read_head(s: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut b = [0u8; 1024];
    loop {
        let n = s.read(&mut b)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&b[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Serve forever: each connection gets a thread, at most `max` at once.
pub fn serve<F>(addr: &str, max: usize, handler: F) -> io::Result<()>
where
    F: Fn(&Request) -> (u16, String) + Send + Sync + 'static,
{
    let l = TcpListener::bind(addr)?;
    let handler = Arc::new(handler);
    let busy = Arc::new(AtomicUsize::new(0));
    for conn in l.incoming() {
        let Ok(mut s) = conn else { continue };
        if busy.load(Ordering::SeqCst) >= max {
            let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        busy.fetch_add(1, Ordering::SeqCst);
        let (h, b) = (handler.clone(), busy.clone());
        std::thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            if let Ok(head) = read_head(&mut s) {
                let mut lines = head.split("\r\n");
                let first = lines.next().unwrap_or("");
                let target = first.split(' ').nth(1).unwrap_or("/");
                let (path, q) = target.split_once('?').unwrap_or((target, ""));
                let headers = lines
                    .take_while(|l| !l.is_empty())
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let req = Request {
                    path: path.to_string(),
                    query: parse_query(q),
                    headers,
                };
                let (code, body) = if first.starts_with("GET ") {
                    h(&req)
                } else {
                    (400, "{\"error\":\"only GET\"}".into())
                };
                let resp = format!(
                    "HTTP/1.1 {code} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    status_text(code),
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
            b.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// GET `path` (with query) from `addr`, naming it `host`. Returns status
/// and body.
pub fn get_addr(
    addr: SocketAddr,
    host: &str,
    path: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let mut s = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(3)))
        .map_err(|e| format!("{addr}: {e}"))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n");
    s.write_all(req.as_bytes()).map_err(|e| format!("{addr}: {e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| format!("{addr}: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{addr}: bad reply"))?;
    let code = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("{addr}: bad status"))?;
    Ok((code, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_round_trip() {
        let q = parse_query("pod=shop%2Fapi&secs=1.5&x=a+b&flag");
        assert_eq!(q["pod"], "shop/api");
        assert_eq!(q["secs"], "1.5");
        assert_eq!(q["x"], "a b");
        assert_eq!(q["flag"], "");
        assert_eq!(decode(&encode("ns/pod name&x")), "ns/pod name&x");
    }

    #[test]
    fn serve_and_get() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let a = addr.to_string();
        std::thread::spawn(move || {
            let _ = serve(&a, 4, |r| (200, format!("{{\"path\":\"{}\",\"q\":\"{}\"}}", r.path, r.query["k"])));
        });
        let mut got = None;
        for _ in 0..50 {
            if let Ok(r) = get_addr(addr, "t", "/v1/x?k=v", None, Duration::from_secs(2)) {
                got = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(got.unwrap(), (200, "{\"path\":\"/v1/x\",\"q\":\"v\"}".to_string()));
    }
}
