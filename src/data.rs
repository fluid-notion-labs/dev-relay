//! Named data buckets: clients export arbitrary per-frame records (JSON
//! lines) and any consumer queries them back with sampling/filtering.
//!
//! Layout: `<dist>/data/<bucket>/<bin>.<session>.jsonl` — one line per
//! record, each line a JSON object. Query filters operate on the `"t"`
//! field (unix ms) when present.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::naming;

pub const DATA_DIR: &str = "data";

pub fn valid_bucket(name: &str) -> bool {
    naming::is_bin_name(name)
}

fn bucket_dir(dist: &Path, bucket: &str) -> PathBuf {
    dist.join(DATA_DIR).join(bucket)
}

fn file_stem(bin: &str, session: &str) -> String {
    format!("{bin}.{session}")
}

/// Append raw JSON lines (one record per line) to a bucket file.
pub fn append(
    dist: &Path,
    bucket: &str,
    bin: &str,
    session: &str,
    body: &[u8],
) -> Result<(), String> {
    if !valid_bucket(bucket) {
        return Err(format!("bad bucket: {bucket}"));
    }
    if !naming::is_bin_name(bin) {
        return Err(format!("bad bin: {bin}"));
    }
    if !naming::is_session_id(session) {
        return Err(format!("bad session: {session}"));
    }
    if body.is_empty() {
        return Err("empty body".to_string());
    }
    if body.len() > 32 * 1024 * 1024 {
        return Err("body too large".to_string());
    }
    let mut body = body.to_vec();
    if body.last() != Some(&b'\n') {
        body.push(b'\n');
    }
    let dir = bucket_dir(dist, bucket);
    fs::create_dir_all(&dir).map_err(|e| format!("data dir: {e}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{}.jsonl", file_stem(bin, session))))
        .map_err(|e| format!("data open: {e}"))?;
    file.write_all(&body).map_err(|e| format!("data write: {e}"))
}

/// One file in a bucket.
#[derive(serde::Serialize)]
pub struct DataFile {
    pub bin: String,
    pub session: String,
    pub bytes: u64,
}

/// Summary of one bucket.
#[derive(serde::Serialize)]
pub struct BucketInfo {
    pub bucket: String,
    pub bytes: u64,
    pub files: Vec<DataFile>,
}

/// List all buckets with their per-session files.
pub fn buckets(dist: &Path) -> Vec<BucketInfo> {
    let Ok(entries) = fs::read_dir(dist.join(DATA_DIR)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let bucket = entry.file_name().to_string_lossy().into_owned();
        if !valid_bucket(&bucket) {
            continue;
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        if let Ok(files_iter) = fs::read_dir(entry.path()) {
            for f in files_iter.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                let Some((bin, session)) = stem.split_once('.') else {
                    continue;
                };
                if !naming::is_bin_name(bin) || !naming::is_session_id(session) {
                    continue;
                }
                let Ok(meta) = f.metadata() else {
                    continue;
                };
                total += meta.len();
                files.push(DataFile {
                    bin: bin.to_string(),
                    session: session.to_string(),
                    bytes: meta.len(),
                });
            }
        }
        files.sort_by(|a, b| (a.bin.as_str(), a.session.as_str()).cmp(&(b.bin.as_str(), b.session.as_str())));
        out.push(BucketInfo {
            bucket,
            bytes: total,
            files,
        });
    }
    out.sort_by(|a, b| a.bucket.cmp(&b.bucket));
    out
}

/// Query options for [`query`].
#[derive(Default)]
pub struct QueryOpts {
    pub bin: Option<String>,
    pub session: Option<String>,
    /// Keep only every Nth record (1 = all).
    pub sample: usize,
    /// Keep only the newest N records (None = all).
    pub last: Option<usize>,
    /// Filter on the record's `"t"` field (unix ms), inclusive.
    pub from: Option<u64>,
    /// Filter on the record's `"t"` field (unix ms), inclusive.
    pub to: Option<u64>,
}

