//! A device's side of sync, as a state machine with no I/O: what to
//! download, remove and push, given the server's changes and what the
//! device holds. The laptop daemon drives it over a folder; cce-vault-web
//! drives it over IndexedDB (with placeholders for attachments on top).
//!
//! What a pass looks like, in host terms:
//!
//! 1. Snapshot the local files: vault path → hash, syncable paths only.
//! 2. Pull: hand [`SyncState::listing`] (first pass, or after a `reset`)
//!    or [`SyncState::changes`] the server's answer, and carry out the
//!    [`Step`]s it returns. Every step names the local hash it expects; a
//!    host that finds something else there does nothing (the file changed
//!    under it; the next pass sees the edit).
//! 3. Push: send each of [`SyncState::pushes`], and report the answer with
//!    [`SyncState::put_ok`], [`SyncState::put_refused`],
//!    [`SyncState::delete_ok`] or [`SyncState::delete_refused`], carrying
//!    out any steps those return.
//! 4. Persist the state. A crash before this re-runs the pull from the old
//!    seq, which is harmless: every step is conditional.
//!
//! The rules, each chosen so no edit is ever lost silently (cce-vault-web's
//! Dropbox engine had the same ones):
//!
//! - Every file is tracked by the revision and hash this device last
//!   agreed with the server on (its base). A remote file whose hash equals
//!   the local bytes is adopted, never downloaded.
//! - A remote change is applied only to a file not edited here. An edited
//!   file is pushed with its base revision instead: the server merges a
//!   note when the edits are apart, and refuses otherwise.
//! - Refused: the server's version takes the path and the local one is kept
//!   beside it as `Name (conflict YYYY-MM-DD HHMM).md`, which the next push
//!   uploads like any new file. Nothing is merged on the device.
//! - A remote deletion removes the local file only if it has no local
//!   edits; an edited one is re-created on the server. A local deletion is
//!   sent with the base revision, and a remote edit since wins (the file
//!   comes back).
//! - A merge the server made is downloaded only over the exact bytes that
//!   were sent. If the file changed again meanwhile, the base stays where
//!   it was, so the next push merges from the right base (the change both
//!   sides now share is taken once).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::proto::{Applied, ChangesPage, FileMeta, Listing, Op, Pre, Refusal, Rev, Seq};

/// What the device and the server last agreed a file was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub rev: Rev,
    pub hash: String,
}

/// Everything sync keeps between passes; the host stores it as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// The seq to pull changes from.
    pub seq: Seq,
    /// Whether a full listing has been taken (the first pass takes one).
    #[serde(default)]
    pub listed: bool,
    /// By vault path.
    pub files: BTreeMap<String, Base>,
}

/// What the device holds now: vault path → hash of its bytes.
pub type Local = BTreeMap<String, String>;

/// Something for the host to do to the local files. Each one is
/// conditional on the local file still having the hash it names (`None`:
/// absent); otherwise the host skips it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Fetch the blob `hash` and write it at `path`, then call
    /// [`SyncState::downloaded`].
    Download { path: String, rev: Rev, hash: String, expect: Option<String> },
    /// Remove the local file at `path`.
    Remove { path: String, expect: String },
    /// Copy the local bytes at `path` to the new path `to` (a conflict
    /// copy; the next push uploads it). Always followed by a `Download`
    /// of the server's version at `path`.
    KeepCopy { path: String, to: String, expect: String },
}

/// Something to send to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    /// Upload the local bytes (whose hash is `hash`) with this precondition.
    Put { path: String, pre: Pre, hash: String },
    /// Delete, guarded by the base revision.
    Delete { path: String, rev: Rev },
}

/// A remote file's state, from the feed or a listing.
enum Remote {
    Live { path: String, rev: Rev, hash: String },
    Gone { path: String },
}

impl Remote {
    fn path(&self) -> &str {
        match self {
            Remote::Live { path, .. } | Remote::Gone { path } => path,
        }
    }
}

impl SyncState {
    /// Files edited or deleted here and not yet pushed, for a status line.
    pub fn pending(&self, local: &Local) -> usize {
        self.pushes(local).len()
    }

