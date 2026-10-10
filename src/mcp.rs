use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::manifest;
use crate::naming;
use crate::server::{ServerState, respond_bytes};

const SHOT_WAIT: Duration = Duration::from_secs(5);
const INLINE_SHOT_MAX: u64 = 1_572_864;
const MAX_EVENTS: usize = 1000;

pub fn handle(stream: &mut TcpStream, state: &ServerState, body: &[u8]) {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        let _ = rpc_error(stream, &Value::Null, -32700, "parse error");
        return;
    };
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(id) = req.get("id").cloned() else {
        let _ = respond_bytes(stream, 202, "Accepted", "text/plain", b"", &[]);
        return;
    };
    match method.as_str() {
        "initialize" => {
            let proto = match req
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
            {
                Some(p) if matches!(p, "2024-11-05" | "2025-03-26" | "2025-06-18") => p,
                _ => "2025-03-26",
            };
            let result = json!({
                "protocolVersion": proto,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "dev-relay", "version": env!("CARGO_PKG_VERSION")},
            });
            let session = new_mcp_session();
            let _ = rpc_ok(stream, &id, result, &[("Mcp-Session-Id", session.as_str())]);
        }
        "ping" => {
            let _ = rpc_ok(stream, &id, json!({}), &[]);
        }
        "tools/list" => {
            let _ = rpc_ok(stream, &id, json!({"tools": tools_spec()}), &[]);
        }
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let known = [
                "list_bins",
                "sessions",
                "tail_logs",
                "session_events",
                "send_input",
                "send_input_mouse",
                "click",
                "screenshot",
                "depth",
                "recent_shots",
                "data_buckets",
                "get_data",
                "client_log_level",
            ];
            if !known.contains(&name) {
                let _ = rpc_error(stream, &id, -32602, &format!("unknown tool: {name}"));
                return;
            }
            let outcome = match name {
                "list_bins" => list_bins(state),
                "sessions" => sessions_tool(state, &args),
                "tail_logs" => tail_logs(state, &args),
                "session_events" => session_events(state, &args),
                "send_input" => send_key(state, &args),
                "send_input_mouse" => send_mouse(state, &args),
                "click" => send_click(state, &args),
                "screenshot" => screenshot(state, &args, false),
                "depth" => screenshot(state, &args, true),
                "data_buckets" => data_buckets(state),
                "get_data" => get_data(state, &args),
                "client_log_level" => client_log_level(state, &args),
                _ => recent_shots(state, &args),
            };
            let result = match outcome {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": v.to_string()}],
                    "structuredContent": v,
                }),
                Err(msg) => json!({
                    "content": [{"type": "text", "text": msg}],
                    "isError": true,
                }),
            };
            let _ = rpc_ok(stream, &id, result, &[]);
        }
        other => {
            let _ = rpc_error(stream, &id, -32601, &format!("unknown method: {other}"));
        }
    }
}

fn rpc_ok(
    stream: &mut TcpStream,
    id: &Value,
    result: Value,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let body = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    respond_bytes(stream, 200, "OK", "application/json", &bytes, extra)
}

fn rpc_error(stream: &mut TcpStream, id: &Value, code: i64, msg: &str) -> std::io::Result<()> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": msg},
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    respond_bytes(stream, 200, "OK", "application/json", &bytes, &[])
}

fn new_mcp_session() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    format!("mcp{ms:x}{n:04x}")
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn resolve_bin(state: &ServerState, args: &Value) -> Result<String, String> {
    if let Some(b) = opt_str(args, "bin").filter(|b| naming::is_bin_name(b)) {
        return Ok(b.to_string());
    }
    latest_bin(state)
}

fn latest_bin(state: &ServerState) -> Result<String, String> {
    let m =
        manifest::scan_bins(&state.dist.join("bins")).map_err(|e| format!("manifest scan: {e}"))?;
    m.latest
        .get("")
        .or_else(|| m.latest.get("exe"))
        .cloned()
        .ok_or_else(|| "no bins published".to_string())
}

fn resolve_live_session(state: &ServerState, args: &Value, bin: &str) -> Result<String, String> {
    if let Some(s) = opt_str(args, "session").filter(|s| naming::is_session_id(s)) {
        return Ok(s.to_string());
    }
    state
        .control
        .active_session(bin)
        .ok_or_else(|| "no active session (game not polling /control)".to_string())
}

