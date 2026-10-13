//! The server's rules, apart from any host: what a write does to the
//! vault, and what the change feed, listing and history answer. The
//! Cloudflare Durable Object (`cce-sync-server`) and the tests run the same
//! code over different [`Store`]s, so every rule here is tested natively.
//!
//! A write is a batch of [`Operation`]s, applied in order to a staged view
//! of the vault and committed in one [`Store::commit`], or refused whole.
//! For each operation:
//!
//! - **Put** with `Pre::Rev(b)`: written when the file is at `b`. If it has
//!   moved on and the file is a note, the server merges base `b`, the
//!   current text and the incoming text three ways ([`crate::merge`]); a
//!   clean merge is written as a new revision and reported `merged`, so the
//!   client downloads it. Anything else is a [`Refusal::Conflict`].
//! - **Put** with `Pre::Absent`: written when the file does not exist (or
//!   was deleted).
//! - Writing bytes a file already has is `unchanged`, not a new revision,
//!   so a retried request is harmless.
//! - **Delete** needs the current revision; a newer remote edit wins.
//! - **Move** needs the source's current revision and a free destination.
//! - A new path whose case-folded form matches another live file's is a
//!   [`Refusal::CaseCollision`]: links resolve case-insensitively.
//!
//! Bytes go to the blob store as each operation is staged (content-
//! addressed, so a refused batch only leaves unreferenced blobs); the
//! metadata lands in the single synchronous commit, which a Durable Object
//! makes atomic by not awaiting inside it. The host must not run two
//! batches at once (the Durable Object holds a lock around [`Vault`]).

use std::collections::BTreeMap;

use crate::merge::merge3;
use crate::proto::{
    check_path, fold, hash, BatchOp, Change, ChangesPage, Committed, FileMeta, History, Kind, Listing, Op, Pre,
    Refusal, Rev, Seq, Version, Applied, MAX_FILE_BYTES,
};

/// A file's current state: live (`hash` set) or a tombstone, which keeps
/// the revision counter so a stale `If-Match` can never succeed after a
/// delete and re-create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub rev: Rev,
    pub hash: Option<String>,
    pub size: u64,
    pub device: String,
    pub at: i64,
}

impl FileRow {
    pub fn is_live(&self) -> bool {
        self.hash.is_some()
    }

    pub fn meta(&self) -> Option<FileMeta> {
        Some(FileMeta {
            path: self.path.clone(),
            rev: self.rev,
            hash: self.hash.clone()?,
            size: self.size,
            kind: Kind::of(&self.path),
        })
    }

    pub fn version(&self) -> Version {
        Version {
            path: self.path.clone(),
            rev: self.rev,
            hash: self.hash.clone(),
            size: self.size,
            device: self.device.clone(),
            at: self.at,
        }
    }
}

/// A change before the store numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChange {
    pub op: Op,
    pub path: String,
    pub rev: Rev,
    pub hash: Option<String>,
    pub size: Option<u64>,
    pub from: Option<String>,
    pub device: String,
    pub at: i64,
}

/// What one batch writes: the rows in order (a version is kept for each),
/// and the changes, which the store numbers from its next seq.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    pub rows: Vec<FileRow>,
    pub changes: Vec<NewChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store: {}", self.0)
    }
}

/// Where a vault lives. Metadata reads and the commit are synchronous
/// (SQLite in a Durable Object); blobs are async (R2).
#[allow(async_fn_in_trait)]
pub trait Store {
    /// The row for `path`, live or tombstone.
    fn file(&self, path: &str) -> Result<Option<FileRow>, StoreError>;
    /// Live rows whose [`fold`]ed path is `folded`.
    fn live_by_fold(&self, folded: &str) -> Result<Vec<FileRow>, StoreError>;
    /// Every live row, by path.
    fn live_files(&self) -> Result<Vec<FileRow>, StoreError>;
    /// The last seq committed (0 for an empty vault).
    fn seq(&self) -> Result<Seq, StoreError>;
    /// The oldest seq still in the change feed, if any is.
    fn oldest_seq(&self) -> Result<Option<Seq>, StoreError>;
    /// Changes with seq > `since`, oldest first, at most `limit`.
    fn changes_after(&self, since: Seq, limit: usize) -> Result<Vec<Change>, StoreError>;
    /// Every kept version of `path`, newest first.
    fn versions(&self, path: &str) -> Result<Vec<Version>, StoreError>;
    fn version(&self, path: &str, rev: Rev) -> Result<Option<Version>, StoreError>;
    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// Store bytes under their hash. Idempotent.
    async fn blob_put(&self, hash: &str, bytes: &[u8]) -> Result<(), StoreError>;
    /// Write the batch atomically: upsert each row (keeping a version of
    /// each), append the changes numbered from `seq() + 1`. Returns the
    /// new seq.
    fn commit(&mut self, commit: Commit) -> Result<Seq, StoreError>;
}

