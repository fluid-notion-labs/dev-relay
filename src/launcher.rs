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

    log(&cfg.url, "INFO", &format!("launcher start url={} dir={} poll={:?}", cfg.url, cfg.dir.display(), cfg.poll));

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
                if let Ok(latest) = self.fetch_latest() {
                    if self.blocked.as_deref() == Some(latest.as_str()) {
                        continue;
                    }
                    if self.bin.as_deref() != Some(latest.as_str()) {
                        self.switch_to(&latest);
                    }
                }
            }
            std::thread::sleep(CHILD_POLL);
        }
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
        let spawn = Command::new(self.cfg.dir.join("bins").join(bin))
            .args(&args)
            .current_dir(&self.cfg.dir)
            .spawn();
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
        if code == 0 {
            log(
                &self.cfg.url,
                "INFO",
                &format!("{bin} exited ok after {:.1}s", ran.as_secs_f32()),
            );
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