fn tools_spec() -> Value {
    let live_props = |extra: Value| {
        let mut p = extra;
        p["bin"] = json!({"type": "string"});
        p["session"] = json!({"type": "string"});
        p
    };
    vec![
        tool(
            "list_bins",
            "Manifest snapshot: latest bin per platform suffix plus all published bins.",
            json!({}),
            &[],
        ),
        tool(
            "sessions",
            "Session buckets with activity: first/last ts, log line, input, and shot counts.",
            json!({"bin": {"type": "string"}}),
            &[],
        ),
        tool(
            "tail_logs",
            "Newest matching JSONL log lines of a bin; omit session to span all runs.",
            json!({
                "bin": {"type": "string"},
                "session": {"type": "string"},
                "max_lines": {"type": "integer"},
                "level_min": {"type": "string", "enum": ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]},
                "contains": {"type": "string"},
            }),
            &[],
        ),
        tool(
            "session_events",
            "One session's bucket: log lines, enqueued input commands, and captures merged into a ts-sorted stream.",
            json!({
                "bin": {"type": "string"},
                "session": {"type": "string"},
                "since_ts": {"type": "integer"},
            }),
            &[],
        ),
        tool(
            "send_input",
            "Enqueue a key press/release on the live session's control channel.",
            live_props(json!({
                "key": {"type": "string"},
                "action": {"type": "string", "enum": ["down", "up"]},
            })),
            &["key", "action"],
        ),
        tool(
            "send_input_mouse",
            "Enqueue relative mouse motion (and optional wheel) on the live session's control channel.",
            live_props(json!({
                "dx": {"type": "number"},
                "dy": {"type": "number"},
                "wheel": {"type": "object", "properties": {"dx": {"type": "number"}, "dy": {"type": "number"}}},
            })),
            &["dx", "dy"],
        ),
        tool(
            "click",
            "Enqueue a mouse button press/release on the live session's control channel.",
            live_props(json!({
                "button": {"type": "string", "enum": ["left", "right", "middle"]},
                "action": {"type": "string", "enum": ["press", "release"]},
            })),
            &["button", "action"],
        ),
        tool(
            "screenshot",
            "Request a color capture from the live session and wait for the PNG.",
            live_props(json!({})),
            &[],
        ),
        tool(
            "depth",
            "Request a depth capture from the live session and wait for the raw buffer.",
            live_props(json!({})),
            &[],
        ),
        tool(
            "recent_shots",
            "Newest captures with sizes and timestamps.",
            json!({
                "bin": {"type": "string"},
                "session": {"type": "string"},
                "max": {"type": "integer"},
            }),
            &[],
        ),
        tool(
            "data_buckets",
            "List named data buckets (client exports, e.g. per-frame ball positions) with per-session file stats.",
            json!({}),
            &[],
        ),
        tool(
            "client_log_level",
            "Read or set the RUST_LOG-style filter for freshly spawned clients, e.g. 'debug' or 'debug,wgpu=warn'. The launcher applies it by restarting the client. Call without arguments to read the current value; empty string resets to default.",
            json!({
                "level": {"type": "string", "description": "RUST_LOG-style filter; omit to read"},
            }),
            &[],
        ),
        tool(
            "get_data",
            "Query a data bucket: newest-last JSONL records with optional sampling/filtering. Defaults to the last 1000 records.",
            json!({
                "bucket": {"type": "string"},
                "bin": {"type": "string"},
                "session": {"type": "string"},
                "last": {"type": "integer", "description": "keep only the newest N records"},
                "sample": {"type": "integer", "description": "keep every Nth record"},
                "from": {"type": "integer", "description": "unix ms lower bound on record t"},
                "to": {"type": "integer", "description": "unix ms upper bound on record t"},
            }),
            &["bucket"],
        ),
    ]
    .into()
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    let mut schema = json!({"type": "object", "properties": props});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({"name": name, "description": desc, "inputSchema": schema})
}

fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" => 3,
        "ERROR" => 4,
        _ => 255,
    }
}

fn client_log_level(state: &ServerState, args: &Value) -> Result<Value, String> {
    let current = state.client_log_level.lock().unwrap().clone();
    let level = opt_str(args, "level");
    match level {
        Some(l) if l.is_empty() => {
            // empty string resets to the client default
            *state.client_log_level.lock().unwrap() = None;
            Ok(json!({"level": null, "note": "reset to client default"}))
        }
        Some(l) => {
            if l.len() > 200 || !l.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
                return Err("level must be 1-200 printable ASCII chars".into());
            }
            *state.client_log_level.lock().unwrap() = Some(l.to_string());
            Ok(json!({"level": l, "note": "the launcher restarts the client with RUST_LOG set to this on its next poll"}))
        }
        None => Ok(json!({"level": current})),
    }
}

fn data_buckets(state: &ServerState) -> Result<Value, String> {
    serde_json::to_value(crate::data::buckets(&state.dist)).map_err(|e| e.to_string())
}

