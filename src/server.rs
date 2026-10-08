use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::control::ControlState;
use crate::manifest;
use crate::naming;

pub const DEFAULT_PORT: u16 = 8642;
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_SHOT_BODY: usize = 64 * 1024 * 1024;

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

pub struct ServerState {
    pub dist: PathBuf,
    pub public_base: Mutex<String>,
    pub control: ControlState,
}

pub struct Server {
    pub listener: TcpListener,
    pub state: Arc<ServerState>,
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
    let port = listener.local_addr()?.port();
    let state = Arc::new(ServerState {
        dist: cfg.dist_dir.clone(),
        public_base: Mutex::new(format!("http://{}:{port}", lan_ip())),
        control: ControlState::new(),
    });
    Ok(Server { listener, state })
}

pub fn serve(cfg: ServeConfig) -> std::io::Result<()> {
    let server = bind(&cfg)?;
    let port = server.listener.local_addr()?.port();
    println!(
        "dev-relay serving {} on http://{}:{port}",
        cfg.dist_dir.display(),
        lan_ip()
    );
    {
        let watcher_dist = Arc::new(server.state.dist.clone());
        std::thread::Builder::new()
            .name("relay-watcher".into())
            .spawn(move || watcher(watcher_dist))?;
    }
    for stream in server.listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&server.state);
        std::thread::Builder::new()
            .name("relay-conn".into())
            .spawn(move || {
                handle_stream(stream, &state);
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

pub(crate) fn handle_stream(mut stream: TcpStream, state: &ServerState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let Some(req) = read_request(&mut stream) else {
        let _ = respond_simple(&mut stream, 400, "Bad Request", b"unparsable request");
        return;
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            let launcher = launcher_path(&state.dist, std::env::consts::EXE_SUFFIX == naming::EXE_SUFFIX)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(missing)".into());
            let html = format!(
                "<html><head><title>dev-relay</title></head><body>\r\n<h1>dev-relay</h1>\r\n<ul>\r\n<li><a href=\"/manifest.json\">manifest.json</a></li>\r\n<li><a href=\"/latest\">latest ({})</a></li>\r\n<li><a href=\"/latest.exe\">latest.exe ({})</a></li>\r\n<li><a href=\"/launcher\">launcher shim ({})</a></li>\r\n</ul>\r\n</body></html>\r\n",
                state.dist.join("latest").read_link().map(|p| p.display().to_string()).unwrap_or_default(),
                state.dist.join("latest.exe").read_link().map(|p| p.display().to_string()).unwrap_or_default(),
                launcher,
            );
            let _ = respond_bytes(&mut stream, 200, "OK", "text/html; charset=utf-8", html.as_bytes(), &[]);
        }
        ("GET", "/launcher") | ("GET", "/launcher.exe") => {
            let want_exe = req.path.ends_with(naming::EXE_SUFFIX);
            match launcher_path(&state.dist, want_exe) {
                Some(path) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("relay-launcher");
                    serve_file_as(&mut stream, &path, Some(name))
                }
                None => {
                    let _ = respond_simple(
                        &mut stream,
                        404,
                        "Not Found",
                        b"launcher shim not found next to the serve binary or in dist",
                    );
                }
            }
        }
        ("GET", "/manifest.json") => serve_file(&mut stream, &state.dist.join("manifest.json")),
        ("GET", "/latest") | ("GET", "/latest.exe") => {
            let path = state.dist.join(&req.path[1..]);
            let name = path
                .read_link()
                .ok()
                .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(String::from));
            serve_file_as(&mut stream, &path, name.as_deref())
        }
        ("GET", p) if p.starts_with("/bins/") => match valid_name(&p["/bins/".len()..]) {
            Some(name) => serve_file(&mut stream, &state.dist.join("bins").join(name)),
            None => {
                let _ = respond_simple(&mut stream, 400, "Bad Name", b"invalid bin name");
            }
        },
        ("GET", p) if p.starts_with("/logs/") => match valid_name(&p["/logs/".len()..]) {
            Some(name) => serve_file(
                &mut stream,
                &state.dist.join("logs").join(format!("{name}.jsonl")),
            ),
            None => {
                let _ = respond_simple(&mut stream, 400, "Bad Name", b"invalid bin name");
            }
        },
        ("GET", "/control") => handle_control(&mut stream, state, &req.query),
        ("POST", "/shot") => handle_shot(&mut stream, state, &req.query, &req.body),
        ("GET", p) if p.starts_with("/shots/") => serve_shot(&mut stream, state, p),
        ("POST", "/mcp") => crate::mcp::handle(&mut stream, state, &req.body),
        ("POST", "/log") => {
            let bin = query_param(&req.query, "bin").filter(|b| naming::is_bin_name(b));
            match bin {
                Some(bin) => {
                    let logs = state.dist.join("logs");
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
                    let _ = respond_simple(
                        &mut stream,
                        400,
                        "Bad Name",
                        b"missing or invalid bin param",
                    );
                }
            }
        }
        _ => {
            let _ = respond_simple(&mut stream, 404, "Not Found", b"not found");
        }
    }
}

