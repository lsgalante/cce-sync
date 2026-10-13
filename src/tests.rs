//! Scenarios for the server's rules ([`crate::core`]) over the in-memory
//! store. A sync rule change gets a scenario here first.

use crate::block_on_ready as run;
use crate::core::{ApplyError, Operation, Vault};
use crate::mem::MemStore;
use crate::proto::{hash, Committed, Op, Pre, Refusal};

fn vault() -> Vault<MemStore> {
    Vault::new(MemStore::default())
}

fn put(path: &str, pre: Pre, text: &str) -> Operation {
    Operation::Put { path: path.into(), pre, bytes: text.as_bytes().to_vec() }
}

fn apply(v: &mut Vault<MemStore>, device: &str, ops: Vec<Operation>) -> Result<Committed, Refusal> {
    match run(v.apply(ops, device, 1_000)) {
        Ok(c) => Ok(c),
        Err(ApplyError::Refused(r)) => Err(r),
        Err(ApplyError::Store(e)) => panic!("{e}"),
    }
}

fn text(v: &Vault<MemStore>, path: &str) -> Option<String> {
    run(v.read(path)).unwrap().map(|(_, b)| String::from_utf8(b).unwrap())
}

#[test]
fn create_then_update_with_the_right_rev() {
    let mut v = vault();
    let c = apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "one\n")]).unwrap();
    assert_eq!((c.applied[0].rev, c.seq), (1, 1));
    let c = apply(&mut v, "laptop", vec![put("a.md", Pre::Rev(1), "two\n")]).unwrap();
    assert_eq!((c.applied[0].rev, c.seq), (2, 2));
    assert_eq!(text(&v, "a.md").as_deref(), Some("two\n"));
}

#[test]
fn creating_an_existing_file_conflicts() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "one\n")]).unwrap();
    let r = apply(&mut v, "phone", vec![put("a.md", Pre::Absent, "other\n")]).unwrap_err();
    assert!(matches!(r, Refusal::Conflict { ref current, .. } if current.as_ref().unwrap().rev == 1), "{r:?}");
}

#[test]
fn writing_the_same_bytes_again_is_unchanged() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "one\n")]).unwrap();
    // A retried create, and a stale If-Match carrying bytes already there.
    for pre in [Pre::Absent, Pre::Rev(0)] {
        let c = apply(&mut v, "laptop", vec![put("a.md", pre, "one\n")]).unwrap();
        assert!(c.applied[0].unchanged);
        assert_eq!((c.applied[0].rev, c.seq), (1, 1), "no new revision, no change");
    }
}

#[test]
fn stale_note_write_merges_when_edits_are_apart() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("n.md", Pre::Absent, "a\nb\nc\nd\ne\n")]).unwrap();
    apply(&mut v, "laptop", vec![put("n.md", Pre::Rev(1), "A\nb\nc\nd\ne\n")]).unwrap();
    // The phone still has rev 1 and edited the last line.
    let c = apply(&mut v, "phone", vec![put("n.md", Pre::Rev(1), "a\nb\nc\nd\nE\n")]).unwrap();
    assert!(c.applied[0].merged);
    assert_eq!(c.applied[0].rev, 3);
    assert_eq!(text(&v, "n.md").as_deref(), Some("A\nb\nc\nd\nE\n"));
    assert_eq!(c.applied[0].hash.as_deref(), Some(hash(b"A\nb\nc\nd\nE\n").as_str()));
}

#[test]
fn stale_note_write_conflicts_when_edits_touch() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("n.md", Pre::Absent, "# Day\n")]).unwrap();
    apply(&mut v, "laptop", vec![put("n.md", Pre::Rev(1), "# Day\n- laptop\n")]).unwrap();
    let r = apply(&mut v, "phone", vec![put("n.md", Pre::Rev(1), "# Day\n- phone\n")]).unwrap_err();
    assert!(matches!(r, Refusal::Conflict { ref current, .. } if current.as_ref().unwrap().rev == 2));
    assert_eq!(text(&v, "n.md").as_deref(), Some("# Day\n- laptop\n"), "nothing written");
}

