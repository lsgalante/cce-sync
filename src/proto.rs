//! The wire protocol between cce-sync clients and the server: the JSON
//! shapes, the headers, and the rules every side checks paths and bytes
//! with. Spelled once here so the server, the laptop daemon and the phone
//! cannot disagree.
//!
//! Endpoints (all under `/v1`, every request with `Authorization: Bearer`):
//!
//! | Method and path | Body / answer |
//! | --- | --- |
//! | `GET changes?since=<seq>&limit=<n>` | [`ChangesPage`] |
//! | `GET files` | [`Listing`] |
//! | `GET files/<path>` | the bytes; [`H_REV`], [`H_HASH`] |
//! | `PUT files/<path>` | the bytes, with `If-Match: <rev>` or `If-None-Match: *` → [`Committed`] |
//! | `DELETE files/<path>` | `If-Match: <rev>` → [`Committed`] |
//! | `POST batch` | [`BatchRequest`] → [`Committed`] |
//! | `GET history/<path>` | [`History`] |
//! | `GET blobs/<sha256>` | the bytes |
//! | `GET live` | WebSocket; the server sends [`LiveNotice`] after each commit |
//! | `GET status` | [`Status`] |
//!
//! A refused write answers `409` with [`Refusal`]. Paths in URLs are
//! percent-encoded per segment ([`encode_path`]).

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A file's revision: 1 when created, +1 on every change, never reused
/// (a deletion is a revision too, so a stale `If-Match` always fails).
pub type Rev = u64;
/// The vault-wide change counter: every committed change gets the next one.
pub type Seq = u64;

/// The revision a write is conditional on.
pub const H_REV: &str = "x-cce-rev";
/// SHA-256 of a file's bytes, lowercase hex.
pub const H_HASH: &str = "x-cce-hash";
/// The device making a request (`laptop`, `phone`, `claude`, …). Recorded
/// on every change so a client can skip its own entries in the feed.
pub const H_DEVICE: &str = "x-cce-device";

/// The largest file the server takes: the request-body limit of a
/// Cloudflare Free or Pro account.
pub const MAX_FILE_BYTES: usize = 100 * 1024 * 1024;
/// The longest path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// The one hidden folder that syncs: the vault's own settings.
pub const SETTINGS_DIR: &str = ".cce";
/// The vault settings file, KDL.
pub const SETTINGS_FILE: &str = ".cce/vault.kdl";

/// SHA-256 of `bytes`, lowercase hex: the hash every side compares.
pub fn hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Whether `s` looks like a [`hash`]: 64 lowercase hex digits.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What a vault file is, by its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// `.md`: merged three-way on conflicting writes.
    Note,
    /// `.canvas` (JSON Canvas).
    Canvas,
    /// Under `.cce/`.
    Settings,
    /// Anything else: images, PDFs, `.base` files.
    Attachment,
}

impl Kind {
    pub fn of(path: &str) -> Kind {
        if path.starts_with(".cce/") {
            return Kind::Settings;
        }
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".md") {
            Kind::Note
        } else if lower.ends_with(".canvas") {
            Kind::Canvas
        } else {
            Kind::Attachment
        }
    }

    /// Whether a conflicting write to this kind is merged as text.
    pub fn merges(self) -> bool {
        matches!(self, Kind::Note)
    }
}

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    TooLong,
    /// Leading or trailing `/`, `//`, `.` or `..` segments, `\`, or a
    /// control character.
    Malformed,
    /// A hidden segment (one starting with `.`) outside `.cce/`.
    Hidden,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PathError::Empty => "empty path",
            PathError::TooLong => "path longer than 1024 bytes",
            PathError::Malformed => "malformed path",
            PathError::Hidden => "hidden paths do not sync (only .cce/ does)",
        })
    }
}

/// Check a vault path: relative, `/`-separated, no empty, `.` or `..`
/// segments, nothing hidden except the settings folder. cce-vault's
/// "hidden means ignored" rule, with `.cce/` the one exception.
pub fn check_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if path.chars().any(|c| c.is_control() || c == '\\') {
        return Err(PathError::Malformed);
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err(PathError::Malformed);
    }
    for (i, s) in segments.iter().enumerate() {
        if s.starts_with('.') && !(i == 0 && *s == SETTINGS_DIR && segments.len() > 1) {
            return Err(PathError::Hidden);
        }
    }
    Ok(())
}

/// The case-folded form two paths must not share (link resolution is
/// case-insensitive, so `Beta.md` and `beta.md` would be ambiguous).
pub fn fold(path: &str) -> String {
    path.to_lowercase()
}