fn handle_control(stream: &mut TcpStream, state: &ServerState, query: &str) {
    let bin = query_param(query, "bin").filter(|b| naming::is_bin_name(b));
    let session = query_param(query, "session").filter(|s| naming::is_session_id(s));
    match (bin, session) {
        (Some(bin), Some(session)) => {
            let cmds = state.control.poll(bin, session);
            let body = serde_json::to_vec(&cmds).unwrap_or_default();
            let _ = respond_bytes(stream, 200, "OK", "application/json", &body, &[]);
        }
        _ => {
            let _ = respond_simple(
                stream,
                400,
                "Bad Request",
                b"bin and session params required",
            );
        }
    }
}

fn handle_shot(stream: &mut TcpStream, state: &ServerState, query: &str, body: &[u8]) {
    let Some(bin) = query_param(query, "bin").filter(|b| naming::is_bin_name(b)) else {
        let _ = respond_simple(stream, 400, "Bad Name", b"missing or invalid bin param");
        return;
    };
    let Some(session) = query_param(query, "session").filter(|s| naming::is_session_id(s)) else {
        let _ = respond_simple(
            stream,
            400,
            "Bad Session",
            b"missing or invalid session param",
        );
        return;
    };
    let kind = query_param(query, "kind").unwrap_or("color");
    let Some(seq) = query_param(query, "seq").and_then(|s| s.parse::<u64>().ok()) else {
        let _ = respond_simple(stream, 400, "Bad Seq", b"missing seq param");
        return;
    };
    let dir = state.dist.join("shots").join(bin).join(session);
    let write = |ext: &str, bytes: &[u8]| std::fs::write(dir.join(format!("{seq}.{ext}")), bytes);
    match kind {
        "color" => {
            if std::fs::create_dir_all(&dir)
                .and(write("png", body))
                .is_ok()
            {
                let _ = respond_simple(stream, 204, "No Content", b"");
            } else {
                let _ = respond_simple(stream, 500, "Write Failed", b"");
            }
        }
        "depth" => {
            let w = query_param(query, "w")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let h = query_param(query, "h")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let near = query_param(query, "near")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let far = query_param(query, "far")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            let proj = query_param(query, "proj")
                .filter(|p| {
                    !p.is_empty()
                        && p.len() <= 16
                        && p.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
                .unwrap_or("unknown");
            let meta = serde_json::json!({
                "width": w,
                "height": h,
                "near": near,
                "far": far,
                "projection": proj,
            });
            let meta_bytes = serde_json::to_vec(&meta).unwrap_or_default();
            if std::fs::create_dir_all(&dir).is_ok()
                && write("depth.json", &meta_bytes).is_ok()
                && write("depthbin", body).is_ok()
            {
                let _ = respond_simple(stream, 204, "No Content", b"");
            } else {
                let _ = respond_simple(stream, 500, "Write Failed", b"");
            }
        }
        _ => {
            let _ = respond_simple(stream, 400, "Bad Kind", b"kind must be color|depth");
        }
    }
}

fn serve_shot(stream: &mut TcpStream, state: &ServerState, path: &str) {
    let rest = &path["/shots/".len()..];
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3
        || !naming::is_bin_name(parts[0])
        || !naming::is_session_id(parts[1])
        || !valid_shot_file(parts[2])
    {
        let _ = respond_simple(stream, 400, "Bad Path", b"invalid shot path");
        return;
    }
    let file = state
        .dist
        .join("shots")
        .join(parts[0])
        .join(parts[1])
        .join(parts[2]);
    serve_file(stream, &file);
}

fn valid_shot_file(name: &str) -> bool {
    for suffix in [".depth.json", ".depthbin", ".png"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return !stem.is_empty()
                && stem.len() <= 20
                && stem.bytes().all(|b| b.is_ascii_digit());
        }
    }
    false
}

fn valid_name(raw: &str) -> Option<&str> {
    naming::is_bin_name(raw).then_some(raw)
}

fn launcher_path(dist: &Path, want_exe: bool) -> Option<PathBuf> {
    let name = if want_exe { "relay-launcher.exe" } else { "relay-launcher" };
    let in_dist = dist.join(name);
    if in_dist.is_file() {
        return Some(in_dist);
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)));
    sibling.filter(|p| p.is_file())
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn serve_file(stream: &mut TcpStream, path: &Path) {
    serve_file_as(stream, path, None)
}

