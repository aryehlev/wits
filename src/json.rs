//! A small JSON reader, for the files and replies wits itself writes
//! (`wits run -o`, `wits diff`, the node agent). Writing is done by hand in
//! out.rs.

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

static NULL: Value = Value::Null;

impl Value {
    pub fn get(&self, key: &str) -> &Value {
        match self {
            Value::Obj(v) => v.iter().find(|(k, _)| k == key).map(|x| &x.1).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by the Linux backends
    pub fn int(&self) -> Option<i64> {
        self.num().map(|n| n as i64)
    }
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by the Linux backends
    pub fn bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Value] {
        match self {
            Value::Arr(v) => v,
            _ => &[],
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl std::fmt::Display for Value {
    /// Compact JSON.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) if !n.is_finite() => f.write_str("null"),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => write!(f, "{}", *n as i64),
            Value::Num(n) => write!(f, "{n}"),
            Value::Str(s) => f.write_str(&crate::out::esc(s)),
            Value::Arr(a) => {
                f.write_str("[")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Value::Obj(o) => {
                f.write_str("{")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{}:{v}", crate::out::esc(k))?;
                }
                f.write_str("}")
            }
        }
    }
}

pub fn parse(s: &str) -> Result<Value, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let v = value(b, &mut i, 0)?;
    ws(b, &mut i);
    if i != b.len() {
        return Err(format!("trailing data at byte {i}"));
    }
    Ok(v)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\n' | b'\r' | b'\t') {
        *i += 1;
    }
}

fn value(b: &[u8], i: &mut usize, depth: u32) -> Result<Value, String> {
    if depth > 128 {
        return Err("nested too deeply".into());
    }
    ws(b, i);
    let err = |i: usize| format!("bad JSON at byte {i}");
    match b.get(*i) {
        None => Err("unexpected end of JSON".into()),
        Some(b'{') => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Ok(Value::Obj(v));
            }
            loop {
                ws(b, i);
                let Value::Str(k) = string(b, i)? else {
                    return Err(err(*i));
                };
                ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return Err(err(*i));
                }
                *i += 1;
                v.push((k, value(b, i, depth + 1)?));
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b'}') => {
                        *i += 1;
                        return Ok(Value::Obj(v));
                    }
                    _ => return Err(err(*i)),
                }
            }
        }
        Some(b'[') => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Ok(Value::Arr(v));
            }
            loop {
                v.push(value(b, i, depth + 1)?);
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b']') => {
                        *i += 1;
                        return Ok(Value::Arr(v));
                    }
                    _ => return Err(err(*i)),
                }
            }
        }
        Some(b'"') => string(b, i),
        Some(b't') if b[*i..].starts_with(b"true") => {
            *i += 4;
            Ok(Value::Bool(true))
        }
        Some(b'f') if b[*i..].starts_with(b"false") => {
            *i += 5;
            Ok(Value::Bool(false))
        }
        Some(b'n') if b[*i..].starts_with(b"null") => {
            *i += 4;
            Ok(Value::Null)
        }
        Some(c) if *c == b'-' || c.is_ascii_digit() => {
            let st = *i;
            *i += 1;
            while *i < b.len() && matches!(b[*i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                *i += 1;
            }
            std::str::from_utf8(&b[st..*i])
                .ok()
                .and_then(|s| s.parse().ok())
                .map(Value::Num)
                .ok_or_else(|| err(st))
        }
        _ => Err(err(*i)),
    }
}

fn string(b: &[u8], i: &mut usize) -> Result<Value, String> {
    if b.get(*i) != Some(&b'"') {
        return Err(format!("expected a string at byte {i}"));
    }
    *i += 1;
    let mut out: Vec<u8> = Vec::new();
    while let Some(&c) = b.get(*i) {
        *i += 1;
        match c {
            b'"' => return Ok(Value::Str(String::from_utf8_lossy(&out).into_owned())),
            b'\\' => {
                let e = *b.get(*i).ok_or("unexpected end of JSON")?;
                *i += 1;
                match e {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' => {
                        let hex = |i: &mut usize| -> Result<u32, String> {
                            let h = b.get(*i..*i + 4).ok_or("bad \\u escape")?;
                            *i += 4;
                            u32::from_str_radix(std::str::from_utf8(h).unwrap_or("x"), 16)
                                .map_err(|_| "bad \\u escape".to_string())
                        };
                        let mut cp = hex(i)?;
                        if (0xd800..0xdc00).contains(&cp) && b.get(*i..*i + 2) == Some(b"\\u") {
                            *i += 2;
                            let lo = hex(i)?;
                            cp = 0x10000 + ((cp - 0xd800) << 10) + (lo.wrapping_sub(0xdc00) & 0x3ff);
                        }
                        let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    other => out.push(other),
                }
            }
            c => out.push(c),
        }
    }
    Err("unterminated string".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_shapes() {
        let v = parse(r#"{"a":1,"b":[true,false,null],"c":{"d":"x\"yé"},"e":-2.5e3}"#).unwrap();
        assert_eq!(v.get("a").num(), Some(1.0));
        assert_eq!(v.get("b").arr().len(), 3);
        assert_eq!(v.get("c").get("d").str(), Some("x\"yé"));
        assert_eq!(v.get("e").num(), Some(-2500.0));
        assert!(v.get("missing").is_null());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{} x").is_err());
    }

    #[test]
    fn reads_what_out_writes() {
        let s = crate::out::esc("tab\there \"quoted\" \u{1} ünï");
        assert_eq!(parse(&s).unwrap().str(), Some("tab\there \"quoted\" \u{1} ünï"));
    }

    #[test]
    fn round_trip() {
        let t = r#"{"a":[1,2.5,null,true],"b":{"c":"x\"y\u00e9"},"d":-3}"#;
        let v = parse(t).unwrap();
        assert_eq!(parse(&v.to_string()).unwrap(), v);
        assert_eq!(v.get("d").to_string(), "-3");
    }
}