/// Percent-encode a vault path for a URL, keeping the `/` separators.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Undo [`encode_path`] (any percent-encoding). `None` on a bad escape or
/// non-UTF-8 result.
pub fn decode_path(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A live file as the server lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    pub rev: Rev,
    pub hash: String,
    pub size: u64,
    pub kind: Kind,
}

/// What a change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Put,
    Delete,
    /// `from` → `path`, same bytes.
    Move,
}

/// One entry of the change feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub seq: Seq,
    pub op: Op,
    pub path: String,
    /// The file's revision after the change (the tombstone's, for a delete).
    pub rev: Rev,
    /// The new bytes' hash; `None` for a delete.
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The old path of a move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub device: String,
    /// Milliseconds since the Unix epoch, server clock.
    pub at: i64,
}

/// `GET changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesPage {
    pub changes: Vec<Change>,
    /// The seq to ask from next time: the last change's, or the vault's
    /// current seq when there are none.
    pub seq: Seq,
    /// More changes follow; ask again from `seq`.
    pub more: bool,
    /// `since` is older than the trimmed feed: re-list with `GET files`.
    pub reset: bool,
}

/// `GET files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub files: Vec<FileMeta>,
    /// The vault's seq when listed: pull changes from here.
    pub seq: Seq,
}

/// One revision of a file, as history lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub path: String,
    pub rev: Rev,
    /// `None` for the revision that deleted the file.
    pub hash: Option<String>,
    pub size: u64,
    pub device: String,
    pub at: i64,
}

/// `GET history/<path>`, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub path: String,
    pub versions: Vec<Version>,
}

/// What a write is conditional on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "rev")]
pub enum Pre {
    /// The file must not exist (`If-None-Match: *`).
    Absent,
    /// The file must be at this revision (`If-Match: <rev>`), or, for a
    /// note, mergeable from it.
    Rev(Rev),
    /// Unconditional: only for the server's own writers (Claude's tools
    /// apply to the current text). HTTP clients must send one of the others.
    Any,
}

/// One operation of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum BatchOp {
    Put {
        path: String,
        pre: Pre,
        /// The bytes, base64 (standard alphabet, padded).
        b64: String,
    },
    Delete {
        path: String,
        pre: Pre,
    },
    Move {
        from: String,
        to: String,
        pre: Pre,
    },
}

impl BatchOp {
    pub fn put(path: impl Into<String>, pre: Pre, bytes: &[u8]) -> BatchOp {
        BatchOp::Put { path: path.into(), pre, b64: b64_encode(bytes) }
    }
}

/// `POST batch`: all land, or none do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub ops: Vec<BatchOp>,
}

/// What one committed operation became.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub path: String,
    pub rev: Rev,
    pub hash: Option<String>,
    /// The server merged this write with a change made since its base;
    /// the client must download the result (`hash` is the merged bytes').
    #[serde(default)]
    pub merged: bool,
    /// Nothing changed: the bytes were already there (a retried write).
    #[serde(default)]
    pub unchanged: bool,
}

/// The answer to a successful write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committed {
    pub applied: Vec<Applied>,
    /// The vault's seq after the commit.
    pub seq: Seq,
}

/// Why a write was refused (HTTP 409 for conflicts, 400 for the rest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Refusal {
    /// The precondition failed and no merge was possible. `current` is the
    /// file as it stands (`None`: it does not exist or was deleted).
    Conflict { index: usize, path: String, current: Option<FileMeta> },
    /// Another live file has this path in a different case.
    CaseCollision { index: usize, path: String, existing: String },
    /// A delete or move of a file that does not exist.
    NotFound { index: usize, path: String },
    BadPath { index: usize, path: String, reason: String },
    TooLarge { index: usize, path: String },
    BadRequest { reason: String },
}

impl Refusal {
    /// The HTTP status a server answers it with.
    pub fn status(&self) -> u16 {
        match self {
            Refusal::Conflict { .. } | Refusal::CaseCollision { .. } => 409,
            Refusal::NotFound { .. } => 404,
            Refusal::TooLarge { .. } => 413,
            Refusal::BadPath { .. } | Refusal::BadRequest { .. } => 400,
        }
    }
}

/// `GET status`: what the vault holds, against the budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub seq: Seq,
    pub files: u64,
    /// Bytes of notes, canvases and settings: what the server holds in memory.
    pub text_bytes: u64,
    pub attachment_bytes: u64,
    /// The server's storage, history included.
    pub database_bytes: u64,
    /// `text_bytes` is past [`TEXT_WARN_BYTES`].
    pub warn: bool,
}