    /// Take the server's full listing (the first pass, or after the feed
    /// said `reset`). A tracked file the listing lacks was deleted there.
    pub fn listing(&mut self, listing: &Listing, local: &Local) -> Vec<Step> {
        let mut remote: Vec<Remote> = listing
            .files
            .iter()
            .map(|f| Remote::Live { path: f.path.clone(), rev: f.rev, hash: f.hash.clone() })
            .collect();
        let listed: std::collections::HashSet<&str> = listing.files.iter().map(|f| f.path.as_str()).collect();
        remote.extend(
            self.files.keys().filter(|p| !listed.contains(p.as_str())).map(|p| Remote::Gone { path: p.clone() }),
        );
        self.seq = listing.seq;
        self.listed = true;
        self.pull(remote, local)
    }

    /// [`listing`](Self::listing), for catching up a folder that is known
    /// to be newer than the server: the first pass after an import, when
    /// the folder kept changing (Obsidian, Dropbox) before the daemon ran.
    /// A local file that differs from the server's and has no base takes
    /// the server's current revision as its base, so the push replaces the
    /// server's version (which stays in history) instead of the default:
    /// keeping the local text as a conflict copy, the server's at the path.
    /// Only for a pass the user asks for; never automatic.
    pub fn listing_preferring_local(&mut self, listing: &Listing, local: &Local) -> Vec<Step> {
        for f in &listing.files {
            if !self.files.contains_key(&f.path) && local.get(&f.path).is_some_and(|h| *h != f.hash) {
                self.files.insert(f.path.clone(), Base { rev: f.rev, hash: f.hash.clone() });
            }
        }
        self.listing(listing, local)
    }

    /// Take a page of the change feed. The caller handles `reset` (by
    /// listing instead) and `more` (by asking again from the new seq).
    pub fn changes(&mut self, page: &ChangesPage, local: &Local) -> Vec<Step> {
        // Within a page only a path's last state matters; applying earlier
        // ones would download bytes about to be replaced.
        let mut last: BTreeMap<String, Remote> = BTreeMap::new();
        let mut note = |r: Remote| {
            last.insert(r.path().to_string(), r);
        };
        for c in &page.changes {
            match (c.op, &c.hash) {
                (Op::Put, Some(h)) => note(Remote::Live { path: c.path.clone(), rev: c.rev, hash: h.clone() }),
                (Op::Move, Some(h)) => {
                    if let Some(from) = &c.from {
                        note(Remote::Gone { path: from.clone() });
                    }
                    note(Remote::Live { path: c.path.clone(), rev: c.rev, hash: h.clone() });
                }
                _ => note(Remote::Gone { path: c.path.clone() }),
            }
        }
        self.seq = page.seq;
        self.pull(last.into_values().collect(), local)
    }

    fn pull(&mut self, remote: Vec<Remote>, local: &Local) -> Vec<Step> {
        let mut steps = Vec::new();
        for r in remote {
            match r {
                Remote::Live { path, rev, hash } => {
                    let here = local.get(&path);
                    if here == Some(&hash) {
                        self.files.insert(path, Base { rev, hash });
                    } else if here == self.files.get(&path).map(|b| &b.hash) {
                        // Not edited here (or new to both): take it.
                        steps.push(Step::Download { path, rev, hash, expect: here.cloned() });
                    }
                    // Otherwise edited here: the push merges or conflicts.
                }
                Remote::Gone { path } => {
                    let base = self.files.remove(&path);
                    if let (Some(h), Some(b)) = (local.get(&path), base) {
                        if *h == b.hash {
                            steps.push(Step::Remove { path, expect: h.clone() });
                        }
                    }
                    // Edited here, or never synced: it stays, and the push
                    // creates it again.
                }
            }
        }
        steps
    }

    /// A download (or a conflict's server version) was written.
    pub fn downloaded(&mut self, path: &str, rev: Rev, hash: &str) {
        self.files.insert(path.to_string(), Base { rev, hash: hash.to_string() });
    }

    /// What to send: every local file whose bytes differ from its base, and
    /// every tracked file gone from here. In path order, puts first.
    pub fn pushes(&self, local: &Local) -> Vec<Push> {
        let mut out = Vec::new();
        for (path, hash) in local {
            let pre = match self.files.get(path) {
                Some(b) if b.hash == *hash => continue,
                Some(b) => Pre::Rev(b.rev),
                None => Pre::Absent,
            };
            out.push(Push::Put { path: path.clone(), pre, hash: hash.clone() });
        }
        for (path, b) in &self.files {
            if !local.contains_key(path) {
                out.push(Push::Delete { path: path.clone(), rev: b.rev });
            }
        }
        out
    }

