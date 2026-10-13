//! The vault folder on the laptop: which files sync, their hashes (cached
//! by size and mtime so a pass rereads only what changed), and the
//! conditional writes the client's steps ask for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cce_sync::client::Local;
use cce_sync::proto::{check_path, hash};
use serde::{Deserialize, Serialize};

/// What a file's hash was computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cached {
    pub size: u64,
    /// Nanoseconds since the epoch.
    pub mtime: u128,
    pub hash: String,
}

pub type Cache = BTreeMap<String, Cached>;

/// A file modified this recently is rehashed even when size and mtime
/// match: a second write within the timestamp's granularity would
/// otherwise go unseen.
const SETTLE: Duration = Duration::from_secs(2);

/// Whether a path under the vault syncs at all: nothing hidden except the
/// settings folder. Hidden temps (cce-vault's `.name.PID.cce-tmp`) and
/// `.obsidian/`, `.git/`, `.trash/` are all out.
pub fn wanted(rel: &str) -> bool {
    check_path(rel).is_ok()
}

/// Every syncable file under `root` as a vault path, and the names skipped
/// as invalid. Hidden folders are not entered.
pub fn walk(root: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let walk = walkdir::WalkDir::new(root).follow_links(false).sort_by_file_name().into_iter().filter_entry(|e| {
        e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.') || (e.depth() == 1 && e.file_name() == ".cce")
    });
    for entry in walk {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).map_err(|e| e.to_string())?;
        let Some(rel) = rel.to_str().map(|s| s.replace(std::path::MAIN_SEPARATOR, "/")) else {
            skipped.push(format!("{} (name is not UTF-8)", rel.display()));
            continue;
        };
        match check_path(&rel) {
            Ok(()) => files.push(rel),
            // Hidden files inside .cce/ and the like: ignored, not reported.
            Err(cce_sync::proto::PathError::Hidden) => {}
            Err(e) => skipped.push(format!("{rel} ({e})")),
        }
    }
    Ok((files, skipped))
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

fn settled(meta: &std::fs::Metadata) -> bool {
    meta.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= SETTLE)
}

/// The folder as the client sees it: vault path → hash. The cache is
/// updated, and entries for vanished files dropped.
pub fn scan(root: &Path, cache: &mut Cache) -> Result<(Local, Vec<String>), String> {
    let (paths, skipped) = walk(root)?;
    let mut local = Local::new();
    let mut fresh = Cache::new();
    for rel in paths {
        let abs = root.join(&rel);
        let Ok(meta) = std::fs::metadata(&abs) else { continue };
        let (size, mtime) = (meta.len(), mtime_ns(&meta));
        let hit = cache.get(&rel).filter(|c| c.size == size && c.mtime == mtime && settled(&meta));
        let h = match hit {
            Some(c) => c.hash.clone(),
            None => match std::fs::read(&abs) {
                Ok(bytes) => hash(&bytes),
                Err(_) => continue, // vanished mid-scan
            },
        };
        fresh.insert(rel.clone(), Cached { size, mtime, hash: h.clone() });
        local.insert(rel, h);
    }
    *cache = fresh;
    Ok((local, skipped))
}

/// The vault folder, with conditional writes.
pub struct Folder {
    pub root: PathBuf,
}

impl Folder {
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The hash of what is at `rel` now, read fresh (`None`: nothing).
    pub fn hash_now(&self, rel: &str) -> Option<String> {
        std::fs::read(self.abs(rel)).ok().map(|b| hash(&b))
    }

    pub fn holds(&self, rel: &str, expect: Option<&str>) -> bool {
        self.hash_now(rel).as_deref() == expect
    }

    pub fn read(&self, rel: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.abs(rel))
    }

    /// Write atomically (a hidden temp and a rename, cce-vault's write), so
    /// cce-notes and the watcher never see half a file.
    pub fn write(&self, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
        cce_vault::write::atomic_write(&self.abs(rel), bytes)
    }

    /// Remove a file, and any folders it leaves empty (a remote folder
    /// rename should not leave a trail of empty ones).
    pub fn remove(&self, rel: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.abs(rel))?;
        let mut dir = self.abs(rel).parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d == self.root || std::fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        Ok(())
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.abs(rel).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &Path, rel: &str, body: &str) {
        std::fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
        std::fs::write(root.join(rel), body).unwrap();
    }

    #[test]
    fn walk_skips_hidden_but_keeps_settings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for rel in ["Note.md", "Tasks/To Do.md", ".obsidian/app.json", ".git/HEAD", ".cce/vault.kdl", "Notes/.draft.md", ".Note.md.12.cce-tmp"] {
            put(p, rel, "x");
        }
        let (files, skipped) = walk(p).unwrap();
        assert_eq!(files, vec![".cce/vault.kdl", "Note.md", "Tasks/To Do.md"]);
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn scan_hashes_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        put(p, "a.md", "one");
        let mut cache = Cache::new();
        let (local, _) = scan(p, &mut cache).unwrap();
        assert_eq!(local["a.md"], hash(b"one"));
        // A changed file is rehashed even when its mtime has not settled.
        put(p, "a.md", "two");
        let (local, _) = scan(p, &mut cache).unwrap();
        assert_eq!(local["a.md"], hash(b"two"));
        std::fs::remove_file(p.join("a.md")).unwrap();
        let (local, _) = scan(p, &mut cache).unwrap();
        assert!(local.is_empty() && cache.is_empty());
    }

    #[test]
    fn remove_prunes_empty_folders() {
        let dir = tempfile::tempdir().unwrap();
        let f = Folder { root: dir.path().to_path_buf() };
        f.write("A/B/c.md", b"x").unwrap();
        f.write("A/d.md", b"y").unwrap();
        f.remove("A/B/c.md").unwrap();
        assert!(!dir.path().join("A/B").exists());
        assert!(dir.path().join("A").exists(), "A still holds d.md");
        assert!(f.holds("A/d.md", Some(&hash(b"y"))));
        assert!(f.holds("A/none.md", None));
    }
}
