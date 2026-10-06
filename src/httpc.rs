use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct Base {
    pub host: String,
    pub port: u16,
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn parse_base(url: &str) -> Result<Base, String> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?)
        }
        _ => (rest.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("bad url: {url}"));
    }
    Ok(Base { host, port })
}

pub fn request(
    base: &Base,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    max: usize,
) -> Result<Response, String> {
    let mut out: Vec<u8> = Vec::new();
    let status = request_into(base, method, target, body, |chunk| {
        if out.len() + chunk.len() > max {
            return Err("response too large".into());
        }
        out.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(Response { status, body: out })
}

pub fn request_into(
    base: &Base,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    mut sink: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<u16, String> {
    let mut stream = connect(base)?;
    write_request(&mut stream, method, target, body)?;
    let mut head: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 16 * 1024];
    let head_end = loop {
        if let Some(p) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if head.len() > 16 * 1024 {
            return Err("response headers too large".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before headers".into());
        }
        head.extend_from_slice(&chunk[..n]);
    };
    let header = std::str::from_utf8(&head[..head_end]).map_err(|_| "bad header encoding")?;
    let status = header
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad status line")?;
    let mut length: Option<usize> = None;
    for line in header.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            length = v.trim().parse().ok();
        }
    }
    let body_start = &head[head_end + 4..];
    let mut remaining = length.map(|n| n.saturating_sub(body_start.len()));
    sink(body_start)?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let want = match remaining {
            Some(0) => break,
            Some(r) => chunk.len().min(r),
            None => chunk.len(),
        };
        let n = stream.read(&mut chunk[..want]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        sink(&chunk[..n])?;
        if let Some(r) = remaining.as_mut() {
            *r -= n;
        }
    }
    Ok(status)
}

fn connect(base: &Base) -> Result<TcpStream, String> {
    let addr = (base.host.as_str(), base.port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("no address for {}:{}", base.host, base.port))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

fn write_request(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
) -> Result<(), String> {
    let len = body.map_or(0, <[u8]>::len);
    let head = format!(
        "{method} {target} HTTP/1.1\r\nHost: \r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    if let Some(body) = body {
        stream.write_all(body).map_err(|e| e.to_string())?;
    }
    stream.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let b = parse_base("http://192.168.1.5:8642").unwrap();
        assert_eq!((b.host.as_str(), b.port), ("192.168.1.5", 8642));
        let b = parse_base("192.168.1.5:8642/").unwrap();
        assert_eq!((b.host.as_str(), b.port), ("192.168.1.5", 8642));
        let b = parse_base("example.com").unwrap();
        assert_eq!(b.port, 80);
        assert!(parse_base("").is_err());
    }
}