#[test]
fn stale_attachment_write_never_merges() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("b.canvas", Pre::Absent, "{}\n\n\n\n")]).unwrap();
    apply(&mut v, "laptop", vec![put("b.canvas", Pre::Rev(1), "{1}\n\n\n\n")]).unwrap();
    let r = apply(&mut v, "phone", vec![put("b.canvas", Pre::Rev(1), "{}\n\n\n\n2")]).unwrap_err();
    assert!(matches!(r, Refusal::Conflict { .. }));
}

#[test]
fn delete_needs_the_current_rev_and_keeps_the_counter() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "one\n")]).unwrap();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Rev(1), "two\n")]).unwrap();
    // A delete from a device that only saw rev 1 loses to the newer edit.
    let r = apply(&mut v, "phone", vec![Operation::Delete { path: "a.md".into(), pre: Pre::Rev(1) }]).unwrap_err();
    assert!(matches!(r, Refusal::Conflict { .. }));
    let c = apply(&mut v, "laptop", vec![Operation::Delete { path: "a.md".into(), pre: Pre::Rev(2) }]).unwrap();
    assert_eq!(c.applied[0].rev, 3);
    assert_eq!(text(&v, "a.md"), None);
    // An old If-Match can never succeed against the re-created file.
    let r = apply(&mut v, "phone", vec![put("a.md", Pre::Rev(2), "zombie\n")]).unwrap_err();
    assert!(matches!(r, Refusal::Conflict { current: None, .. }));
    let c = apply(&mut v, "phone", vec![put("a.md", Pre::Absent, "again\n")]).unwrap();
    assert_eq!(c.applied[0].rev, 4);
    let r = apply(&mut v, "phone", vec![Operation::Delete { path: "gone.md".into(), pre: Pre::Rev(1) }]).unwrap_err();
    assert!(matches!(r, Refusal::NotFound { .. }));
}

#[test]
fn case_collisions_are_refused() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("Notes/Beta.md", Pre::Absent, "b\n")]).unwrap();
    let r = apply(&mut v, "phone", vec![put("notes/beta.md", Pre::Absent, "x\n")]).unwrap_err();
    assert!(matches!(r, Refusal::CaseCollision { ref existing, .. } if existing == "Notes/Beta.md"), "{r:?}");
    // Renaming a file to a new case of its own name is fine.
    let c = apply(
        &mut v,
        "laptop",
        vec![Operation::Move { from: "Notes/Beta.md".into(), to: "Notes/beta.md".into(), pre: Pre::Rev(1) }],
    )
    .unwrap();
    assert_eq!(c.applied[0].path, "Notes/beta.md");
    // And deleting in the same batch frees the folded name.
    apply(
        &mut v,
        "laptop",
        vec![
            Operation::Delete { path: "Notes/beta.md".into(), pre: Pre::Rev(1) },
            put("NOTES/BETA.md", Pre::Absent, "y\n"),
        ],
    )
    .unwrap();
}

#[test]
fn a_batch_lands_whole_or_not_at_all() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "a\n")]).unwrap();
    let r = apply(
        &mut v,
        "claude",
        vec![put("b.md", Pre::Absent, "b\n"), put("a.md", Pre::Rev(9), "stale\n")],
    )
    .unwrap_err();
    assert!(matches!(r, Refusal::Conflict { index: 1, .. }));
    assert_eq!(text(&v, "b.md"), None, "the first op did not land");
    assert_eq!(v.store.seq, 1);
}