fn serve_file_as(stream: &mut TcpStream, path: &Path, download_name: Option<&str>) {
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
    let disposition = download_name
        .map(|n| format!("Content-Disposition: attachment; filename=\"{n}\"\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\n{disposition}Connection: close\r\n\r\n",
        content_type(path)
    );
    if stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
    {
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
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

pub(crate) fn respond_bytes(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn respond_simple(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &[u8],
) -> std::io::Result<()> {
    respond_bytes(stream, code, reason, "text/plain", body, &[])
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
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let cap = if path.starts_with("/shot") {
        MAX_SHOT_BODY
    } else {
        MAX_BODY
    };
    if content_length > cap {
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
pub(crate) use tests::start_test_server;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    pub(crate) fn start_test_server(dist: PathBuf) -> (u16, Arc<ServerState>) {
        let server = bind(&ServeConfig {
            dist_dir: dist.clone(),
            port: 0,
        })
        .unwrap();
        let port = server.listener.local_addr().unwrap().port();
        *server.state.public_base.lock().unwrap() = format!("http://127.0.0.1:{port}");
        let accept_state = Arc::clone(&server.state);
        let ret_state = Arc::clone(&server.state);
        let watcher_dist = dist.clone();
        std::thread::spawn(move || {
            for stream in server.listener.incoming().flatten() {
                let state = Arc::clone(&accept_state);
                std::thread::spawn(move || {
                    handle_stream(stream, &state);
                });
            }
        });
        std::thread::spawn(move || watcher(Arc::new(watcher_dist)));
        (port, ret_state)
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
        let (port, _state) = start_test_server(dist.clone());

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
    fn control_poll_and_shot_endpoints() {
        let dist = testutil::temp_dir("control-shot");
        let (port, state) = start_test_server(dist.clone());
        state.control.wait_ms.store(50, Ordering::Relaxed);

        let (code, body) = http(port, "GET", "/control?bin=0123abcd&session=ab12cd", b"");
        assert_eq!(code, 200);
        assert_eq!(body, b"[]");
        assert_eq!(
            state.control.active_session("0123abcd"),
            Some("ab12cd".into())
        );

        let id = state.control.enqueue(
            &dist,
            "0123abcd",
            "ab12cd",
            serde_json::json!({"op": "screenshot"}),
        );
        let (code, body) = http(port, "GET", "/control?bin=0123abcd&session=ab12cd", b"");
        assert_eq!(code, 200);
        let cmds: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["op"], "screenshot");
        assert_eq!(cmds[0]["id"], id.as_str());

        let rec = std::fs::read_to_string(dist.join("inputs").join("0123abcd.jsonl")).unwrap();
        assert!(rec.contains(&id));
        assert!(rec.contains(r#""session":"ab12cd""#));

        let (code, _) = http(
            port,
            "POST",
            "/shot?bin=0123abcd&session=ab12cd&kind=color&seq=1",
            b"fake-png-bytes",
        );
        assert_eq!(code, 204);
        let (code, body) = http(port, "GET", "/shots/0123abcd/ab12cd/1.png", b"");
        assert_eq!(code, 200);
        assert_eq!(body, b"fake-png-bytes");

        let depth_target = "/shot?bin=0123abcd&session=ab12cd&kind=depth&seq=2&w=4&h=2&near=0.1&far=100.5&proj=perspective";
        let (code, _) = http(port, "POST", depth_target, b"raw-depth");
        assert_eq!(code, 204);
        let (code, body) = http(port, "GET", "/shots/0123abcd/ab12cd/2.depthbin", b"");
        assert_eq!(code, 200);
        assert_eq!(body, b"raw-depth");
        let sidecar = std::fs::read_to_string(
            dist.join("shots")
                .join("0123abcd")
                .join("ab12cd")
                .join("2.depth.json"),
        )
        .unwrap();
        assert!(sidecar.contains(r#""width":4"#));
        assert!(sidecar.contains(r#""far":100.5"#));
        assert!(sidecar.contains(r#""projection":"perspective""#));

        let (code, _) = http(port, "GET", "/shots/0123abcd/ab12cd/x.png", b"");
        assert_eq!(code, 400);
        let (code, _) = http(port, "GET", "/shots/0123abcd/ab12cd/../1.png", b"");
        assert_eq!(code, 400);
        let (code, _) = http(
            port,
            "POST",
            "/shot?bin=0123abcd&session=SHORT&kind=color&seq=3",
            b"x",
        );
        assert_eq!(code, 400);
        let (code, _) = http(
            port,
            "POST",
            "/shot?bin=0123abcd&session=ab12cd&kind=bitmap&seq=3",
            b"x",
        );
        assert_eq!(code, 400);
        let (code, _) = http(port, "GET", "/control?bin=0123abcd", b"");
        assert_eq!(code, 400);

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
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        let port = rx.recv().unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(
            b"POST /log HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhel",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        s.write_all(b"lo").unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200"), "got {text}");
        assert!(text.ends_with("ok"), "got {text}");
    }
}