    /// The server took a put of bytes hashing to `sent`. A merge comes
    /// back as a download of the merged bytes over exactly what was sent.
    pub fn put_ok(&mut self, path: &str, sent: &str, applied: &Applied) -> Option<Step> {
        let hash = applied.hash.clone().unwrap_or_default();
        if applied.merged && hash != sent {
            // The base moves only once the merged bytes are here (see the
            // module doc).
            return Some(Step::Download {
                path: path.to_string(),
                rev: applied.rev,
                hash,
                expect: Some(sent.to_string()),
            });
        }
        self.downloaded(path, applied.rev, &hash);
        None
    }

    /// The server refused a put of bytes hashing to `sent`. `now` is local
    /// time as `YYYY-MM-DD HHMM`; `taken` says whether a path is in use
    /// here, for naming a conflict copy.
    pub fn put_refused(
        &mut self,
        path: &str,
        sent: &str,
        refusal: &Refusal,
        now: &str,
        taken: impl Fn(&str) -> bool,
    ) -> Vec<Step> {
        match refusal {
            Refusal::Conflict { current: Some(cur), .. } => self.conflict(path, sent, cur, now, taken),
            Refusal::Conflict { current: None, .. } => {
                // Deleted (or never there) on the server: the edit wins, and
                // the next push creates the file again.
                self.files.remove(path);
                Vec::new()
            }
            // Case collisions, bad paths, too large: nothing to do here;
            // the host reports them and the file stays local.
            _ => Vec::new(),
        }
    }

    fn conflict(&mut self, path: &str, sent: &str, cur: &FileMeta, now: &str, taken: impl Fn(&str) -> bool) -> Vec<Step> {
        if cur.hash == sent {
            self.downloaded(path, cur.rev, &cur.hash);
            return Vec::new();
        }
        let to = conflict_name(path, now, |p| taken(p) || self.files.contains_key(p));
        vec![
            Step::KeepCopy { path: path.to_string(), to, expect: sent.to_string() },
            Step::Download { path: path.to_string(), rev: cur.rev, hash: cur.hash.clone(), expect: Some(sent.to_string()) },
        ]
    }

    /// A delete was done, or found the file already gone.
    pub fn delete_ok(&mut self, path: &str) {
        self.files.remove(path);
    }

    /// The server refused a delete: the file changed there since the base,
    /// and the edit wins. It comes back.
    pub fn delete_refused(&mut self, path: &str, refusal: &Refusal) -> Vec<Step> {
        self.files.remove(path);
        match refusal {
            Refusal::Conflict { current: Some(cur), .. } => vec![Step::Download {
                path: path.to_string(),
                rev: cur.rev,
                hash: cur.hash.clone(),
                expect: None,
            }],
            _ => Vec::new(),
        }
    }
}