#[test]
fn a_rename_with_link_rewrites_is_one_commit() {
    let mut v = vault();
    apply(
        &mut v,
        "laptop",
        vec![put("Beta.md", Pre::Absent, "beta\n"), put("Alpha.md", Pre::Absent, "see [[Beta]]\n")],
    )
    .unwrap();
    let c = apply(
        &mut v,
        "laptop",
        vec![
            Operation::Move { from: "Beta.md".into(), to: "Gamma.md".into(), pre: Pre::Rev(1) },
            put("Alpha.md", Pre::Rev(1), "see [[Gamma]]\n"),
        ],
    )
    .unwrap();
    assert_eq!(c.seq, 4);
    let page = v.changes(2, 100).unwrap();
    assert_eq!(page.changes.iter().map(|c| c.op).collect::<Vec<_>>(), vec![Op::Move, Op::Put]);
    assert_eq!(page.changes[0].from.as_deref(), Some("Beta.md"));
    assert_eq!(text(&v, "Gamma.md").as_deref(), Some("beta\n"));
    assert_eq!(text(&v, "Beta.md"), None);
    let listed: Vec<String> = v.listing().unwrap().files.into_iter().map(|f| f.path).collect();
    assert_eq!(listed, vec!["Alpha.md", "Gamma.md"]);
}

#[test]
fn moving_onto_a_live_file_conflicts() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "a\n"), put("b.md", Pre::Absent, "b\n")]).unwrap();
    let r = apply(&mut v, "laptop", vec![Operation::Move { from: "a.md".into(), to: "b.md".into(), pre: Pre::Rev(1) }])
        .unwrap_err();
    assert!(matches!(r, Refusal::Conflict { ref path, .. } if path == "b.md"));
}

#[test]
fn the_feed_pages_and_resets() {
    let mut v = vault();
    for i in 0..5 {
        apply(&mut v, "laptop", vec![put(&format!("n{i}.md"), Pre::Absent, "x\n")]).unwrap();
    }
    let p = v.changes(0, 2).unwrap();
    assert_eq!((p.changes.len(), p.seq, p.more, p.reset), (2, 2, true, false));
    let p = v.changes(p.seq, 10).unwrap();
    assert_eq!((p.changes.len(), p.seq, p.more), (3, 5, false));
    let p = v.changes(5, 10).unwrap();
    assert_eq!((p.changes.len(), p.seq, p.more, p.reset), (0, 5, false, false));
    v.store.trim_changes(3);
    assert!(v.changes(1, 10).unwrap().reset, "seq 2..3 were trimmed");
    assert!(!v.changes(3, 10).unwrap().reset, "everything after 3 is still there");
    assert!(v.changes(99, 10).unwrap().reset, "a client ahead of the server re-lists");
}

#[test]
fn history_keeps_every_revision_and_deletes() {
    let mut v = vault();
    apply(&mut v, "laptop", vec![put("a.md", Pre::Absent, "one\n")]).unwrap();
    apply(&mut v, "claude", vec![put("a.md", Pre::Rev(1), "two\n")]).unwrap();
    apply(&mut v, "phone", vec![Operation::Delete { path: "a.md".into(), pre: Pre::Rev(2) }]).unwrap();
    let h = v.history("a.md").unwrap();
    let revs: Vec<(u64, bool, &str)> = h.versions.iter().map(|x| (x.rev, x.hash.is_some(), x.device.as_str())).collect();
    assert_eq!(revs, vec![(3, false, "phone"), (2, true, "claude"), (1, true, "laptop")]);
    let old = h.versions[2].hash.clone().unwrap();
    assert_eq!(run(v.store_blob(&old)).as_deref(), Some(&b"one\n"[..]));
}

#[test]
fn bad_paths_and_preconditions_are_refused() {
    let mut v = vault();
    let r = apply(&mut v, "laptop", vec![put(".obsidian/app.json", Pre::Absent, "{}")]).unwrap_err();
    assert!(matches!(r, Refusal::BadPath { .. }));
    apply(&mut v, "laptop", vec![put(".cce/vault.kdl", Pre::Absent, "daily {}\n")]).unwrap();
    let r = apply(&mut v, "laptop", vec![Operation::Delete { path: ".cce/vault.kdl".into(), pre: Pre::Absent }])
        .unwrap_err();
    assert!(matches!(r, Refusal::BadRequest { .. }));
}

impl Vault<MemStore> {
    async fn store_blob(&self, hash: &str) -> Option<Vec<u8>> {
        use crate::core::Store;
        self.store.blob_get(hash).await.unwrap()
    }
}
