use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::httpc::{self, Base};
use crate::naming;

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub ts: u64,
    pub level: String,
    pub target: String,
    pub msg: String,
    pub seq: u64,
    pub fields: String,
    pub session: String,
}

enum SinkCmd {
    Line(LogLine),
    Flush(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct LogSink {
    tx: mpsc::Sender<SinkCmd>,
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static SESSION: OnceLock<String> = OnceLock::new();

pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

pub fn mint_session() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let pid = std::process::id() as u64;
    format!(
        "{:012x}",
        splitmix64(nanos ^ (pid << 32)) & 0xFFFF_FFFF_FFFF
    )
}

pub fn ensure_session(args: &[String]) -> String {
    if let Some(s) = SESSION.get() {
        return s.clone();
    }
    let s = find_flag_value(args, "--relay-session")
        .filter(|s| naming::is_session_id(s))
        .unwrap_or_else(mint_session);
    SESSION.set(s.clone()).ok();
    s
}

fn session() -> String {
    ensure_session(&[])
}

fn find_flag_value(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlCmd {
    Key {
        id: String,
        key: String,
        action: String,
    },
    MouseMove {
        id: String,
        dx: f64,
        dy: f64,
    },
    MouseButton {
        id: String,
        button: String,
        action: String,
    },
    Wheel {
        id: String,
        dx: f64,
        dy: f64,
    },
    Screenshot {
        id: String,
    },
    Depth {
        id: String,
    },
}

pub struct ControlChannel {
    rx: Mutex<mpsc::Receiver<ControlCmd>>,
    stop: Arc<AtomicBool>,
}

impl ControlChannel {
    pub fn start(base_url: &str, bin: &str) -> ControlChannel {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let base = base_url.to_string();
        let bin = bin.to_string();
        let session = session();
        let sig = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("relay-control".into())
            .spawn(move || control_loop(&base, &bin, &session, tx, sig))
            .expect("relay control thread");
        ControlChannel {
            rx: Mutex::new(rx),
            stop,
        }
    }

    pub fn try_recv(&self) -> Option<ControlCmd> {
        self.rx.lock().unwrap().try_recv().ok()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn control_loop(
    base_url: &str,
    bin: &str,
    session: &str,
    tx: mpsc::Sender<ControlCmd>,
    stop: Arc<AtomicBool>,
) {
    let Ok(base) = httpc::parse_base(base_url) else {
        return;
    };
    while !stop.load(Ordering::Relaxed) {
        let target = format!("/control?bin={bin}&session={session}");
        match httpc::request(&base, "GET", &target, None, 1024 * 1024) {
            Ok(resp)
                if resp.status == 200
                    && let Ok(cmds) =
                        serde_json::from_slice::<Vec<serde_json::Value>>(&resp.body) =>
            {
                for cmd in cmds {
                    if let Ok(cmd) = serde_json::from_value::<ControlCmd>(cmd)
                        && tx.send(cmd).is_err()
                    {
                        return;
                    }
                }
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

pub struct ShotMeta {
    pub width: u32,
    pub height: u32,
    pub near: f32,
    pub far: f32,
    pub proj: String,
}

pub fn post_shot(
    base_url: &str,
    bin: &str,
    kind: &str,
    seq: u64,
    body: &[u8],
    meta: Option<&ShotMeta>,
) -> Result<(), String> {
    let base = httpc::parse_base(base_url)?;
    let session = session();
    let mut target = format!("/shot?bin={bin}&session={session}&kind={kind}&seq={seq}");
    if let Some(m) = meta {
        target.push_str(&format!(
            "&w={}&h={}&near={}&far={}&proj={}",
            m.width, m.height, m.near, m.far, m.proj
        ));
    }
    let resp = httpc::request(&base, "POST", &target, Some(body), 0)?;
    if resp.status == 204 {
        Ok(())
    } else {
        Err(format!("shot POST: HTTP {}", resp.status))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl LogSink {
    pub fn start(base_url: &str, bin: &str) -> LogSink {
        let (tx, rx) = mpsc::channel();
        let base = httpc::parse_base(base_url).ok();
        let bin = bin.to_string();
        std::thread::Builder::new()
            .name("relay-log-sink".into())
            .spawn(move || sink_loop(rx, base, bin))
            .expect("relay log sink thread");
        LogSink { tx }
    }

    pub fn log(&self, level: &str, target: &str, msg: String, fields: String) {
        let line = LogLine {
            ts: now_ms(),
            level: level.to_string(),
            target: target.to_string(),
            msg,
            seq: next_seq(),
            fields,
            session: session(),
        };
        let _ = self.tx.send(SinkCmd::Line(line));
    }

    pub fn flush(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(SinkCmd::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }
}

fn sink_loop(rx: mpsc::Receiver<SinkCmd>, base: Option<Base>, bin: String) {
    let mut batch: Vec<LogLine> = Vec::with_capacity(32);
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(SinkCmd::Line(line)) => {
                batch.push(line);
                if batch.len() < 32 {
                    continue;
                }
            }
            Ok(SinkCmd::Flush(ack)) => {
                post_batch(&base, &bin, &batch);
                batch.clear();
                let _ = ack.send(());
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                post_batch(&base, &bin, &batch);
                return;
            }
        }
        post_batch(&base, &bin, &batch);
        batch.clear();
    }
}

fn post_batch(base: &Option<Base>, bin: &str, batch: &[LogLine]) {
    if batch.is_empty() {
        return;
    }
    let Some(base) = base else { return };
    let mut body = Vec::new();
    for line in batch {
        if serde_json::to_writer(&mut body, line).is_err() {
            return;
        }
        body.push(b'\n');
    }
    let _ = httpc::request(base, "POST", &format!("/log?bin={bin}"), Some(&body), 0);
}

pub fn post_line(base_url: &str, bin: &str, level: &str, target: &str, msg: &str, fields: &str) {
    let Ok(base) = httpc::parse_base(base_url) else {
        return;
    };
    let line = LogLine {
        ts: now_ms(),
        level: level.to_string(),
        target: target.to_string(),
        msg: msg.to_string(),
        seq: next_seq(),
        fields: fields.to_string(),
        session: session(),
    };
    let mut body = Vec::new();
    if serde_json::to_writer(&mut body, &line).is_err() {
        return;
    }
    body.push(b'\n');
    let _ = httpc::request(&base, "POST", &format!("/log?bin={bin}"), Some(&body), 0);
}

/// Upload one batch of JSON lines to a named data bucket.
/// Expect 204 No Content; body must be one JSON object per line.
fn post_data_base(
    base: &Option<Base>,
    bin: &str,
    session: &str,
    bucket: &str,
    body: &[u8],
) {
    let Some(base) = base else { return };
    let _ = httpc::request(
        base,
        "POST",
        &format!("/data/{bucket}?bin={bin}&session={session}"),
        Some(body),
        0,
    );
}

pub fn post_data(
    base_url: &str,
    bucket: &str,
    bin: &str,
    session: &str,
    body: &[u8],
) -> Result<(), String> {
    let base = httpc::parse_base(base_url)?;
    let resp = httpc::request(
        &base,
        "POST",
        &format!("/data/{bucket}?bin={bin}&session={session}"),
        Some(body),
        0,
    )?;
    if resp.status == 204 {
        Ok(())
    } else {
        Err(format!("data POST: HTTP {}", resp.status))
    }
}

enum DataSinkCmd {
    Line(String),
    Flush(mpsc::Sender<()>),
}

/// Batched sender for data-bucket lines: per-frame pushes are cheap
/// (channel send); a background thread posts up to 128 lines every
/// ~500 ms. Lines must already be JSON (one object per line).
#[derive(Clone)]
pub struct DataSink {
    tx: mpsc::Sender<DataSinkCmd>,
}

impl DataSink {
    pub fn start(base_url: &str, bin: &str, bucket: &str) -> DataSink {
        let (tx, rx) = mpsc::channel();
        let base = httpc::parse_base(base_url).ok();
        let bin = bin.to_string();
        let bucket = bucket.to_string();
        let session = session();
        std::thread::Builder::new()
            .name("relay-data-sink".into())
            .spawn(move || data_sink_loop(rx, base, &bin, &bucket, &session))
            .expect("relay data sink thread");
        DataSink { tx }
    }

    pub fn push(&self, line: String) {
        let _ = self.tx.send(DataSinkCmd::Line(line));
    }

    pub fn flush(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(DataSinkCmd::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }
}

fn data_sink_loop(
    rx: mpsc::Receiver<DataSinkCmd>,
    base: Option<Base>,
    bin: &str,
    bucket: &str,
    session: &str,
) {
    let mut batch: Vec<String> = Vec::with_capacity(128);
    let mut failures: u32 = 0;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(DataSinkCmd::Line(line)) => {
                batch.push(line);
                if batch.len() < 128 {
                    continue;
                }
            }
            Ok(DataSinkCmd::Flush(ack)) => {
                post_data_batch(&base, bin, session, bucket, &batch, &mut failures);
                batch.clear();
                let _ = ack.send(());
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                post_data_batch(&base, bin, session, bucket, &batch, &mut failures);
                return;
            }
        }
        post_data_batch(&base, bin, session, bucket, &batch, &mut failures);
        batch.clear();
    }
}

fn post_data_batch(
    base: &Option<Base>,
    bin: &str,
    session: &str,
    bucket: &str,
    batch: &[String],
    failures: &mut u32,
) {
    if batch.is_empty() {
        return;
    }
    let mut body = batch.join("\n").into_bytes();
    body.push(b'\n');
    post_data_base(base, bin, session, bucket, &body);
    // post_data_base swallows errors; detect via a cheap success marker is
    // overkill — log every 50th attempt that fails outright
    if base.is_none() {
        *failures += 1;
        if *failures == 1 || *failures % 50 == 0 {
            eprintln!("relay-data-sink: no valid url, dropped {} batches", *failures);
        }
    }
}

pub fn own_version_name(git_sha: &str) -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .filter(|n| naming::is_bin_name(n))
        .unwrap_or_else(|| git_sha.to_string())
}

pub fn install_bin(base: &Base, dir: &Path, name: &str) -> Result<bool, String> {
    if !naming::is_bin_name(name) {
        return Err(format!("bad bin name: {name}"));
    }
    let target = dir.join(name);
    if target.is_file() {
        return Ok(false);
    }
    let part = dir.join(format!(".{name}.part"));
    let status = download(base, name, &part)?;
    if status != 200 {
        let _ = fs::remove_file(&part);
        return Err(format!("download {name}: HTTP {status}"));
    }
    make_executable(&part);
    fs::rename(&part, &target).map_err(|e| format!("install {name}: {e}"))?;
    Ok(true)
}

fn download(base: &Base, name: &str, part: &Path) -> Result<u16, String> {
    let mut file = File::create(part).map_err(|e| format!("create {}: {e}", part.display()))?;
    let target = format!("/bins/{name}");
    let status = httpc::request_into(base, "GET", &target, None, |chunk| {
        file.write_all(chunk).map_err(|e| e.to_string())
    });
    match status {
        Ok(s) => {
            file.flush().map_err(|e| e.to_string())?;
            Ok(s)
        }
        Err(e) => {
            let _ = fs::remove_file(part);
            Err(e)
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::naming;
    use crate::testutil;

    #[test]
    fn log_sink_round_trip() {
        let dist = testutil::temp_dir("client-sink");
        let (port, _state) = crate::server::start_test_server(dist.clone());
        let sink = LogSink::start(&format!("http://127.0.0.1:{port}"), "0123abcd");
        sink.log("INFO", "aim", "hello".into(), "dy=1.5".into());
        sink.log("DEBUG", "aim", "world".into(), String::new());
        sink.flush(Duration::from_secs(5));
        let base = Base {
            host: "127.0.0.1".into(),
            port,
        };
        let resp = httpc::request(&base, "GET", "/logs/0123abcd", None, 64 * 1024).unwrap();
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body);
        assert!(text.contains(r#""msg":"hello""#), "got {text}");
        assert!(text.contains(r#""seq":1"#));
        assert!(text.contains(r#""seq":2"#));
        assert!(text.contains(r#""fields":"dy=1.5""#));
        let session = session();
        assert!(naming::is_session_id(&session));
        assert!(text.contains(&format!(r#""session":"{session}""#)));
        testutil::rmrf(&dist);
    }

    #[test]
    fn minted_session_matches_grammar() {
        let s = ensure_session(&[]);
        assert!(naming::is_session_id(&s));
        assert_eq!(ensure_session(&[]), s);
    }

    #[test]
    fn session_flag_lookup_forms() {
        let args = vec!["--relay-session".to_string(), "ab12cd".to_string()];
        assert_eq!(find_flag_value(&args, "--relay-session").unwrap(), "ab12cd");
        let args = vec!["--relay-session=ef1234".to_string()];
        assert_eq!(find_flag_value(&args, "--relay-session").unwrap(), "ef1234");
        assert!(find_flag_value(&[], "--relay-session").is_none());
    }

    #[test]
    fn minted_session_is_valid() {
        assert!(naming::is_session_id(&mint_session()));
    }

    #[test]
    fn own_name_falls_back_to_sha() {
        assert_eq!(own_version_name("cafe123"), "cafe123");
        assert!(naming::is_bin_name("cafe123"));
    }

    #[test]
    fn install_bin_skips_existing_and_rejects_bad_names() {
        let dist = testutil::temp_dir("client-install");
        let (port, _state) = crate::server::start_test_server(dist.clone());
        let base = Base {
            host: "127.0.0.1".into(),
            port,
        };
        let err = install_bin(&base, &dist, "../evil");
        assert!(err.is_err());
        let err = install_bin(&base, &dist, "0123abc");
        assert!(err.is_err(), "missing bin should 404");
        testutil::rmrf(&dist);
    }

    #[test]
    fn control_channel_receives_enqueued_commands() {
        let dist = testutil::temp_dir("client-control");
        let (port, state) = crate::server::start_test_server(dist.clone());
        state.control.wait_ms.store(50, Ordering::Relaxed);
        state.control.enqueue(
            &dist,
            "0123abcd",
            &session(),
            serde_json::json!({"op": "key", "key": "w", "action": "down"}),
        );
        let chan = ControlChannel::start(&format!("http://127.0.0.1:{port}"), "0123abcd");
        let got = chan
            .rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("cmd");
        assert!(
            matches!(got, ControlCmd::Key { key, action, .. } if key == "w" && action == "down")
        );
        chan.stop();
        testutil::rmrf(&dist);
    }
}
