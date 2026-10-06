use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::manifest;
use crate::naming;

pub const DEFAULT_PORT: u16 = 8642;
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct ServeConfig {
    pub dist_dir: PathBuf,
    pub port: u16,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            dist_dir: PathBuf::from("relay-dist"),
            port: DEFAULT_PORT,
        }
    }
}

pub struct Server {
    pub listener: TcpListener,
    pub dist_dir: PathBuf,
}

pub fn prepare(dist_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dist_dir.join("bins"))?;
    std::fs::create_dir_all(dist_dir.join("incoming"))?;
    std::fs::create_dir_all(dist_dir.join("logs"))?;
    manifest::refresh(dist_dir)?;
    refresh_symlinks(dist_dir)
}

pub fn bind(cfg: &ServeConfig) -> std::io::Result<Server> {
    prepare(&cfg.dist_dir)?;
    let listener = TcpListener::bind(("0.0.0.0", cfg.port))?;
    Ok(Server {
        listener,
        dist_dir: cfg.dist_dir.clone(),
    })
}

pub fn serve(cfg: ServeConfig) -> std::io::Result<()> {
    let server = bind(&cfg)?;
    let port = server.listener.local_addr()?.port();
    println!(
        "dev-relay serving {} on http://{}:{port}",
        cfg.dist_dir.display(),
        lan_ip()
    );
    let dist = Arc::new(server.dist_dir.clone());
    {
        let dist = Arc::clone(&dist);
        std::thread::Builder::new()
            .name("relay-watcher".into())
            .spawn(move || watcher(dist))?;
    }
    for stream in server.listener.incoming() {
        let Ok(stream) = stream else { continue };
        let dist = Arc::clone(&dist);
        std::thread::Builder::new()
            .name("relay-conn".into())
            .spawn(move || {
                handle_stream(stream, &dist);
            })?;
    }
    Ok(())
}

