//! A [`Store`] in memory: the tests' vault, and the shape any other host
//! (a VPS binary) would start from.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use crate::core::{Commit, FileRow, Store, StoreError};
use crate::proto::{fold, Change, Rev, Seq, Version};

#[derive(Default)]
pub struct MemStore {
    pub files: BTreeMap<String, FileRow>,
    pub changes: Vec<Change>,
    pub versions: Vec<Version>,
    pub blobs: RefCell<HashMap<String, Vec<u8>>>,
    pub seq: Seq,
}

impl MemStore {
    /// Drop changes with seq <= `upto`, as the server's trim does.
    pub fn trim_changes(&mut self, upto: Seq) {
        self.changes.retain(|c| c.seq > upto);
    }
}

impl Store for MemStore {
    fn file(&self, path: &str) -> Result<Option<FileRow>, StoreError> {
        Ok(self.files.get(path).cloned())
    }

    fn live_by_fold(&self, folded: &str) -> Result<Vec<FileRow>, StoreError> {
        Ok(self.files.values().filter(|r| r.is_live() && fold(&r.path) == folded).cloned().collect())
    }

    fn live_files(&self) -> Result<Vec<FileRow>, StoreError> {
        Ok(self.files.values().filter(|r| r.is_live()).cloned().collect())
    }

    fn seq(&self) -> Result<Seq, StoreError> {
        Ok(self.seq)
    }

    fn oldest_seq(&self) -> Result<Option<Seq>, StoreError> {
        Ok(self.changes.first().map(|c| c.seq))
    }

    fn changes_after(&self, since: Seq, limit: usize) -> Result<Vec<Change>, StoreError> {
        Ok(self.changes.iter().filter(|c| c.seq > since).take(limit).cloned().collect())
    }

    fn versions(&self, path: &str) -> Result<Vec<Version>, StoreError> {
        let mut v: Vec<Version> = self.versions.iter().filter(|v| v.path == path).cloned().collect();
        v.sort_by_key(|v| std::cmp::Reverse(v.rev));
        Ok(v)
    }

    fn version(&self, path: &str, rev: Rev) -> Result<Option<Version>, StoreError> {
        Ok(self.versions.iter().find(|v| v.path == path && v.rev == rev).cloned())
    }

    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.blobs.borrow().get(hash).cloned())
    }

    async fn blob_put(&self, hash: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.blobs.borrow_mut().insert(hash.to_string(), bytes.to_vec());
        Ok(())
    }

    fn commit(&mut self, commit: Commit) -> Result<Seq, StoreError> {
        for row in commit.rows {
            self.versions.push(row.version());
            self.files.insert(row.path.clone(), row);
        }
        for c in commit.changes {
            self.seq += 1;
            self.changes.push(Change {
                seq: self.seq,
                op: c.op,
                path: c.path,
                rev: c.rev,
                hash: c.hash,
                size: c.size,
                from: c.from,
                device: c.device,
                at: c.at,
            });
        }
        Ok(self.seq)
    }
}