/// Read records back: merge matching files, sort by `"t"` (records without
/// one keep file order, last), then apply from/to/sample/last.
pub fn query(dist: &Path, bucket: &str, opts: &QueryOpts) -> Result<String, String> {
    if !valid_bucket(bucket) {
        return Err(format!("bad bucket: {bucket}"));
    }
    let dir = bucket_dir(dist, bucket);
    if !dir.is_dir() {
        return Err(format!("no such bucket: {bucket}"));
    }
    let mut records: Vec<(Option<u64>, String)> = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("data read: {e}"))?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some((bin, session)) = stem.split_once('.') else {
            continue;
        };
        if let Some(want_bin) = &opts.bin {
            if bin != want_bin.as_str() {
                continue;
            }
        }
        if let Some(want_session) = &opts.session {
            if session != want_session.as_str() {
                continue;
            }
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let ts = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v.get("t").and_then(Value::as_u64));
            records.push((ts, line.to_string()));
        }
    }
    let sample = opts.sample.max(1);
    let mut kept: Vec<String> = Vec::new();
    let mut idx = 0usize;
    for (ts, line) in &records {
        if let (Some(from), Some(t)) = (opts.from, ts) {
            if *t < from {
                continue;
            }
        }
        if let (Some(to), Some(t)) = (opts.to, ts) {
            if *t > to {
                continue;
            }
        }
        if idx % sample == 0 {
            kept.push(line.clone());
        }
        idx += 1;
    }
    if let Some(last) = opts.last {
        let skip = kept.len().saturating_sub(last);
        kept.drain(..skip);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn write_bucket(dist: &Path, bucket: &str, bin: &str, session: &str, lines: &[&str]) {
        append(
            dist,
            bucket,
            bin,
            session,
            lines.join("\n").as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn append_then_query_roundtrip() {
        let dist = testutil::temp_dir("data-bucket");
        write_bucket(
            &dist,
            "ball-positions",
            "testbin-1234",
            "abc123def456",
            &[
                r#"{"t":100,"f":1,"b":[[0,0.0,0.05,0.0]]}"#,
                r#"{"t":200,"f":2,"b":[[0,0.1,0.05,0.0]]}"#,
                r#"{"t":300,"f":3,"b":[[0,0.2,0.05,0.0]]}"#,
                r#"{"t":400,"f":4,"b":[[0,0.3,0.05,0.0]]}"#,
            ],
        );
        let opts = QueryOpts::default();
        let all = query(&dist, "ball-positions", &opts).unwrap();
        assert_eq!(all.lines().count(), 4);

        let opts = QueryOpts {
            sample: 2,
            ..Default::default()
        };
        let sampled = query(&dist, "ball-positions", &opts).unwrap();
        assert_eq!(sampled.lines().count(), 2);
        assert!(sampled.contains(r#""t":100"#));
        assert!(sampled.contains(r#""t":300"#));

        let opts = QueryOpts {
            last: Some(1),
            ..Default::default()
        };
        let last = query(&dist, "ball-positions", &opts).unwrap();
        assert!(last.contains(r#""t":400"#));

        let opts = QueryOpts {
            from: Some(200),
            to: Some(300),
            ..Default::default()
        };
        let window = query(&dist, "ball-positions", &opts).unwrap();
        assert!(window.contains(r#""t":200"#));
        assert!(window.contains(r#""t":300"#));
        assert!(!window.contains(r#""t":100"#));

        testutil::rmrf(&dist);
    }

    #[test]
    fn rejects_bad_names() {
        let dist = testutil::temp_dir("data-bad");
        assert!(append(&dist, "../evil", "testbin-1", "abc123def456", b"{}").is_err());
        assert!(append(&dist, "ball-positions", "../evil", "abc123def456", b"{}").is_err());
        assert!(append(&dist, "ball-positions", "testbin-1", "bad session!", b"{}").is_err());
        assert!(append(&dist, "ball-positions", "testbin-1", "abc123def456", b"").is_err());
        let listed = buckets(&dist);
        assert!(listed.is_empty(), "no valid buckets written");
        testutil::rmrf(&dist);
    }

    #[test]
    fn http_post_get_roundtrip() {
        let dist = testutil::temp_dir("data-http");
        let (port, _state) = crate::server::start_test_server(dist.clone());

        let lines = b"{\"t\":10,\"f\":1}\n{\"t\":20,\"f\":2}\n{\"t\":30,\"f\":3}";
        let (status, _) = http_post(
            port,
            "/data/ball-positions?bin=testbin-1234&session=abc123def456",
            lines,
        );
        assert_eq!(status, 204);

        let (status, body) = http_get(port, "/data/ball-positions?sample=2");
        assert_eq!(status, 200);
        assert!(body.contains("\"t\":10"), "{body}");
        assert!(body.contains("\"t\":30"), "{body}");
        assert!(!body.contains("\"t\":20"), "{body}");

        let (status, body) = http_get(port, "/data/nope-bucket");
        assert_eq!(status, 404);
        assert!(body.contains("no such bucket"), "{body}");

        testutil::rmrf(&dist);
    }

    fn http_post(port: u16, target: &str, body: &[u8]) -> (u16, String) {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "POST {target} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        s.write_all(body).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status: u16 = out
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, out)
    }

    fn http_get(port: u16, target: &str) -> (u16, String) {
        use std::io::Read;
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status: u16 = out
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = out
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    #[test]
    fn buckets_lists_and_filters_by_bin() {
        let dist = testutil::temp_dir("data-list");
        write_bucket(&dist, "ball-positions", "testbin-1234", "abc123def456", &[r#"{"t":1}"#]);
        write_bucket(&dist, "ball-positions", "testbin-1234", "bcd234efg789", &[r#"{"t":2}"#]);
        let listed = buckets(&dist);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].bucket, "ball-positions");
        assert_eq!(listed[0].files.len(), 2);

        let opts = QueryOpts {
            session: Some("bcd234efg789".into()),
            ..Default::default()
        };
        let one = query(&dist, "ball-positions", &opts).unwrap();
        assert_eq!(one.lines().count(), 1);
        assert!(one.contains(r#""t":2"#));
        testutil::rmrf(&dist);
    }
}
