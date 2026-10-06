use std::path::{Path, PathBuf};
use std::process::Command;

use crate::naming;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Linux,
    Win,
}

#[derive(Clone)]
pub struct BuildConfig {
    pub project: PathBuf,
    pub dist: PathBuf,
    pub bin: Option<String>,
    pub target: Target,
}

pub fn run(cfg: &BuildConfig) -> Result<String, String> {
    let bin = match &cfg.bin {
        Some(b) => b.clone(),
        None => default_bin(&cfg.project)?,
    };
    let exe = match cfg.target {
        Target::Linux => false,
        Target::Win => {
            return Err(
                "--target win is deferred (M4): install the mingw toolchain and wire \
                 x86_64-pc-windows-gnu first"
                    .into(),
            );
        }
    };
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--release")
        .arg("--bin")
        .arg(&bin)
        .current_dir(&cfg.project);
    let status = cmd.status().map_err(|e| format!("cargo: {e}"))?;
    if !status.success() {
        return Err("cargo build failed".into());
    }
    let bin_path = cfg.project.join("target/release").join(if exe {
        format!("{bin}.exe")
    } else {
        bin.clone()
    });
    if !bin_path.is_file() {
        return Err(format!(
            "cargo succeeded but binary missing: {}",
            bin_path.display()
        ));
    }
    let sha = git(&cfg.project, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();
    let dirty = !git(&cfg.project, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    crate::server::prepare(&cfg.dist).map_err(|e| format!("dist dir: {e}"))?;
    let existing = existing_names(&cfg.dist);
    let name = if dirty {
        naming::next_dirty_name(&sha, exe, &existing)
    } else if exe {
        format!("{sha}.exe")
    } else {
        sha
    };
    let incoming = cfg.dist.join("incoming");
    let tmp = incoming.join(format!(".tmp-{name}"));
    std::fs::copy(&bin_path, &tmp).map_err(|e| format!("copy to incoming: {e}"))?;
    std::fs::rename(&tmp, incoming.join(&name)).map_err(|e| format!("rename in incoming: {e}"))?;
    Ok(name)
}

fn git(project: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!("git {} failed (not a git repo?)", args[0]));
    }
    String::from_utf8(out.stdout).map_err(|_| "git output not utf-8".into())
}

fn existing_names(dist: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for sub in ["bins", "incoming"] {
        if let Ok(entries) = std::fs::read_dir(dist.join(sub)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if naming::is_bin_name(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

fn default_bin(project: &Path) -> Result<String, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(project)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed; pass --bin explicitly".into());
    }
    let meta = String::from_utf8_lossy(&out.stdout).into_owned();
    let abs = project
        .canonicalize()
        .map_err(|e| format!("project dir: {e}"))?;
    root_package_name(&meta, &abs)
}

fn root_package_name(metadata_json: &str, project: &Path) -> Result<String, String> {
    let meta: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|_| "unparsable cargo metadata")?;
    let packages = meta["packages"]
        .as_array()
        .ok_or("no packages in cargo metadata")?;
    let wanted = project.join("Cargo.toml");
    let name = packages
        .iter()
        .find(|p| {
            p["manifest_path"]
                .as_str()
                .is_some_and(|m| Path::new(m) == wanted)
        })
        .and_then(|p| p["name"].as_str())
        .ok_or("no package at project root; pass --bin explicitly")?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn parses_root_package_name() {
        let project = Path::new("/repo");
        let one = r#"{"packages":[{"id":"p1","name":"my-game","manifest_path":"/repo/Cargo.toml"}],"resolve":null}"#;
        assert_eq!(root_package_name(one, project).unwrap(), "my-game");
        let multi = r#"{"packages":[{"id":"a","name":"x","manifest_path":"/other/Cargo.toml"},{"id":"b","name":"y","manifest_path":"/repo/Cargo.toml"}],"resolve":null}"#;
        assert_eq!(root_package_name(multi, project).unwrap(), "y");
        let none = r#"{"packages":[{"id":"a","name":"x","manifest_path":"/other/Cargo.toml"}],"resolve":null}"#;
        assert!(root_package_name(none, project).is_err());
    }

    #[test]
    fn existing_names_scans_bins_and_incoming() {
        let dist = testutil::temp_dir("build-names");
        std::fs::create_dir_all(dist.join("bins")).unwrap();
        std::fs::create_dir_all(dist.join("incoming")).unwrap();
        std::fs::write(dist.join("bins").join("aaa1111"), b"x").unwrap();
        std::fs::write(dist.join("bins").join("ignored.txt"), b"x").unwrap();
        std::fs::write(dist.join("incoming").join("bbb2222"), b"x").unwrap();
        let names = existing_names(&dist);
        assert_eq!(names, vec!["aaa1111".to_string(), "bbb2222".to_string()]);
        testutil::rmrf(&dist);
    }
}
