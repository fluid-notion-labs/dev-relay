use std::path::PathBuf;
use std::process::{Child, Command, ExitCode};
use std::time::{Duration, Instant};

use crate::client;
use crate::httpc::{self, Base};
use crate::manifest;

pub const LAUNCHER_BIN: &str = "relay-launcher";
pub const QUICK_FAIL: Duration = Duration::from_secs(10);
pub const FAIL_STRIKES: u32 = 2;
pub const RESPAWN_DELAY: Duration = Duration::from_secs(1);
pub const CHILD_POLL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct LauncherConfig {
    pub url: String,
    pub dir: PathBuf,
    pub poll: Duration,
    pub game_args: Vec<String>,
    /// when true, also emit DEBUG-level lines (poll results, child args, …)
    /// via `--verbose` or `BILLIARDS_LAUNCHER_VERBOSE=1`
    pub verbose: bool,
    /// when true, respawn the child even after a clean (code 0) exit, e.g.
    /// when the user quits the game and the launcher is meant to keep the
    /// session alive (dev-relay dev loop). Default: stop on clean exit.
    pub respawn_on_ok: bool,
}

pub fn run(cfg: &LauncherConfig) -> Result<ExitCode, String> {
    let base = httpc::parse_base(&cfg.url)?;
    let bins_dir = cfg.dir.join("bins");
    std::fs::create_dir_all(&bins_dir).map_err(|e| format!("bins dir: {e}"))?;
    let dir = cfg
        .dir
        .canonicalize()
        .map_err(|e| format!("dir {}: {e}", cfg.dir.display()))?;
    let cfg = LauncherConfig {
        dir,
        ..cfg.clone()
    };
    let exe = std::env::consts::EXE_SUFFIX == ".exe";

    log(&cfg.url, "INFO", &format!("launcher start url={} dir={} poll={:?} verbose={}", cfg.url, cfg.dir.display(), cfg.poll, cfg.verbose));

    let mut run = Run {
        cfg: &cfg,
        base,
        exe,
        child: None,
        bin: None,
        spawned_at: Instant::now(),
        quick_fails: 0,
        blocked: None,
        manifest_ok: true,
        log_level: None,
        stderr_path: None,
    };
    run.loop_forever()
}

struct Run<'a> {
    cfg: &'a LauncherConfig,
    base: Base,
    exe: bool,
    child: Option<Child>,
    bin: Option<String>,
    spawned_at: Instant,
    quick_fails: u32,
    blocked: Option<String>,
    manifest_ok: bool,
    /// RUST_LOG filter currently deployed to the child (`None` = default)
    log_level: Option<String>,
    /// per-session stderr capture file (panics, early crashes)
    stderr_path: Option<std::path::PathBuf>,
}

impl Run<'_> {
    /// Last panic-relevant lines from the current session's stderr file.
    fn stderr_excerpt(&self) -> Option<String> {
        let path = self.stderr_path.as_ref()?;
        let data = std::fs::read(path).ok()?;
        if data.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&data);
        // keep the tail: panic message, location, note
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(8);
        Some(
            lines[start..]
                .join(" | ")
                .chars()
                .take(500)
                .collect(),
        )
    }
}