fn watcher(dist: Arc<PathBuf>) {
    loop {
        let moved = poll_incoming(&dist);
        if moved > 0
            && let Ok(m) = manifest::refresh(&dist)
        {
            let _ = refresh_symlinks_for(&dist, &m);
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn poll_incoming(dist: &Path) -> usize {
    let incoming = dist.join("incoming");
    let Ok(entries) = std::fs::read_dir(&incoming) else {
        return 0;
    };
    let mut moved = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmp-") || !naming::is_bin_name(&name) {
            continue;
        }
        if std::fs::rename(incoming.join(&name), dist.join("bins").join(&name)).is_ok() {
            moved += 1;
        }
    }
    moved
}

fn refresh_symlinks(dist: &Path) -> std::io::Result<()> {
    match manifest::scan_bins(&dist.join("bins")) {
        Ok(m) => refresh_symlinks_for(dist, &m),
        Err(e) => Err(e),
    }
}

fn refresh_symlinks_for(dist: &Path, m: &manifest::Manifest) -> std::io::Result<()> {
    for (key, link) in [("", "latest"), ("exe", "latest.exe")] {
        if let Some(name) = m.latest.get(key) {
            let link_path = dist.join(link);
            let _ = std::fs::remove_file(&link_path);
            std::os::unix::fs::symlink(Path::new("bins").join(name), link_path)?;
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

pub(crate) fn handle_stream(mut stream: TcpStream, dist: &Path) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let Some(req) = read_request(&mut stream) else {
        let _ = respond_simple(&mut stream, 400, "Bad Request", b"unparsable request");
        return;
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/manifest.json") => serve_file(&mut stream, &dist.join("manifest.json")),
        ("GET", "/latest") | ("GET", "/latest.exe") => {
            serve_file(&mut stream, &dist.join(&req.path[1..]))
        }
        ("GET", p) if p.starts_with("/bins/") => match valid_name(&p["/bins/".len()..]) {
            Some(name) => serve_file(&mut stream, &dist.join("bins").join(name)),
            None => {
                let _ = respond_simple(&mut stream, 400, "Bad Name", b"invalid bin name");
            }
        },
        ("GET", p) if p.starts_with("/logs/") => match valid_name(&p["/logs/".len()..]) {
            Some(name) => {
                serve_file(&mut stream, &dist.join("logs").join(format!("{name}.jsonl")))
            }
            None => {
                let _ = respond_simple(&mut stream, 400, "Bad Name", b"invalid bin name");
            }
        },
        ("POST", "/log") => {
            let bin = query_param(&req.query, "bin").filter(|b| naming::is_bin_name(b));
            match bin {
                Some(bin) => {
                    let logs = dist.join("logs");
                    let _ = std::fs::create_dir_all(&logs);
                    let mut body = req.body;
                    body.push(b'\n');
                    let result = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(logs.join(format!("{bin}.jsonl")))
                        .and_then(|mut f| f.write_all(&body));
                    match result {
                        Ok(()) => {
                            let _ = respond_simple(&mut stream, 204, "No Content", b"");
                        }
                        Err(_) => {
                            let _ = respond_simple(&mut stream, 500, "Write Failed", b"");
                        }
                    }
                }
                None => {
                    {
                        let _ = respond_simple(&mut stream, 400, "Bad Name", b"missing or invalid bin param");
                    }
                }
            }
        }
        _ => {
            let _ = respond_simple(&mut stream, 404, "Not Found", b"not found");
        }
    }
}

fn valid_name(raw: &str) -> Option<&str> {
    naming::is_bin_name(raw).then_some(raw)
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn serve_file(stream: &mut TcpStream, path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        let _ = respond_simple(stream, 404, "Not Found", b"");
        return;
    };
    if !meta.is_file() {
        let _ = respond_simple(stream, 404, "Not Found", b"");
        return;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        let _ = respond_simple(stream, 404, "Not Found", b"");
        return;
    };
    let len = meta.len();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        content_type(path)
    );
    if stream.write_all(head.as_bytes()).and_then(|_| stream.flush()).is_err() {
        return;
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        _ => "application/octet-stream",
    }
}

fn respond_simple(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let header = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().ok()?;
        }
    }
    if content_length > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Request {
        method,
        path,
        query,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn lan_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "0.0.0.0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::sync::mpsc;

    fn start_test_server(dist: PathBuf) -> u16 {
        let server = bind(&ServeConfig {
            dist_dir: dist.clone(),
            port: 0,
        })
        .unwrap();
        let port = server.listener.local_addr().unwrap().port();
        let handler_dist = dist.clone();
        std::thread::spawn(move || {
            for stream in server.listener.incoming().flatten() {
                let d = handler_dist.clone();
                std::thread::spawn(move || {
                    handle_stream(stream, &d);
                });
            }
        });
        let wdist = Arc::new(dist);
        std::thread::spawn(move || watcher(wdist));
        port
    }

    fn http(port: u16, method: &str, target: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let head = format!(
            "{method} {target} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).unwrap();
        let text = String::from_utf8_lossy(&resp);
        let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let split = find_header_end(&resp).unwrap();
        (status, resp[split + 4..].to_vec())
    }

    #[test]
    fn serve_round_trip() {
        let dist = testutil::temp_dir("serve");
        let port = start_test_server(dist.clone());

        std::fs::write(dist.join("incoming").join("0123abcd"), b"fake-binary-bytes").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let (code, body) = http(port, "GET", "/manifest.json", b"");
            let ok = code == 200 && String::from_utf8_lossy(&body).contains("0123abcd");
            if ok {
                assert!(String::from_utf8_lossy(&body).contains(r#""latest":{"":"0123abcd""#));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never published the bin"
            );
        }

        let (code, body) = http(port, "GET", "/bins/0123abcd", b"");
        assert_eq!(code, 200);
        assert_eq!(body, b"fake-binary-bytes");

        let (code, body) = http(port, "GET", "/latest", b"");
        assert_eq!(code, 200);
        assert_eq!(body, b"fake-binary-bytes");

        let (code, _) = http(port, "GET", "/latest.exe", b"");
        assert_eq!(code, 404);

        let (code, _) = http(port, "GET", "/bins/../../etc/passwd", b"");
        assert_eq!(code, 400);

        let log_line = br#"{"level":"INFO","target":"aim","msg":"hello"}"#;
        let (code, _) = http(port, "POST", "/log?bin=0123abcd", log_line);
        assert_eq!(code, 204);
        let (code, body) = http(port, "GET", "/logs/0123abcd", b"");
        assert_eq!(code, 200);
        assert_eq!(&body[..body.len() - 1], log_line);
        assert_eq!(body.last(), Some(&b'\n'));

        let (code, _) = http(port, "POST", "/log?bin=..%2Fetc", log_line);
        assert_eq!(code, 400);

        let (code, _) = http(port, "GET", "/nope", b"");
        assert_eq!(code, 404);

        testutil::rmrf(&dist);
    }

    #[test]
    fn body_read_waits_for_full_content_length() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            let (s, _) = listener.accept().unwrap();
            let mut s = s;
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(text.contains("Content-Length: 5"), "got {text}");
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        let port = rx.recv().unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"POST /log HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhel").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        s.write_all(b"lo").unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200"), "got {text}");
        assert!(text.ends_with("ok"), "got {text}");
    }
}