fn get_data(state: &ServerState, args: &Value) -> Result<Value, String> {
    let bucket = opt_str(args, "bucket")
        .filter(|b| crate::data::valid_bucket(b))
        .ok_or("missing or invalid bucket")?;
    let as_usize = |args: &Value, key: &str| -> Option<usize> {
        args.get(key).and_then(Value::as_u64).map(|v| v as usize)
    };
    let opts = crate::data::QueryOpts {
        bin: opt_str(args, "bin").filter(|b| naming::is_bin_name(b)).map(String::from),
        session: opt_str(args, "session")
            .filter(|s| naming::is_session_id(s))
            .map(String::from),
        sample: as_usize(args, "sample").unwrap_or(1).max(1),
        last: match as_usize(args, "last") {
            Some(n) => Some(n),
            None => Some(1000),
        },
        from: args.get("from").and_then(Value::as_u64),
        to: args.get("to").and_then(Value::as_u64),
    };
    let text = crate::data::query(&state.dist, bucket, &opts)?;
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or(Value::String(l.to_string())))
        .collect();
    Ok(json!({"bucket": bucket, "count": lines.len(), "records": lines}))
}

fn list_bins(state: &ServerState) -> Result<Value, String> {
    let m =
        manifest::scan_bins(&state.dist.join("bins")).map_err(|e| format!("manifest scan: {e}"))?;
    serde_json::to_value(m).map_err(|e| e.to_string())
}

#[derive(Default)]
struct SessionAcc {
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    log_lines: u64,
    inputs: u64,
    shots: u64,
}

impl SessionAcc {
    fn note_ts(&mut self, ts: u64) {
        self.first_ts = Some(self.first_ts.map_or(ts, |t| t.min(ts)));
        self.last_ts = Some(self.last_ts.map_or(ts, |t| t.max(ts)));
    }
}

fn scan_jsonl_sessions(
    dist: &Path,
    dir: &str,
    bin_filter: Option<&str>,
    acc: &mut HashMap<(String, String), SessionAcc>,
    logs: bool,
) {
    let Ok(entries) = std::fs::read_dir(dist.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(bin) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !naming::is_bin_name(bin) || bin_filter.is_some_and(|f| f != bin) {
            continue;
        }
        for v in jsonl_values(&entry.path()) {
            let (Some(session), Some(ts)) = (
                v.get("session").and_then(Value::as_str),
                v.get("ts").and_then(Value::as_u64),
            ) else {
                continue;
            };
            let e = acc
                .entry((bin.to_string(), session.to_string()))
                .or_default();
            e.note_ts(ts);
            if logs {
                e.log_lines += 1;
            } else {
                e.inputs += 1;
            }
        }
    }
}

fn sessions_tool(state: &ServerState, args: &Value) -> Result<Value, String> {
    let bin_filter = opt_str(args, "bin").filter(|b| naming::is_bin_name(b));
    let mut acc: HashMap<(String, String), SessionAcc> = HashMap::new();
    scan_jsonl_sessions(&state.dist, "logs", bin_filter, &mut acc, true);
    scan_jsonl_sessions(&state.dist, "inputs", bin_filter, &mut acc, false);
    let Ok(bin_dirs) = std::fs::read_dir(state.dist.join("shots")) else {
        return finish_sessions(acc);
    };
    for bin_dir in bin_dirs.flatten() {
        let bin = bin_dir.file_name().to_string_lossy().into_owned();
        if !naming::is_bin_name(&bin) || bin_filter.is_some_and(|f| f != bin) {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(bin_dir.path()) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            let session = session_dir.file_name().to_string_lossy().into_owned();
            if !naming::is_session_id(&session) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(session_dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if shot_file_parts(&fname).is_none() {
                    continue;
                }
                let e = acc.entry((bin.clone(), session.clone())).or_default();
                e.note_ts(mtime_ms(&f.path()));
                e.shots += 1;
            }
        }
    }
    finish_sessions(acc)
}

fn finish_sessions(acc: HashMap<(String, String), SessionAcc>) -> Result<Value, String> {
    let mut rows: Vec<Value> = acc
        .into_iter()
        .map(|((bin, session), a)| {
            json!({
                "bin": bin,
                "session": session,
                "first_ts": a.first_ts.unwrap_or(0),
                "last_ts": a.last_ts.unwrap_or(0),
                "log_lines": a.log_lines,
                "inputs": a.inputs,
                "shots": a.shots,
            })
        })
        .collect();
    rows.sort_by_key(|r| r["last_ts"].as_u64().unwrap_or(0));
    rows.reverse();
    Ok(json!({"sessions": rows}))
}