/// The note-text budget: cce-vault's 5,000-note benchmark, measured to fit.
pub const TEXT_BUDGET_BYTES: u64 = 40 * 1024 * 1024;
/// Where `status` starts warning.
pub const TEXT_WARN_BYTES: u64 = 30 * 1024 * 1024;

impl Status {
    pub fn of(listing: &Listing, database_bytes: u64) -> Status {
        let (mut text, mut attach) = (0, 0);
        for f in &listing.files {
            match f.kind {
                Kind::Attachment => attach += f.size,
                _ => text += f.size,
            }
        }
        Status {
            seq: listing.seq,
            files: listing.files.len() as u64,
            text_bytes: text,
            attachment_bytes: attach,
            database_bytes,
            warn: text > TEXT_WARN_BYTES,
        }
    }
}

/// Sent on the live socket after each commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveNotice {
    pub seq: Seq,
}

pub fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Parse an `If-Match` / `If-None-Match` pair into a [`Pre`]. HTTP clients
/// must send one: an unconditional write from a device is refused, since
/// it is how edits get lost.
pub fn pre_from_headers(if_match: Option<&str>, if_none_match: Option<&str>) -> Option<Pre> {
    match (if_match.map(str::trim), if_none_match.map(str::trim)) {
        (Some(m), None) => m.trim_matches('"').parse().ok().map(Pre::Rev),
        (None, Some("*")) => Some(Pre::Absent),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(check_path("Daily/2026-10-13.md").is_ok());
        assert!(check_path(".cce/vault.kdl").is_ok());
        assert!(check_path("Notes/Ünïcode & spaces.md").is_ok());
        assert_eq!(check_path(""), Err(PathError::Empty));
        assert_eq!(check_path("/abs.md"), Err(PathError::Malformed));
        assert_eq!(check_path("a//b.md"), Err(PathError::Malformed));
        assert_eq!(check_path("a/../b.md"), Err(PathError::Malformed));
        assert_eq!(check_path("dir/"), Err(PathError::Malformed));
        assert_eq!(check_path("a\\b.md"), Err(PathError::Malformed));
        assert_eq!(check_path(".obsidian/app.json"), Err(PathError::Hidden));
        assert_eq!(check_path("Notes/.hidden.md"), Err(PathError::Hidden));
        assert_eq!(check_path(".cce"), Err(PathError::Hidden), "the folder itself is not a file");
        assert_eq!(check_path("x/.cce/vault.kdl"), Err(PathError::Hidden));
    }

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("a/B.MD"), Kind::Note);
        assert_eq!(Kind::of("Board.canvas"), Kind::Canvas);
        assert_eq!(Kind::of(".cce/vault.kdl"), Kind::Settings);
        assert_eq!(Kind::of("pic.jpg"), Kind::Attachment);
        assert_eq!(Kind::of("Untitled.base"), Kind::Attachment);
    }

    #[test]
    fn path_encoding_round_trips() {
        for p in ["Daily/2026-10-13.md", "Tasks/To Do.md", "Ünï/c%de#?.md"] {
            let e = encode_path(p);
            assert!(!e.contains(' ') && !e.contains('#') && !e.contains('?'));
            assert_eq!(decode_path(&e).as_deref(), Some(p));
        }
        assert_eq!(decode_path("%zz"), None);
    }

    #[test]
    fn hashes() {
        let h = hash(b"");
        assert_eq!(h, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert!(is_hash(&h));
        assert!(!is_hash("E3B0"));
    }

    #[test]
    fn preconditions() {
        assert_eq!(pre_from_headers(Some("7"), None), Some(Pre::Rev(7)));
        assert_eq!(pre_from_headers(Some("\"7\""), None), Some(Pre::Rev(7)));
        assert_eq!(pre_from_headers(None, Some("*")), Some(Pre::Absent));
        assert_eq!(pre_from_headers(None, None), None);
        assert_eq!(pre_from_headers(Some("7"), Some("*")), None);
    }

    #[test]
    fn wire_shapes() {
        let op = BatchOp::put("a.md", Pre::Rev(3), b"hi");
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(json, r#"{"op":"put","path":"a.md","pre":{"kind":"rev","rev":3},"b64":"aGk="}"#);
        let r = Refusal::Conflict { index: 0, path: "a.md".into(), current: None };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"error":"conflict","index":0,"path":"a.md","current":null}"#);
    }
}