/// One write, with its bytes decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Put { path: String, pre: Pre, bytes: Vec<u8> },
    Delete { path: String, pre: Pre },
    Move { from: String, to: String, pre: Pre },
}

impl Operation {
    /// Decode a wire op. `index` is its place in the batch, for the refusal.
    pub fn from_wire(op: BatchOp) -> Result<Operation, Refusal> {
        Ok(match op {
            BatchOp::Put { path, pre, b64 } => {
                let bytes = crate::proto::b64_decode(&b64)
                    .ok_or_else(|| Refusal::BadRequest { reason: format!("{path}: bad base64") })?;
                Operation::Put { path, pre, bytes }
            }
            BatchOp::Delete { path, pre } => Operation::Delete { path, pre },
            BatchOp::Move { from, to, pre } => Operation::Move { from, to, pre },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    Refused(Refusal),
    Store(StoreError),
}

impl From<StoreError> for ApplyError {
    fn from(e: StoreError) -> Self {
        ApplyError::Store(e)
    }
}

impl From<Refusal> for ApplyError {
    fn from(r: Refusal) -> Self {
        ApplyError::Refused(r)
    }
}

/// The vault over a store: the one place writes are decided.
pub struct Vault<S> {
    pub store: S,
}

/// The view of the vault a batch works against: committed rows, overlaid
/// by what earlier operations in the batch staged.
struct Staged<'s, S> {
    store: &'s S,
    rows: BTreeMap<String, FileRow>,
    commit: Commit,
}

impl<S: Store> Staged<'_, S> {
    fn get(&self, path: &str) -> Result<Option<FileRow>, StoreError> {
        match self.rows.get(path) {
            Some(row) => Ok(Some(row.clone())),
            None => self.store.file(path),
        }
    }

    /// Another live file whose path folds like `path`, ignoring `except`
    /// (a move's own source: renaming `beta.md` to `Beta.md` is fine).
    fn collision(&self, path: &str, except: Option<&str>) -> Result<Option<String>, StoreError> {
        let folded = fold(path);
        let other = |p: &str| p != path && Some(p) != except;
        for row in self.rows.values() {
            if row.is_live() && fold(&row.path) == folded && other(&row.path) {
                return Ok(Some(row.path.clone()));
            }
        }
        for row in self.store.live_by_fold(&folded)? {
            // Staged rows override committed ones (a staged delete frees it).
            let row = self.rows.get(&row.path).cloned().unwrap_or(row);
            if row.is_live() && other(&row.path) {
                return Ok(Some(row.path));
            }
        }
        Ok(None)
    }

    fn stage(&mut self, row: FileRow, change: NewChange) {
        self.rows.insert(row.path.clone(), row.clone());
        self.commit.rows.push(row);
        self.commit.changes.push(change);
    }
}

fn path_ok(index: usize, path: &str) -> Result<(), Refusal> {
    check_path(path).map_err(|e| Refusal::BadPath { index, path: path.to_string(), reason: e.to_string() })
}

fn conflict(index: usize, path: &str, current: Option<&FileRow>) -> ApplyError {
    ApplyError::Refused(Refusal::Conflict {
        index,
        path: path.to_string(),
        current: current.and_then(FileRow::meta),
    })
}

impl<S: Store> Vault<S> {
    pub fn new(store: S) -> Self {
        Vault { store }
    }

    /// Apply a batch: all of it, or none (the first refusal is returned).
    pub async fn apply(&mut self, ops: Vec<Operation>, device: &str, now: i64) -> Result<Committed, ApplyError> {
        let mut staged = Staged { store: &self.store, rows: BTreeMap::new(), commit: Commit::default() };
        let mut applied = Vec::with_capacity(ops.len());
        for (index, op) in ops.into_iter().enumerate() {
            let a = match op {
                Operation::Put { path, pre, bytes } => put(&mut staged, index, path, pre, bytes, device, now).await?,
                Operation::Delete { path, pre } => delete(&mut staged, index, path, pre, device, now)?,
                Operation::Move { from, to, pre } => move_file(&mut staged, index, from, to, pre, device, now)?,
            };
            applied.push(a);
        }
        let commit = staged.commit;
        let seq = if commit.changes.is_empty() { self.store.seq()? } else { self.store.commit(commit)? };
        Ok(Committed { applied, seq })
    }

