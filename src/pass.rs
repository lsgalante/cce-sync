//! One sync pass over the vault folder: pull the server's changes, carry
//! out the client's steps, push until nothing is left. The rules are
//! `cce_sync::client`'s; this is the I/O around them.

use cce_sync::client::{Push, Step, SyncState};
use cce_sync::proto::{hash, Pre, Refusal};
use serde::{Deserialize, Serialize};

use crate::http::{Answer, Client};
use crate::local::{scan, Cache, Folder};

/// What the daemon keeps between passes, as JSON under
/// `~/.local/state/cce/sync/`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    pub server: String,
    pub vault: String,
    pub sync: SyncState,
    #[serde(default)]
    pub cache: Cache,
}

/// What a pass did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub downloaded: usize,
    pub removed: usize,
    pub uploaded: usize,
    pub merged: usize,
    pub deleted: usize,
    /// (path, where this device's version was kept).
    pub conflicts: Vec<(String, String)>,
    /// Files the server would not take (case collisions, bad names, too
    /// large), with why.
    pub refused: Vec<String>,
    /// Local names that cannot sync.
    pub skipped: Vec<String>,
    /// Edits still to push after the pass.
    pub pending: usize,
}

impl Report {
    pub fn wrote_anything(&self) -> bool {
        self.downloaded + self.removed + self.conflicts.len() > 0
    }
}

/// Push rounds per pass: a refused create comes back as a new push, and a
/// conflict copy is pushed in the round after it was written.
const ROUNDS: usize = 4;

pub fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H%M").to_string()
}

pub fn run(http: &Client, folder: &Folder, saved: &mut Saved) -> Result<Report, String> {
    let mut report = Report::default();
    let (local, skipped) = scan(&folder.root, &mut saved.cache)?;
    report.skipped = skipped;

    // Pull.
    if !saved.sync.listed {
        let listing = http.listing()?;
        let steps = saved.sync.listing(&listing, &local);
        carry_out(http, folder, &mut saved.sync, steps, &mut report)?;
    } else {
        let mut local = local;
        loop {
            let page = http.changes(saved.sync.seq)?;
            if page.reset {
                saved.sync.listed = false;
                return run(http, folder, saved);
            }
            let steps = saved.sync.changes(&page, &local);
            if !steps.is_empty() {
                carry_out(http, folder, &mut saved.sync, steps, &mut report)?;
                local = scan(&folder.root, &mut saved.cache)?.0;
            }
            if !page.more {
                break;
            }
        }
    }

    // Push.
    for _ in 0..ROUNDS {
        let (local, _) = scan(&folder.root, &mut saved.cache)?;
        let pushes = saved.sync.pushes(&local);
        if pushes.is_empty() {
            break;
        }
        let mut progressed = false;
        for push in pushes {
            match push {
                Push::Put { path, pre, hash: sent } => {
                    let Ok(bytes) = folder.read(&path) else { continue };
                    if hash(&bytes) != sent {
                        continue; // changed since the scan; the next round sees it
                    }
                    match http.put(&path, pre, &bytes)? {
                        Answer::Done(applied) => {
                            progressed = true;
                            report.uploaded += usize::from(!applied.unchanged);
                            report.merged += usize::from(applied.merged);
                            let step = saved.sync.put_ok(&path, &sent, &applied);
                            carry_out(http, folder, &mut saved.sync, step.into_iter().collect(), &mut report)?;
                        }
                        Answer::Refused(r) => {
                            if !matches!(r, Refusal::Conflict { .. }) {
                                report.refused.push(refusal_text(&path, &r));
                                continue;
                            }
                            progressed = true;
                            let steps =
                                saved.sync.put_refused(&path, &sent, &r, &now_stamp(), |p| folder.exists(p));
                            carry_out(http, folder, &mut saved.sync, steps, &mut report)?;
                        }
                    }
                }
                Push::Delete { path, rev } => {
                    progressed = true;
                    match http.delete(&path, rev)? {
                        Answer::Done(_) | Answer::Refused(Refusal::NotFound { .. }) => {
                            report.deleted += 1;
                            saved.sync.delete_ok(&path);
                        }
                        Answer::Refused(r) => {
                            let steps = saved.sync.delete_refused(&path, &r);
                            carry_out(http, folder, &mut saved.sync, steps, &mut report)?;
                        }
                    }
                }
            }
        }
        if !progressed {
            break;
        }
    }
    let (local, _) = scan(&folder.root, &mut saved.cache)?;
    report.pending = saved
        .sync
        .pushes(&local)
        .iter()
        .filter(|p| !matches!(p, Push::Put { path, .. } if report.refused.iter().any(|r| r.starts_with(path.as_str()))))
        .count();
    Ok(report)
}

fn refusal_text(path: &str, r: &Refusal) -> String {
    match r {
        Refusal::CaseCollision { existing, .. } => format!("{path}: differs only in case from {existing}; rename one"),
        Refusal::BadPath { reason, .. } => format!("{path}: {reason}"),
        Refusal::TooLarge { .. } => format!("{path}: over 100 MB"),
        other => format!("{path}: {other:?}"),
    }
}

/// Do the steps, each only if the file still has the hash it expects.
fn carry_out(
    http: &Client,
    folder: &Folder,
    sync: &mut SyncState,
    steps: Vec<Step>,
    report: &mut Report,
) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Download { path, rev, hash: h, expect } => {
                if !folder.holds(&path, expect.as_deref()) {
                    continue;
                }
                let bytes = http.blob(&h)?;
                if hash(&bytes) != h {
                    return Err(format!("blob {h} for {path} did not match its hash"));
                }
                // Checked again right before the write: a download can take
                // a while, and an edit made meanwhile must not be replaced.
                if !folder.holds(&path, expect.as_deref()) {
                    continue;
                }
                folder.write(&path, &bytes).map_err(|e| format!("{path}: {e}"))?;
                sync.downloaded(&path, rev, &h);
                report.downloaded += 1;
            }
            Step::Remove { path, expect } => {
                if folder.holds(&path, Some(&expect)) {
                    folder.remove(&path).map_err(|e| format!("{path}: {e}"))?;
                    report.removed += 1;
                }
            }
            Step::KeepCopy { path, to, expect } => {
                let Ok(bytes) = folder.read(&path) else { continue };
                if hash(&bytes) != expect {
                    continue;
                }
                folder.write(&to, &bytes).map_err(|e| format!("{to}: {e}"))?;
                report.conflicts.push((path, to));
            }
        }
    }
    Ok(())
}

/// Restore a file to an earlier revision: its bytes become a new revision
/// (history is never rewritten). The daemon brings it to the folder.
pub fn restore(http: &Client, path: &str, rev: u64) -> Result<u64, String> {
    let history = http.history(path)?;
    let want = history.versions.iter().find(|v| v.rev == rev).ok_or(format!("{path} has no revision {rev}"))?;
    let h = want.hash.clone().ok_or(format!("revision {rev} of {path} is its deletion"))?;
    let bytes = http.blob(&h)?;
    let pre = match history.versions.first() {
        Some(cur) if cur.hash.is_some() => Pre::Rev(cur.rev),
        _ => Pre::Absent,
    };
    match http.put(path, pre, &bytes)? {
        Answer::Done(a) => Ok(a.rev),
        Answer::Refused(r) => Err(format!("refused: {r:?} (it changed meanwhile; run again)")),
    }
}