fn jsonl_values(path: &Path) -> Vec<Value> {
    std::fs::File::open(path)
        .map(|f| {
            std::io::BufRead::lines(std::io::BufReader::new(f))
                .map_while(Result::ok)
                .filter_map(|l| serde_json::from_str(&l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn tail_logs(state: &ServerState, args: &Value) -> Result<Value, String> {
    let bin = resolve_bin(state, args)?;
    let session = opt_str(args, "session").filter(|s| naming::is_session_id(s));
    let max = args
        .get("max_lines")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .clamp(1, 5000) as usize;
    let level_min = opt_str(args, "level_min").map(level_rank);
    let contains = opt_str(args, "contains");
    let mut lines = jsonl_values(&log_path(state, &bin));
    lines.retain(|l| {
        session.is_none_or(|s| l.get("session").and_then(Value::as_str) == Some(s))
            && level_min.is_none_or(|m| {
                l.get("level")
                    .and_then(Value::as_str)
                    .is_none_or(|lv| level_rank(lv) >= m)
            })
            && contains.is_none_or(|c| {
                l.get("msg")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.contains(c))
            })
    });
    let start = lines.len().saturating_sub(max);
    let lines = lines.split_off(start);
    Ok(json!({"bin": bin, "lines": lines}))
}

fn log_path(state: &ServerState, bin: &str) -> PathBuf {
    state.dist.join("logs").join(format!("{bin}.jsonl"))
}

fn input_path(state: &ServerState, bin: &str) -> PathBuf {
    state.dist.join("inputs").join(format!("{bin}.jsonl"))
}

fn newest_log_session(state: &ServerState, bin: &str) -> Result<String, String> {
    let mut best: Option<(u64, String)> = None;
    for l in jsonl_values(&log_path(state, bin)) {
        let (Some(session), Some(ts)) = (
            l.get("session").and_then(Value::as_str),
            l.get("ts").and_then(Value::as_u64),
        ) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bts, _)| ts > *bts) {
            best = Some((ts, session.to_string()));
        }
    }
    best.map(|(_, s)| s)
        .ok_or_else(|| format!("no sessions logged for {bin}"))
}

fn session_events(state: &ServerState, args: &Value) -> Result<Value, String> {
    let bin = resolve_bin(state, args)?;
    let session = match opt_str(args, "session").filter(|s| naming::is_session_id(s)) {
        Some(s) => s.to_string(),
        None => newest_log_session(state, &bin)?,
    };
    let since = args.get("since_ts").and_then(Value::as_u64);
    let mut events: Vec<(u64, Value)> = Vec::new();
    for l in jsonl_values(&log_path(state, &bin)) {
        if l.get("session").and_then(Value::as_str) != Some(session.as_str()) {
            continue;
        }
        let ts = l.get("ts").and_then(Value::as_u64).unwrap_or(0);
        if since.is_some_and(|s| ts < s) {
            continue;
        }
        let mut e = l.clone();
        e["kind"] = json!("log");
        events.push((ts, e));
    }
    for r in jsonl_values(&input_path(state, &bin)) {
        if r.get("session").and_then(Value::as_str) != Some(session.as_str()) {
            continue;
        }
        let ts = r.get("ts").and_then(Value::as_u64).unwrap_or(0);
        if since.is_some_and(|s| ts < s) {
            continue;
        }
        events.push((
            ts,
            json!({
                "kind": "input",
                "ts": ts,
                "id": r.get("id").cloned().unwrap_or(Value::Null),
                "cmd": r.get("cmd").cloned().unwrap_or(Value::Null),
            }),
        ));
    }
    let shots_dir = state.dist.join("shots").join(&bin).join(&session);
    if let Ok(files) = std::fs::read_dir(&shots_dir) {
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            let Some((seq, kind)) = shot_file_parts(&fname) else {
                continue;
            };
            let ts = mtime_ms(&f.path());
            if since.is_some_and(|s| ts < s) {
                continue;
            }
            events.push((
                ts,
                json!({
                    "kind": "shot",
                    "ts": ts,
                    "seq": seq,
                    "shot_kind": kind,
                    "size": f.metadata().map_or(0, |m| m.len()),
                }),
            ));
        }
    }
    events.sort_by_key(|(ts, _)| *ts);
    if events.len() > MAX_EVENTS {
        events = events.split_off(events.len() - MAX_EVENTS);
    }
    let events: Vec<Value> = events.into_iter().map(|(_, e)| e).collect();
    Ok(json!({"bin": bin, "session": session, "events": events}))
}

fn send_key(state: &ServerState, args: &Value) -> Result<Value, String> {
    let key = opt_str(args, "key")
        .filter(|k| !k.is_empty() && k.len() <= 16)
        .ok_or("key required")?;
    let action = match opt_str(args, "action") {
        Some("down") => "down",
        Some("up") => "up",
        _ => return Err("action must be down|up".into()),
    };
    let bin = resolve_bin(state, args)?;
    let session = resolve_live_session(state, args, &bin)?;
    let id = state.control.enqueue(
        &state.dist,
        &bin,
        &session,
        json!({"op": "key", "key": key, "action": action}),
    );
    Ok(json!({"session": session, "queued": 1, "ids": [id]}))
}

fn send_mouse(state: &ServerState, args: &Value) -> Result<Value, String> {
    let dx = args
        .get("dx")
        .and_then(Value::as_f64)
        .ok_or("dx required")?;
    let dy = args
        .get("dy")
        .and_then(Value::as_f64)
        .ok_or("dy required")?;
    let wheel = args.get("wheel").filter(|w| w.is_object());
    let bin = resolve_bin(state, args)?;
    let session = resolve_live_session(state, args, &bin)?;
    let mut ids = vec![state.control.enqueue(
        &state.dist,
        &bin,
        &session,
        json!({"op": "mouse_move", "dx": dx, "dy": dy}),
    )];
    if let Some(w) = wheel {
        ids.push(state.control.enqueue(
            &state.dist,
            &bin,
            &session,
            json!({
                "op": "wheel",
                "dx": w.get("dx").and_then(Value::as_f64).unwrap_or(0.0),
                "dy": w.get("dy").and_then(Value::as_f64).unwrap_or(0.0),
            }),
        ));
    }
    Ok(json!({"session": session, "queued": ids.len(), "ids": ids}))
}

fn send_click(state: &ServerState, args: &Value) -> Result<Value, String> {
    let button = match opt_str(args, "button") {
        Some(b @ ("left" | "right" | "middle")) => b,
        _ => return Err("button must be left|right|middle".into()),
    };
    let action = match opt_str(args, "action") {
        Some(a @ ("press" | "release")) => a,
        _ => return Err("action must be press|release".into()),
    };
    let bin = resolve_bin(state, args)?;
    let session = resolve_live_session(state, args, &bin)?;
    let id = state.control.enqueue(
        &state.dist,
        &bin,
        &session,
        json!({"op": "mouse_button", "button": button, "action": action}),
    );
    Ok(json!({"session": session, "queued": 1, "ids": [id]}))
}

fn screenshot(state: &ServerState, args: &Value, depth: bool) -> Result<Value, String> {
    let bin = resolve_bin(state, args)?;
    let session = resolve_live_session(state, args, &bin)?;
    let op = if depth { "depth" } else { "screenshot" };
    state
        .control
        .enqueue(&state.dist, &bin, &session, json!({"op": op}));
    let kind = if depth { "depth" } else { "color" };
    let dir = state.dist.join("shots").join(&bin).join(&session);
    let start_ts = now_ms();
    let deadline = Instant::now() + SHOT_WAIT;
    let found = loop {
        if let Some(found) = newest_shot(&dir, kind, start_ts) {
            break Some(found);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let Some((ts, seq, size)) = found else {
        return Err(format!(
            "no {kind} capture arrived within {}s",
            SHOT_WAIT.as_secs()
        ));
    };
    let public = state.public_base.lock().unwrap().clone();
    if depth {
        let ext = "depthbin";
        let url = format!("{public}/shots/{bin}/{session}/{seq}.{ext}");
        let meta = std::fs::read(dir.join(format!("{seq}.depth.json")))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or_else(|| {
                json!({"width": 0, "height": 0, "near": 0.0, "far": 0.0, "projection": "unknown"})
            });
        Ok(json!({
            "session": session,
            "seq": seq,
            "url": url,
            "width": meta["width"],
            "height": meta["height"],
            "near": meta["near"],
            "far": meta["far"],
            "projection": meta["projection"],
        }))
    } else {
        let url = format!("{public}/shots/{bin}/{session}/{seq}.png");
        let bytes = std::fs::read(dir.join(format!("{seq}.png"))).unwrap_or_default();
        let (w, h) = png_dims(&bytes).unwrap_or((0, 0));
        let mut out = json!({
            "session": session,
            "seq": seq,
            "url": url,
            "width": w,
            "height": h,
            "ts": ts,
            "size": size,
        });
        if size < INLINE_SHOT_MAX {
            out["base64"] = json!(base64(&bytes));
        }
        Ok(out)
    }
}

fn recent_shots(state: &ServerState, args: &Value) -> Result<Value, String> {
    let bin = resolve_bin(state, args)?;
    let session = opt_str(args, "session").filter(|s| naming::is_session_id(s));
    let max = args
        .get("max")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100) as usize;
    let bins_root = state.dist.join("shots").join(&bin);
    let session_dirs: Vec<PathBuf> = match session {
        Some(s) => vec![bins_root.join(s)],
        None => std::fs::read_dir(&bins_root)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default(),
    };
    let mut shots: Vec<(u64, Value)> = Vec::new();
    for sdir in session_dirs {
        let Some(sname) = sdir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !naming::is_session_id(sname) {
            continue;
        }
        for f in std::fs::read_dir(&sdir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            let fname = f.file_name().to_string_lossy().into_owned();
            let Some((seq, kind)) = shot_file_parts(&fname) else {
                continue;
            };
            let ts = mtime_ms(&f.path());
            let size = f.metadata().map_or(0, |m| m.len());
            shots.push((
                ts,
                json!({"session": sname, "seq": seq, "kind": kind, "size": size, "ts": ts}),
            ));
        }
    }
    shots.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    shots.truncate(max);
    let shots: Vec<Value> = shots.into_iter().map(|(_, v)| v).collect();
    Ok(json!({"bin": bin, "shots": shots}))
}

fn shot_file_parts(name: &str) -> Option<(u64, &'static str)> {
    if let Some(stem) = name.strip_suffix(".png") {
        return stem.parse().ok().map(|seq| (seq, "color"));
    }
    if let Some(stem) = name.strip_suffix(".depthbin") {
        return stem.parse().ok().map(|seq| (seq, "depth"));
    }
    None
}

fn newest_shot(dir: &Path, kind: &str, since_ts: u64) -> Option<(u64, u64, u64)> {
    let ext = match kind {
        "depth" => ".depthbin",
        _ => ".png",
    };
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, u64, u64)> = None;
    for f in entries.flatten() {
        let fname = f.file_name().to_string_lossy().into_owned();
        let Some(stem) = fname.strip_suffix(ext) else {
            continue;
        };
        let Ok(seq) = stem.parse::<u64>() else {
            continue;
        };
        let ts = mtime_ms(&f.path());
        if ts < since_ts {
            continue;
        }
        let size = f.metadata().map_or(0, |m| m.len());
        if best.as_ref().is_none_or(|(bts, ..)| ts >= *bts) {
            best = Some((ts, seq, size));
        }
    }
    best
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn png_dims(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 24 || data[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return None;
    }
    if &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w as u64, h as u64))
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::start_test_server;
    use crate::testutil;
    use std::io::{Read, Write};

    const PNG_1X1: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
    ];

    fn raw_http_full(port: u16, method: &str, target: &str, body: &[u8]) -> (u16, Vec<u8>) {
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
        (status, resp)
    }

    fn body_of(resp: &[u8]) -> Vec<u8> {
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        resp[split..].to_vec()
    }

    fn raw_http(port: u16, method: &str, target: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let (status, resp) = raw_http_full(port, method, target, body);
        (status, body_of(&resp))
    }

    fn mcp_call(port: u16, body: &Value) -> (u16, Option<String>, Value) {
        let raw = serde_json::to_vec(body).unwrap();
        let (status, resp) = raw_http_full(port, "POST", "/mcp", &raw);
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&resp[..split]).to_string();
        let sid = headers
            .lines()
            .find_map(|l| l.strip_prefix("Mcp-Session-Id: ").map(|v| v.to_string()));
        let v = serde_json::from_slice(&resp[split + 4..]).unwrap_or(Value::Null);
        (status, sid, v)
    }

    fn call_tool(port: u16, name: &str, args: Value) -> Value {
        let (status, _, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
        );
        assert_eq!(status, 200, "tool {name} failed: {v}");
        assert!(v["result"]["isError"].is_null(), "tool {name} errored: {v}");
        v["result"]["structuredContent"].clone()
    }

    fn publish_bin(dist: &Path, name: &str) {
        std::fs::write(dist.join("bins").join(name), b"bin-bytes").unwrap();
        manifest::refresh(dist).unwrap();
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn png_dims_parses_header() {
        assert_eq!(png_dims(PNG_1X1), Some((1, 1)));
        assert_eq!(png_dims(b"not-a-png"), None);
    }

    #[test]
    fn mcp_handshake_and_discovery() {
        let dist = testutil::temp_dir("mcp-handshake");
        let (port, _state) = start_test_server(dist.clone());

        let (status, sid, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t"}}}),
        );
        assert_eq!(status, 200);
        assert!(sid.is_some());
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert!(v["result"]["capabilities"]["tools"].is_object());

        let (status, _, _) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert_eq!(status, 202);

        let (status, _, v) = mcp_call(port, &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
        assert_eq!(status, 200);
        assert_eq!(v["result"], json!({}));

        let (_, _, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        );
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "list_bins",
            "sessions",
            "tail_logs",
            "session_events",
            "send_input",
            "send_input_mouse",
            "click",
            "screenshot",
            "depth",
            "recent_shots",
        ] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }

        let (_, _, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "no/such"}),
        );
        assert_eq!(v["error"]["code"], -32601);

        let (status, _, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}}),
        );
        assert_eq!(status, 200);
        assert_eq!(v["error"]["code"], -32602);

        let (status, _) = raw_http(port, "POST", "/mcp", b"{broken");
        assert_eq!(status, 200);

        testutil::rmrf(&dist);
    }

    #[test]
    fn tail_logs_sessions_and_events() {
        let dist = testutil::temp_dir("mcp-logs");
        let (port, _state) = start_test_server(dist.clone());
        publish_bin(&dist, "0123abcd");

        let log = concat!(
            r#"{"ts":100,"level":"DEBUG","target":"aim","msg":"pull dy=1","seq":1,"session":"aaaaaa1"}"#,
            "\n",
            r#"{"ts":200,"level":"INFO","target":"aim","msg":"strike fired","seq":2,"session":"aaaaaa1"}"#,
            "\n",
            r#"{"ts":300,"level":"ERROR","target":"cam","msg":"boom","seq":1,"session":"bbbbbb2"}"#,
            "\n",
        );
        std::fs::write(dist.join("logs").join("0123abcd.jsonl"), log).unwrap();

        let out = call_tool(port, "list_bins", json!({}));
        assert_eq!(out["latest"][""], "0123abcd");
        assert_eq!(out["files"][0]["name"], "0123abcd");

        let out = call_tool(port, "tail_logs", json!({}));
        assert_eq!(out["lines"].as_array().unwrap().len(), 3);

        let out = call_tool(port, "tail_logs", json!({"session": "aaaaaa1"}));
        let lines = out["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["msg"], "strike fired");

        let out = call_tool(port, "tail_logs", json!({"level_min": "ERROR"}));
        assert_eq!(out["lines"].as_array().unwrap().len(), 1);

        let out = call_tool(port, "tail_logs", json!({"contains": "strike"}));
        assert_eq!(out["lines"].as_array().unwrap().len(), 1);

        let out = call_tool(port, "tail_logs", json!({"max_lines": 1}));
        assert_eq!(out["lines"][0]["msg"], "boom");

        let out = call_tool(port, "sessions", json!({}));
        let sessions = out["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["session"], "bbbbbb2");
        assert_eq!(sessions[0]["log_lines"], 1);
        assert_eq!(sessions[1]["session"], "aaaaaa1");
        assert_eq!(sessions[1]["log_lines"], 2);
        assert_eq!(sessions[1]["first_ts"], 100);

        let out = call_tool(
            port,
            "session_events",
            json!({"session": "aaaaaa1", "since_ts": 150}),
        );
        let events = out["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "log");
        assert_eq!(events[0]["msg"], "strike fired");

        let out = call_tool(port, "session_events", json!({}));
        assert_eq!(out["session"], "bbbbbb2");
        assert_eq!(out["events"].as_array().unwrap().len(), 1);

        testutil::rmrf(&dist);
    }

    #[test]
    fn send_input_targets_active_session() {
        let dist = testutil::temp_dir("mcp-input");
        let (port, state) = start_test_server(dist.clone());
        publish_bin(&dist, "0123abcd");
        state.control.wait_ms.store(50, Ordering::Relaxed);

        let (_, _, v) = mcp_call(
            port,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "send_input", "arguments": {"key": "w", "action": "down"}}}),
        );
        assert!(v["result"]["isError"].as_bool().unwrap());

        state.control.poll("0123abcd", "ab12cd");
        let out = call_tool(port, "send_input", json!({"key": "w", "action": "down"}));
        assert_eq!(out["session"], "ab12cd");
        assert_eq!(out["queued"], 1);

        let cmds = state.control.poll("0123abcd", "ab12cd");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["op"], "key");
        assert_eq!(cmds[0]["key"], "w");
        assert_eq!(cmds[0]["action"], "down");

        let out = call_tool(
            port,
            "send_input_mouse",
            json!({"dx": 3.5, "dy": -1.0, "wheel": {"dx": 0.0, "dy": -2.0}}),
        );
        assert_eq!(out["queued"], 2);
        let cmds = state.control.poll("0123abcd", "ab12cd");
        assert_eq!(cmds[0]["op"], "mouse_move");
        assert_eq!(cmds[1]["op"], "wheel");

        let out = call_tool(port, "click", json!({"button": "left", "action": "press"}));
        assert_eq!(out["queued"], 1);

        let inputs = std::fs::read_to_string(dist.join("inputs").join("0123abcd.jsonl")).unwrap();
        assert!(inputs.contains(r#""session":"ab12cd""#));
        assert!(inputs.contains(r#""op":"key""#));

        testutil::rmrf(&dist);
    }

    #[test]
    fn screenshot_and_depth_round_trip() {
        let dist = testutil::temp_dir("mcp-shots");
        let (port, state) = start_test_server(dist.clone());
        publish_bin(&dist, "0123abcd");
        state.control.wait_ms.store(50, Ordering::Relaxed);
        state.control.poll("0123abcd", "ab12cd");

        let shot_port = port;
        let t = std::thread::spawn(move || call_tool(shot_port, "screenshot", json!({})));
        std::thread::sleep(Duration::from_millis(200));
        let (code, _) = raw_http(
            port,
            "POST",
            "/shot?bin=0123abcd&session=ab12cd&kind=color&seq=7",
            PNG_1X1,
        );
        assert_eq!(code, 204);
        let out = t.join().unwrap();
        assert_eq!(out["session"], "ab12cd");
        assert_eq!(out["seq"], 7);
        assert_eq!(out["width"], 1);
        assert_eq!(out["height"], 1);
        assert!(
            out["url"]
                .as_str()
                .unwrap()
                .ends_with("/shots/0123abcd/ab12cd/7.png")
        );
        assert!(!out["base64"].as_str().unwrap().is_empty());

        let depth_port = port;
        let t = std::thread::spawn(move || call_tool(depth_port, "depth", json!({})));
        std::thread::sleep(Duration::from_millis(200));
        let (code, _) = raw_http(
            port,
            "POST",
            "/shot?bin=0123abcd&session=ab12cd&kind=depth&seq=8&w=4&h=2&near=0.1&far=100&proj=perspective",
            b"\0\0\x80\x3f",
        );
        assert_eq!(code, 204);
        let out = t.join().unwrap();
        assert_eq!(out["seq"], 8);
        assert_eq!(out["width"], 4);
        assert_eq!(out["height"], 2);
        assert_eq!(out["projection"], "perspective");
        assert!(out["url"].as_str().unwrap().ends_with("8.depthbin"));

        let out = call_tool(port, "recent_shots", json!({}));
        let shots = out["shots"].as_array().unwrap();
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0]["seq"], 8);
        assert_eq!(shots[0]["kind"], "depth");
        assert_eq!(shots[1]["seq"], 7);
        assert_eq!(shots[1]["kind"], "color");

        testutil::rmrf(&dist);
    }

    #[test]
    fn session_events_merges_all_three_streams() {
        let dist = testutil::temp_dir("mcp-bucket");
        let (port, state) = start_test_server(dist.clone());
        publish_bin(&dist, "0123abcd");
        state.control.wait_ms.store(50, Ordering::Relaxed);

        let log = concat!(
            r#"{"ts":100,"level":"INFO","target":"run","msg":"launch","seq":1,"session":"ab12cd"}"#,
            "\n",
        );
        std::fs::write(dist.join("logs").join("0123abcd.jsonl"), log).unwrap();
        state.control.enqueue(
            &dist,
            "0123abcd",
            "ab12cd",
            json!({"op": "key", "key": "w", "action": "down"}),
        );
        std::thread::sleep(Duration::from_millis(20));
        std::fs::create_dir_all(dist.join("shots").join("0123abcd").join("ab12cd")).unwrap();
        std::fs::write(
            dist.join("shots")
                .join("0123abcd")
                .join("ab12cd")
                .join("3.png"),
            b"png-bytes",
        )
        .unwrap();

        let out = call_tool(
            port,
            "session_events",
            json!({"bin": "0123abcd", "session": "ab12cd"}),
        );
        let events = out["events"].as_array().unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["log", "input", "shot"]);
        assert_eq!(events[1]["cmd"]["op"], "key");
        assert_eq!(events[2]["seq"], 3);

        let out = call_tool(port, "sessions", json!({"bin": "0123abcd"}));
        let sessions = out["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["log_lines"], 1);
        assert_eq!(sessions[0]["inputs"], 1);
        assert_eq!(sessions[0]["shots"], 1);

        testutil::rmrf(&dist);
    }
}
