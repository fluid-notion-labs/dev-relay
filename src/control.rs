use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ACTIVE_WINDOW: Duration = Duration::from_secs(6);

pub struct ControlState {
    queues: Mutex<HashMap<(String, String), VecDeque<Value>>>,
    active: Mutex<HashMap<String, (String, Instant)>>,
    signal: Condvar,
    counter: AtomicU64,
    pub wait_ms: AtomicU64,
}

impl Default for ControlState {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlState {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            signal: Condvar::new(),
            counter: AtomicU64::new(0),
            wait_ms: AtomicU64::new(5000),
        }
    }

    pub fn enqueue(&self, dist: &Path, bin: &str, session: &str, mut cmd: Value) -> String {
        let id = format!("c{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        cmd["id"] = Value::String(id.clone());
        {
            let mut queues = self.queues.lock().unwrap();
            queues
                .entry((bin.to_string(), session.to_string()))
                .or_default()
                .push_back(cmd.clone());
        }
        self.signal.notify_all();
        let rec = json!({"ts": now_ms(), "session": session, "id": id, "cmd": cmd});
        let dir = dist.join("inputs");
        let _ = std::fs::create_dir_all(&dir);
        let mut line = serde_json::to_vec(&rec).unwrap_or_default();
        line.push(b'\n');
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{bin}.jsonl")))
            .and_then(|mut f| f.write_all(&line));
        id
    }

    pub fn poll(&self, bin: &str, session: &str) -> Vec<Value> {
        {
            let mut active = self.active.lock().unwrap();
            active.insert(bin.to_string(), (session.to_string(), Instant::now()));
        }
        let key = (bin.to_string(), session.to_string());
        let wait = Duration::from_millis(self.wait_ms.load(Ordering::Relaxed));
        let mut queues = self.queues.lock().unwrap();
        let deadline = Instant::now() + wait;
        loop {
            let out = queues
                .get_mut(&key)
                .map(|queue| queue.drain(..).collect::<Vec<_>>())
                .unwrap_or_default();
            if !out.is_empty() {
                queues.remove(&key);
                return out;
            }
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            let (guard, _) = self.signal.wait_timeout(queues, deadline - now).unwrap();
            queues = guard;
        }
    }

    pub fn active_session(&self, bin: &str) -> Option<String> {
        let active = self.active.lock().unwrap();
        active
            .get(bin)
            .filter(|(_, t)| t.elapsed() < ACTIVE_WINDOW)
            .map(|(s, _)| s.clone())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use serde_json::json;

    #[test]
    fn enqueue_then_poll_drains_and_records() {
        let dist = testutil::temp_dir("control-state");
        let state = ControlState::new();
        state.wait_ms.store(20, Ordering::Relaxed);
        let id = state.enqueue(&dist, "0123abcd", "ab12cd", json!({"op": "screenshot"}));
        assert_eq!(state.active_session("0123abcd"), None);
        let out = state.poll("0123abcd", "ab12cd");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], id.as_str());
        assert!(state.poll("0123abcd", "ab12cd").is_empty());
        assert_eq!(state.active_session("0123abcd"), Some("ab12cd".into()));
        let rec = std::fs::read_to_string(dist.join("inputs").join("0123abcd.jsonl")).unwrap();
        assert!(rec.contains(r#""session":"ab12cd""#));
        assert!(rec.contains(&id));
        testutil::rmrf(&dist);
    }

    #[test]
    fn poll_waits_until_enqueued() {
        let dist = testutil::temp_dir("control-wait");
        let state = std::sync::Arc::new(ControlState::new());
        state.wait_ms.store(2000, Ordering::Relaxed);
        let waiter = std::sync::Arc::clone(&state);
        let t = std::thread::spawn(move || waiter.poll("0123abcd", "ab12cd"));
        std::thread::sleep(Duration::from_millis(50));
        state.enqueue(
            &dist,
            "0123abcd",
            "ab12cd",
            json!({"op": "wheel", "dx": 0.0, "dy": -1.0}),
        );
        let out = t.join().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["op"], "wheel");
        testutil::rmrf(&dist);
    }
}
