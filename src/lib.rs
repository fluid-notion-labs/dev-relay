pub mod build;
pub mod client;
pub mod control;
pub mod httpc;
pub mod launcher;
pub mod manifest;
pub mod mcp;
pub mod naming;
pub mod server;

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    pub fn temp_dir(name: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dev-relay-test-{}-{name}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn rmrf(dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rmrf(&path);
                } else {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let _ = std::fs::remove_dir(dir);
    }
}