    /// The change feed after `since`.
    pub fn changes(&self, since: Seq, limit: usize) -> Result<ChangesPage, StoreError> {
        let current = self.store.seq()?;
        let reset = since > current
            || match self.store.oldest_seq()? {
                Some(oldest) => since + 1 < oldest,
                None => since < current,
            };
        if reset {
            return Ok(ChangesPage { changes: Vec::new(), seq: current, more: false, reset: true });
        }
        let limit = limit.max(1);
        let mut changes = self.store.changes_after(since, limit + 1)?;
        let more = changes.len() > limit;
        changes.truncate(limit);
        let seq = changes.last().map_or(current, |c| c.seq);
        Ok(ChangesPage { changes, seq, more, reset: false })
    }

    /// Every live file and the seq to pull changes from.
    pub fn listing(&self) -> Result<Listing, StoreError> {
        let files = self.store.live_files()?.iter().filter_map(FileRow::meta).collect();
        Ok(Listing { files, seq: self.store.seq()? })
    }

    /// A live file's metadata and bytes.
    pub async fn read(&self, path: &str) -> Result<Option<(FileMeta, Vec<u8>)>, StoreError> {
        let Some(meta) = self.store.file(path)?.as_ref().and_then(FileRow::meta) else { return Ok(None) };
        let bytes = self
            .store
            .blob_get(&meta.hash)
            .await?
            .ok_or_else(|| StoreError(format!("blob {} of {path} is missing", meta.hash)))?;
        Ok(Some((meta, bytes)))
    }

    pub fn history(&self, path: &str) -> Result<History, StoreError> {
        Ok(History { path: path.to_string(), versions: self.store.versions(path)? })
    }
}

async fn put<S: Store>(
    staged: &mut Staged<'_, S>,
    index: usize,
    path: String,
    pre: Pre,
    bytes: Vec<u8>,
    device: &str,
    now: i64,
) -> Result<Applied, ApplyError> {
    path_ok(index, &path)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Refusal::TooLarge { index, path }.into());
    }
    let cur = staged.get(&path)?;
    let live = cur.as_ref().filter(|r| r.is_live());
    let incoming = hash(&bytes);
    let unchanged = |row: &FileRow| Applied {
        path: row.path.clone(),
        rev: row.rev,
        hash: row.hash.clone(),
        merged: false,
        unchanged: true,
    };
    if let Some(row) = live {
        if row.hash.as_deref() == Some(incoming.as_str()) {
            return Ok(unchanged(row));
        }
    }
    let (bytes, merged) = match (pre, live) {
        (Pre::Any, _) | (Pre::Absent, None) => (bytes, false),
        (Pre::Rev(base), Some(row)) if row.rev == base => (bytes, false),
        (Pre::Rev(base), Some(row)) if base < row.rev && Kind::of(&path).merges() => {
            match merge_from(staged, &path, base, row, &bytes).await? {
                Some(m) => (m, true),
                None => return Err(conflict(index, &path, live)),
            }
        }
        _ => return Err(conflict(index, &path, live)),
    };
    if live.is_none() {
        if let Some(existing) = staged.collision(&path, None)? {
            return Err(Refusal::CaseCollision { index, path, existing }.into());
        }
    }
    let h = hash(&bytes);
    if merged && live.and_then(|r| r.hash.as_deref()) == Some(h.as_str()) {
        // The incoming change was already in the current text.
        return Ok(Applied { merged: true, ..unchanged(live.unwrap()) });
    }
    staged.store.blob_put(&h, &bytes).await?;
    let row = FileRow {
        path: path.clone(),
        rev: cur.as_ref().map_or(1, |r| r.rev + 1),
        hash: Some(h.clone()),
        size: bytes.len() as u64,
        device: device.to_string(),
        at: now,
    };
    let applied = Applied { path: path.clone(), rev: row.rev, hash: Some(h.clone()), merged, unchanged: false };
    let change = NewChange {
        op: Op::Put,
        path,
        rev: row.rev,
        hash: Some(h),
        size: Some(row.size),
        from: None,
        device: device.to_string(),
        at: now,
    };
    staged.stage(row, change);
    Ok(applied)
}