impl Run<'_> {
    fn loop_forever(&mut self) -> Result<ExitCode, String> {
        let mut next_fetch = Instant::now();
        loop {
            if let Some(code) = self.reap_child() {
                return Ok(code);
            }
            if Instant::now() >= next_fetch {
                next_fetch = Instant::now() + self.cfg.poll;
                self.apply_log_level();
                if let Ok(latest) = self.fetch_latest() {
                    if self.blocked.as_deref() == Some(latest.as_str()) {
                        self.dbg(&format!("latest={latest} blocked, holding"));
                        continue;
                    }
                    if self.bin.as_deref() != Some(latest.as_str()) {
                        self.switch_to(&latest);
                    } else {
                        self.dbg(&format!("latest={latest} already running"));
                    }
                } else {
                    self.dbg("manifest fetch failed");
                }
            }
            std::thread::sleep(CHILD_POLL);
        }
    }

    /// Pick up `client_log_level` from the server; on change, restart the
    /// running child so it comes back with the new RUST_LOG filter.
    fn apply_log_level(&mut self) {
        let fetched = self.fetch_log_level();
        if fetched == self.log_level {
            return;
        }
        self.log_level = fetched.clone();
        let shown = fetched.as_deref().unwrap_or("(client default)");
        log(
            &self.cfg.url,
            "INFO",
            &format!("client log level -> {shown}"),
        );
        if self.child.is_some() {
            let bin = self.bin.clone();
            self.kill_child("client log level change");
            if let Some(bin) = bin {
                self.spawn_game(&bin);
            }
        }
    }

    fn fetch_log_level(&mut self) -> Option<String> {
        let fetch = httpc::request(&self.base, "GET", "/client-log-level", None, 8 * 1024);
        fetch
            .ok()
            .filter(|r| r.status == 200)
            .and_then(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .and_then(|v| {
                v.get("level")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
    }

    fn fetch_latest(&mut self) -> Result<String, ()> {
        let fetch = httpc::request(&self.base, "GET", "/manifest.json", None, 256 * 1024);
        let name = match fetch {
            Ok(resp) if resp.status == 200 => manifest::parse(&resp.body)
                .ok()
                .and_then(|m| m.latest_for(self.exe).cloned()),
            _ => None,
        };
        match name {
            Some(n) => {
                self.manifest_ok = true;
                Ok(n)
            }
            None => {
                if self.manifest_ok {
                    self.manifest_ok = false;
                    log(&self.cfg.url, "WARN", "manifest fetch failed; will retry");
                }
                Err(())
            }
        }
    }

    fn dbg(&self, msg: &str) {
        if self.cfg.verbose {
            log(&self.cfg.url, "DEBUG", msg);
        }
    }

    fn switch_to(&mut self, latest: &str) {
        if self.child.is_some() {
            let old = self.bin.clone().unwrap_or_default();
            self.kill_child("update available");
            log(
                &self.cfg.url,
                "INFO",
                &format!("update: {old} -> {latest}"),
            );
        }
        if self.bin.as_deref() != Some(latest) {
            self.quick_fails = 0;
            self.blocked = None;
        }
        let t = Instant::now();
        match client::install_bin(&self.base, &self.cfg.dir.join("bins"), latest) {
            Ok(fresh) => log(
                &self.cfg.url,
                "INFO",
                &format!(
                    "install {latest} fresh={fresh} in {}ms",
                    t.elapsed().as_millis()
                ),
            ),
            Err(e) => {
                log(&self.cfg.url, "WARN", &format!("install {latest} failed: {e}"));
                return;
            }
        }
        self.spawn_game(latest);
    }

    fn spawn_game(&mut self, bin: &str) {
        let session = client::mint_session();
        let mut args = self.cfg.game_args.clone();
        args.push("--relay-url".into());
        args.push(self.cfg.url.clone());
        args.push("--relay-bin".into());
        args.push(bin.to_string());
        args.push("--relay-session".into());
        args.push(session.clone());
        let t = Instant::now();
        self.dbg(&format!("spawn {bin}: {:?} (cwd {})", args, self.cfg.dir.display()));
        let mut command = Command::new(self.cfg.dir.join("bins").join(bin));
        command.args(&args).current_dir(&self.cfg.dir);
        if let Some(level) = &self.log_level {
            command.env("RUST_LOG", level);
        }
        // capture stderr (panics, early crashes) to a per-session file so a
        // code-101 startup abort is diagnosable; read back on abnormal exit
        let logs_dir = self.cfg.dir.join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        self.stderr_path = Some(
            logs_dir.join(format!("{bin}.{session}.stderr")),
        );
        if let Some(path) = &self.stderr_path {
            match std::fs::File::create(path) {
                Ok(f) => {
                    command.stderr(std::process::Stdio::from(f));
                }
                Err(e) => self.dbg(&format!("stderr capture unavailable: {e}")),
            }
        }
        let spawn = command.spawn();
        match spawn {
            Ok(child) => {
                self.child = Some(child);
                self.bin = Some(bin.to_string());
                self.spawned_at = Instant::now();
                log(
                    &self.cfg.url,
                    "INFO",
                    &format!("spawn {bin} session={session} in {}ms", t.elapsed().as_millis()),
                );
            }
            Err(e) => {
                log(&self.cfg.url, "ERROR", &format!("spawn {bin} failed: {e}"));
            }
        }
    }

    fn kill_child(&mut self, reason: &str) -> bool {
        if let Some(mut child) = self.child.take() {
            let bin = self.bin.take();
            let _ = child.kill();
            let _ = child.wait();
            if let Some(bin) = bin {
                log(
                    &self.cfg.url,
                    "INFO",
                    &format!("stopped {bin} ({reason})"),
                );
            }
            return true;
        }
        false
    }

    fn reap_child(&mut self) -> Option<ExitCode> {
        let exit = self
            .child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten());
        let exit = exit?;
        self.child = None;
        let bin = self.bin.clone().unwrap_or_default();
        let ran = self.spawned_at.elapsed();
        let Some(code) = exit.code() else {
            log(&self.cfg.url, "WARN", &format!("{bin} killed by signal"));
            self.respawn_or_block(&bin, true);
            return None;
        };
        // abnormal exits log a compact stderr excerpt (panic message)
        if code != 0 {
            if let Some(excerpt) = self.stderr_excerpt() {
                log(
                    &self.cfg.url,
                    "ERROR",
                    &format!("{bin} stderr: {excerpt}"),
                );
            }
        }
        if code == 0 {
            log(
                &self.cfg.url,
                "INFO",
                &format!("{bin} exited ok after {:.1}s", ran.as_secs_f32()),
            );
            if self.cfg.respawn_on_ok {
                // respawn_or_block spawned a replacement child — keep looping
                self.respawn_or_block(&bin, false);
                return None;
            }
            return Some(ExitCode::SUCCESS);
        }
        log(
            &self.cfg.url,
            "WARN",
            &format!("{bin} exited code={code} after {:.1}s", ran.as_secs_f32()),
        );
        self.respawn_or_block(&bin, ran < QUICK_FAIL);
        None
    }

    fn respawn_or_block(&mut self, bin: &str, quick: bool) {
        if !quick {
            self.quick_fails = 0;
            log(&self.cfg.url, "INFO", &format!("restarting {bin}"));
            self.spawn_game(bin);
            return;
        }
        self.quick_fails += 1;
        if self.quick_fails >= FAIL_STRIKES {
            self.blocked = Some(bin.to_string());
            log(
                &self.cfg.url,
                "ERROR",
                &format!(
                    "{bin} failed to boot {} times; holding until a newer build is published",
                    self.quick_fails
                ),
            );
            return;
        }
        log(
            &self.cfg.url,
            "WARN",
            &format!("{bin} died at startup; retrying in {:?}", RESPAWN_DELAY),
        );
        std::thread::sleep(RESPAWN_DELAY);
        self.spawn_game(bin);
    }
}

fn log(url: &str, level: &str, msg: &str) {
    eprintln!("relay-launcher: {msg}");
    client::post_line(url, LAUNCHER_BIN, level, "launcher", msg, "");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::httpc::Base;
    use crate::server::start_test_server;
    use crate::testutil;
    use std::fs;
    use std::path::Path;

    fn publish_bin(dist: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(dist.join("incoming").join(name), body).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let published = dist.join("bins").join(name);
            if published.is_file() {
                let _ = fs::set_permissions(&published, fs::Permissions::from_mode(0o755));
                return;
            }
            assert!(Instant::now() < deadline, "watcher never published {name}");
        }
    }

    #[test]
    fn launcher_runs_latest_then_exits_with_child() {
        let dist = testutil::temp_dir("launcher-e2e");
        let (port, _state) = start_test_server(dist.clone());
        publish_bin(&dist, "aaa1111", "#!/bin/sh\nexit 0\n");

        let work = testutil::temp_dir("launcher-work");
        let cfg = LauncherConfig {
            url: format!("http://127.0.0.1:{port}"),
            dir: work.clone(),
            poll: Duration::from_millis(100),
            game_args: vec![],
            verbose: false,
            respawn_on_ok: false,
        };
        let code = run(&cfg).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);

        assert!(work.join("bins").join("aaa1111").is_file());
        let base = Base {
            host: "127.0.0.1".into(),
            port,
        };
        let resp = httpc::request(
            &base,
            "GET",
            &format!("/logs/{LAUNCHER_BIN}"),
            None,
            64 * 1024,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&resp.body);
        assert!(text.contains("spawn aaa1111"), "{text}");
        assert!(text.contains("exited ok"), "{text}");

        testutil::rmrf(&work);
        testutil::rmrf(&dist);
    }

    #[test]
    fn crash_guard_blocks_after_two_quick_failures() {
        let dist = testutil::temp_dir("launcher-guard");
        let (port, _state) = start_test_server(dist.clone());
        publish_bin(&dist, "aaa1111", "#!/bin/sh\nsleep 30\n");

        let cfg = LauncherConfig {
            url: format!("http://127.0.0.1:{port}"),
            dir: dist.clone(),
            poll: Duration::from_millis(100),
            game_args: vec![],
            verbose: false,
            respawn_on_ok: false,
        };
        let base = Base {
            host: "127.0.0.1".into(),
            port,
        };
        let mut run = Run {
            cfg: &cfg,
            base,
            exe: false,
            child: None,
            bin: None,
            spawned_at: Instant::now(),
            quick_fails: 0,
            blocked: None,
            manifest_ok: true,
            log_level: None,
            stderr_path: None,
        };
        run.respawn_or_block("aaa1111", true);
        assert_eq!(run.quick_fails, 1);
        assert!(run.blocked.is_none());
        assert!(run.child.is_some(), "first strike respawns");
        let child = run.child.as_mut().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        run.child = None;
        run.respawn_or_block("aaa1111", true);
        assert_eq!(run.quick_fails, FAIL_STRIKES);
        assert_eq!(run.blocked.as_deref(), Some("aaa1111"));
        assert!(run.child.is_none(), "second strike holds");

        testutil::rmrf(&dist);
    }
}