/// `dir/Name (conflict 2026-10-13 1430).md`, numbered if taken.
pub fn conflict_name(path: &str, now: &str, taken: impl Fn(&str) -> bool) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let base = format!("{dir}{stem} (conflict {now})");
    let mut candidate = format!("{base}{ext}");
    let mut n = 2;
    while taken(&candidate) {
        candidate = format!("{base} {n}{ext}");
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on_ready as run;
    use crate::core::{ApplyError, Operation, Store, Vault};
    use crate::mem::MemStore;
    use crate::proto::hash;

    /// A device: its files, its sync state, and how many writes its passes
    /// made to the files.
    #[derive(Default)]
    struct Device {
        name: &'static str,
        files: BTreeMap<String, Vec<u8>>,
        state: SyncState,
        writes: usize,
    }

    /// The server, driven the way the daemon drives it.
    struct World {
        server: Vault<MemStore>,
        clock: i64,
    }

    impl Device {
        fn new(name: &'static str) -> Device {
            Device { name, ..Device::default() }
        }

        fn local(&self) -> Local {
            self.files.iter().map(|(p, b)| (p.clone(), hash(b))).collect()
        }

        fn text(&self, path: &str) -> Option<&str> {
            self.files.get(path).map(|b| std::str::from_utf8(b).unwrap())
        }

        fn write(&mut self, path: &str, text: &str) {
            self.files.insert(path.to_string(), text.as_bytes().to_vec());
        }

        fn holds(&self, path: &str, expect: &Option<String>) -> bool {
            self.files.get(path).map(|b| hash(b)) == *expect
        }

        fn carry_out(&mut self, w: &World, steps: Vec<Step>) {
            for step in steps {
                match step {
                    Step::Download { path, rev, hash: h, expect } => {
                        if self.holds(&path, &expect) {
                            let bytes = run(w.server.store.blob_get(&h)).unwrap().unwrap();
                            self.files.insert(path.clone(), bytes);
                            self.writes += 1;
                            self.state.downloaded(&path, rev, &h);
                        }
                    }
                    Step::Remove { path, expect } => {
                        if self.holds(&path, &Some(expect)) {
                            self.files.remove(&path);
                            self.writes += 1;
                        }
                    }
                    Step::KeepCopy { path, to, expect } => {
                        if self.holds(&path, &Some(expect)) {
                            let bytes = self.files[&path].clone();
                            self.files.insert(to, bytes);
                            self.writes += 1;
                        }
                    }
                }
            }
        }

        /// One pass: pull, then push until nothing is left (a refused
        /// create comes back as a new push).
        fn pass(&mut self, w: &mut World) {
            if !self.state.listed {
                let listing = w.server.listing().unwrap();
                let steps = self.state.listing(&listing, &self.local());
                self.carry_out(w, steps);
            } else {
                loop {
                    let page = w.server.changes(self.state.seq, 2).unwrap();
                    if page.reset {
                        self.state.listed = false;
                        return self.pass(w);
                    }
                    let steps = self.state.changes(&page, &self.local());
                    self.carry_out(w, steps);
                    if !page.more {
                        break;
                    }
                }
            }
            for _ in 0..3 {
                let pushes = self.state.pushes(&self.local());
                if pushes.is_empty() {
                    break;
                }
                for push in pushes {
                    w.clock += 1;
                    let steps = match push {
                        Push::Put { path, pre, hash: sent } => {
                            let bytes = self.files[&path].clone();
                            let op = Operation::Put { path: path.clone(), pre, bytes };
                            match run(w.server.apply(vec![op], self.name, w.clock)) {
                                Ok(c) => self.state.put_ok(&path, &sent, &c.applied[0]).into_iter().collect(),
                                Err(ApplyError::Refused(r)) => {
                                    let files = &self.files;
                                    self.state.put_refused(&path, &sent, &r, "2026-10-13 1200", |p| files.contains_key(p))
                                }
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                        Push::Delete { path, rev } => {
                            let op = Operation::Delete { path: path.clone(), pre: Pre::Rev(rev) };
                            match run(w.server.apply(vec![op], self.name, w.clock)) {
                                Ok(_) | Err(ApplyError::Refused(Refusal::NotFound { .. })) => {
                                    self.state.delete_ok(&path);
                                    Vec::new()
                                }
                                Err(ApplyError::Refused(r)) => self.state.delete_refused(&path, &r),
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                    };
                    self.carry_out(w, steps);
                }
            }
        }
    }

    fn world() -> World {
        World { server: Vault::new(MemStore::default()), clock: 0 }
    }

    /// Both devices pass until neither has anything to send; then both
    /// must hold exactly what the server holds.
    fn settle(w: &mut World, a: &mut Device, b: &mut Device) {
        for _ in 0..3 {
            a.pass(w);
            b.pass(w);
        }
        assert!(a.state.pushes(&a.local()).is_empty(), "{} has pending pushes", a.name);
        assert!(b.state.pushes(&b.local()).is_empty(), "{} has pending pushes", b.name);
        assert_eq!(a.files, b.files, "devices differ");
        let server: BTreeMap<String, Vec<u8>> = w
            .server
            .listing()
            .unwrap()
            .files
            .iter()
            .map(|f| (f.path.clone(), run(w.server.store.blob_get(&f.hash)).unwrap().unwrap()))
            .collect();
        assert_eq!(a.files, server, "devices differ from the server");
    }

    #[test]
    fn files_reach_the_other_device() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("Note.md", "hello\n");
        a.write("Tasks/To Do.md", "- [ ] x\n");
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("Note.md"), Some("hello\n"));
        a.write("Note.md", "hello again\n");
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("Note.md"), Some("hello again\n"));
    }

    #[test]
    fn a_first_pass_over_identical_files_writes_nothing() {
        let (mut w, mut a) = (world(), Device::new("import"));
        a.write("A.md", "a\n");
        a.write("pic.jpg", "\u{1}\u{2}");
        a.pass(&mut w);
        // The laptop's folder already holds exactly what was imported.
        let mut laptop = Device::new("laptop");
        laptop.files = a.files.clone();
        laptop.pass(&mut w);
        assert_eq!(laptop.writes, 0);
        assert_eq!(w.server.store.seq, 2, "nothing pushed either");
        assert!(laptop.state.pushes(&laptop.local()).is_empty());
    }

    #[test]
    fn a_folder_newer_than_its_import_can_win_on_request() {
        let mut w = world();
        let mut import = Device::new("import");
        import.write("Same.md", "same\n");
        import.write("Edited.md", "as imported\n");
        import.pass(&mut w);
        // The folder kept changing before the daemon first ran.
        let folder = |d: &mut Device| {
            d.write("Same.md", "same\n");
            d.write("Edited.md", "edited since the import\n");
        };

        // By default an unknown difference is a conflict: both kept.
        let mut cautious = Device::new("laptop");
        folder(&mut cautious);
        let mut w2 = world();
        import.state = SyncState::default();
        import.pass(&mut w2);
        cautious.pass(&mut w2);
        assert_eq!(cautious.text("Edited.md"), Some("as imported\n"));
        assert!(cautious.files.keys().any(|p| p.starts_with("Edited (conflict")));

        // Asked to prefer the folder, it replaces the server's copy.
        let mut laptop = Device::new("laptop");
        folder(&mut laptop);
        let steps = laptop.state.listing_preferring_local(&w.server.listing().unwrap(), &laptop.local());
        laptop.carry_out(&w, steps);
        laptop.pass(&mut w);
        assert_eq!(laptop.writes, 0, "nothing in the folder was touched");
        assert_eq!(laptop.files.len(), 2, "no conflict copy");
        let (meta, bytes) = run(w.server.read("Edited.md")).unwrap().unwrap();
        assert_eq!((meta.rev, bytes.as_slice()), (2, &b"edited since the import\n"[..]));
        assert_eq!(w.server.history("Edited.md").unwrap().versions.len(), 2, "the imported text stays in history");
        assert!(laptop.state.pushes(&laptop.local()).is_empty());
    }

    #[test]
    fn edits_apart_merge_on_the_server() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("N.md", "a\nb\nc\nd\ne\n");
        settle(&mut w, &mut a, &mut b);
        a.write("N.md", "A\nb\nc\nd\ne\n");
        b.write("N.md", "a\nb\nc\nd\nE\n");
        settle(&mut w, &mut a, &mut b);
        assert_eq!(a.text("N.md"), Some("A\nb\nc\nd\nE\n"));
        assert!(a.files.keys().all(|p| !p.contains("conflict")));
    }

    #[test]
    fn touching_edits_keep_a_conflict_copy() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("Daily/D.md", "# Day\n");
        settle(&mut w, &mut a, &mut b);
        a.write("Daily/D.md", "# Day\n- laptop\n");
        b.write("Daily/D.md", "# Day\n- phone\n");
        settle(&mut w, &mut a, &mut b);
        // The laptop pushed first; the phone's text is kept beside it.
        assert_eq!(a.text("Daily/D.md"), Some("# Day\n- laptop\n"));
        assert_eq!(a.text("Daily/D (conflict 2026-10-13 1200).md"), Some("# Day\n- phone\n"));
    }

    #[test]
    fn a_remote_delete_removes_only_unedited_files() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("Gone.md", "x\n");
        a.write("Kept.md", "y\n");
        settle(&mut w, &mut a, &mut b);
        a.files.remove("Gone.md");
        a.files.remove("Kept.md");
        b.write("Kept.md", "y, edited on the phone\n");
        a.pass(&mut w);
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("Gone.md"), None);
        assert_eq!(a.text("Kept.md"), Some("y, edited on the phone\n"), "the edit came back");
    }

    #[test]
    fn a_local_delete_loses_to_a_remote_edit() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("N.md", "one\n");
        settle(&mut w, &mut a, &mut b);
        a.write("N.md", "two\n");
        a.pass(&mut w);
        b.files.remove("N.md"); // deleted before seeing "two"
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("N.md"), Some("two\n"));
    }

    #[test]
    fn the_same_new_path_on_both_devices() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("Idea.md", "from the laptop\n");
        b.write("Idea.md", "from the phone\n");
        settle(&mut w, &mut a, &mut b);
        assert_eq!(a.text("Idea.md"), Some("from the laptop\n"));
        assert_eq!(a.text("Idea (conflict 2026-10-13 1200).md"), Some("from the phone\n"));
        // The same bytes created on both sides are simply adopted.
        a.write("Same.md", "s\n");
        b.write("Same.md", "s\n");
        settle(&mut w, &mut a, &mut b);
        assert!(a.files.keys().all(|p| !p.starts_with("Same (conflict")));
    }

    #[test]
    fn a_server_side_rename_moves_the_file_everywhere() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("Beta.md", "beta\n");
        a.write("Alpha.md", "see [[Beta]]\n");
        settle(&mut w, &mut a, &mut b);
        // Claude renames Beta with a link rewrite, in one batch.
        let ops = vec![
            Operation::Move { from: "Beta.md".into(), to: "Gamma.md".into(), pre: Pre::Rev(1) },
            Operation::Put { path: "Alpha.md".into(), pre: Pre::Rev(1), bytes: b"see [[Gamma]]\n".to_vec() },
        ];
        run(w.server.apply(ops, "claude", 99)).unwrap();
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("Gamma.md"), Some("beta\n"));
        assert_eq!(b.text("Beta.md"), None);
        assert_eq!(a.text("Alpha.md"), Some("see [[Gamma]]\n"));
    }

    #[test]
    fn a_merge_that_arrives_after_another_edit_still_converges() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("N.md", "1\n2\n3\n4\n5\n6\n7\n");
        settle(&mut w, &mut a, &mut b);
        b.write("N.md", "1\n2\n3\n4\n5\n6\nseven\n");
        b.pass(&mut w);
        // The laptop edits the top; the server merges. Before the merged
        // bytes are written, the laptop edits the middle too.
        a.write("N.md", "one\n2\n3\n4\n5\n6\n7\n");
        let sent = hash(b"one\n2\n3\n4\n5\n6\n7\n");
        let op = Operation::Put { path: "N.md".into(), pre: Pre::Rev(1), bytes: a.files["N.md"].clone() };
        let c = run(w.server.apply(vec![op], "laptop", 50)).unwrap();
        let step = a.state.put_ok("N.md", &sent, &c.applied[0]).unwrap();
        a.write("N.md", "one\n2\n3\nfour\n5\n6\n7\n");
        a.carry_out(&w, vec![step]); // skipped: the file changed
        assert_eq!(a.text("N.md"), Some("one\n2\n3\nfour\n5\n6\n7\n"));
        settle(&mut w, &mut a, &mut b);
        assert_eq!(a.text("N.md"), Some("one\n2\n3\nfour\n5\n6\nseven\n"), "every edit kept, none twice");
    }

    #[test]
    fn a_trimmed_feed_relists_and_converges() {
        let (mut w, mut a, mut b) = (world(), Device::new("laptop"), Device::new("phone"));
        a.write("A.md", "a\n");
        settle(&mut w, &mut a, &mut b);
        for i in 0..5 {
            a.write(&format!("N{i}.md"), "n\n");
            a.pass(&mut w);
        }
        a.files.remove("A.md");
        a.pass(&mut w);
        let seq = w.server.store.seq;
        w.server.store.trim_changes(seq - 1);
        settle(&mut w, &mut a, &mut b);
        assert_eq!(b.text("A.md"), None);
        assert_eq!(b.files.len(), 5);
    }

    #[test]
    fn conflict_names() {
        assert_eq!(conflict_name("Daily/D.md", "2026-10-13 1200", |_| false), "Daily/D (conflict 2026-10-13 1200).md");
        let taken = |p: &str| p == "a (conflict t).md";
        assert_eq!(conflict_name("a.md", "t", taken), "a (conflict t) 2.md");
        assert_eq!(conflict_name("README", "t", |_| false), "README (conflict t)");
        assert_eq!(conflict_name(".cce/vault.kdl", "t", |_| false), ".cce/vault (conflict t).kdl");
    }
}
