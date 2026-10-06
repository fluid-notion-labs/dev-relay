use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::naming;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub mtime: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub latest: BTreeMap<String, String>,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    pub fn latest_for(&self, exe: bool) -> Option<&String> {
        self.latest.get(if exe { "exe" } else { "" })
    }
}

pub fn scan_bins(bins_dir: &Path) -> std::io::Result<Manifest> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(bins_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !naming::is_bin_name(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        files.push(FileEntry {
            name,
            mtime,
            size: meta.len(),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let mut best: BTreeMap<&str, (u64, &str)> = BTreeMap::new();
    for f in &files {
        let key = naming::suffix_key(&f.name);
        let cand = (f.mtime, f.name.as_str());
        let better = match best.get(key) {
            Some(cur) => cand > *cur,
            None => true,
        };
        if better {
            best.insert(key, cand);
        }
    }
    let latest = best
        .into_iter()
        .map(|(k, (_, n))| (k.to_string(), n.to_string()))
        .collect();
    Ok(Manifest { latest, files })
}

pub fn refresh(dist_dir: &Path) -> std::io::Result<Manifest> {
    let manifest = scan_bins(&dist_dir.join("bins"))?;
    write_atomic(dist_dir, &manifest)?;
    Ok(manifest)
}

pub fn write_atomic(dist_dir: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let json = serde_json::to_vec(manifest).map_err(std::io::Error::other)?;
    let tmp = dist_dir.join("manifest.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(tmp, dist_dir.join("manifest.json"))
}

pub fn parse(json: &[u8]) -> Result<Manifest, String> {
    serde_json::from_slice(json).map_err(|e| format!("bad manifest: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn scan_picks_latest_per_suffix() {
        let dist = testutil::temp_dir("manifest");
        let bins = dist.join("bins");
        fs::create_dir_all(&bins).unwrap();
        fs::write(bins.join("aaa1111"), b"old").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(bins.join("bbb2222"), b"new").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        fs::write(bins.join("ccc3333.exe"), b"newest-exe").unwrap();
        let m = scan_bins(&bins).unwrap();
        assert_eq!(m.latest.get(""), Some(&"bbb2222".to_string()));
        assert_eq!(m.latest.get("exe"), Some(&"ccc3333.exe".to_string()));
        assert_eq!(m.files.len(), 3);
        testutil::rmrf(&dist);
    }

    #[test]
    fn write_parse_round_trip() {
        let dist = testutil::temp_dir("manifest-rt");
        fs::create_dir_all(&dist).unwrap();
        let m = Manifest {
            latest: BTreeMap::from([("".to_string(), "x".to_string())]),
            files: vec![FileEntry {
                name: "x".to_string(),
                mtime: 5,
                size: 2,
            }],
        };
        write_atomic(&dist, &m).unwrap();
        let raw = fs::read(dist.join("manifest.json")).unwrap();
        assert_eq!(parse(&raw).unwrap(), m);
        assert!(parse(b"{oops").is_err());
        testutil::rmrf(&dist);
    }
}