/// Merge `incoming` (written from revision `base`) into the current text.
/// `None` when the base is gone from history or the edits touch.
async fn merge_from<S: Store>(
    staged: &Staged<'_, S>,
    path: &str,
    base: Rev,
    current: &FileRow,
    incoming: &[u8],
) -> Result<Option<Vec<u8>>, StoreError> {
    let Some(base_hash) = staged.store.version(path, base)?.and_then(|v| v.hash) else { return Ok(None) };
    let Some(base_bytes) = staged.store.blob_get(&base_hash).await? else { return Ok(None) };
    let cur_hash = current.hash.as_deref().unwrap_or_default();
    let Some(cur_bytes) = staged.store.blob_get(cur_hash).await? else { return Ok(None) };
    Ok(merge3(&base_bytes, &cur_bytes, incoming))
}

fn delete<S: Store>(
    staged: &mut Staged<'_, S>,
    index: usize,
    path: String,
    pre: Pre,
    device: &str,
    now: i64,
) -> Result<Applied, ApplyError> {
    path_ok(index, &path)?;
    let cur = staged.get(&path)?;
    let Some(row) = cur.filter(FileRow::is_live) else {
        return Err(Refusal::NotFound { index, path }.into());
    };
    match pre {
        Pre::Any => {}
        Pre::Rev(r) if r == row.rev => {}
        Pre::Rev(_) => return Err(conflict(index, &path, Some(&row))),
        Pre::Absent => return Err(Refusal::BadRequest { reason: format!("{path}: delete needs If-Match") }.into()),
    }
    let tomb = FileRow { rev: row.rev + 1, hash: None, size: 0, device: device.to_string(), at: now, ..row };
    let applied = Applied { path: path.clone(), rev: tomb.rev, hash: None, merged: false, unchanged: false };
    let change = NewChange {
        op: Op::Delete,
        path,
        rev: tomb.rev,
        hash: None,
        size: None,
        from: None,
        device: device.to_string(),
        at: now,
    };
    staged.stage(tomb, change);
    Ok(applied)
}

fn move_file<S: Store>(
    staged: &mut Staged<'_, S>,
    index: usize,
    from: String,
    to: String,
    pre: Pre,
    device: &str,
    now: i64,
) -> Result<Applied, ApplyError> {
    path_ok(index, &from)?;
    path_ok(index, &to)?;
    if from == to {
        return Err(Refusal::BadRequest { reason: format!("{from}: move onto itself") }.into());
    }
    let Some(src) = staged.get(&from)?.filter(FileRow::is_live) else {
        return Err(Refusal::NotFound { index, path: from }.into());
    };
    match pre {
        Pre::Any => {}
        Pre::Rev(r) if r == src.rev => {}
        Pre::Rev(_) => return Err(conflict(index, &from, Some(&src))),
        Pre::Absent => return Err(Refusal::BadRequest { reason: format!("{from}: move needs If-Match") }.into()),
    }
    let dest = staged.get(&to)?;
    if let Some(d) = dest.as_ref().filter(|d| d.is_live()) {
        return Err(conflict(index, &to, Some(d)));
    }
    if let Some(existing) = staged.collision(&to, Some(&from))? {
        return Err(Refusal::CaseCollision { index, path: to, existing }.into());
    }
    let tomb = FileRow { rev: src.rev + 1, hash: None, size: 0, device: device.to_string(), at: now, ..src.clone() };
    let moved = FileRow {
        path: to.clone(),
        rev: dest.as_ref().map_or(1, |d| d.rev + 1),
        hash: src.hash.clone(),
        size: src.size,
        device: device.to_string(),
        at: now,
    };
    let applied = Applied { path: to.clone(), rev: moved.rev, hash: moved.hash.clone(), merged: false, unchanged: false };
    let change = NewChange {
        op: Op::Move,
        path: to,
        rev: moved.rev,
        hash: moved.hash.clone(),
        size: Some(moved.size),
        from: Some(from),
        device: device.to_string(),
        at: now,
    };
    // Both rows land (the source's tombstone keeps its history); the feed
    // carries one change, which clients apply as remove + put.
    staged.rows.insert(tomb.path.clone(), tomb.clone());
    staged.commit.rows.push(tomb);
    staged.stage(moved, change);
    Ok(applied)
}
