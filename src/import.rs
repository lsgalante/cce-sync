//! `cce-sync import <dir>`: put a vault folder on the server, the first
//! step of leaving Dropbox. Every file is created with `If-None-Match: *`,
//! so an import never overwrites anything already there: a file the server
//! has with the same bytes is skipped, one with other bytes is reported and
//! left alone.
//!
//! `.obsidian/` is not uploaded. Its three settings cce-vault reads (daily
//! notes, the attachment folder, templates) become `.cce/vault.kdl`, unless
//! the folder already has one.

use std::collections::HashMap;
use std::path::Path;

use cce_sync::proto::{hash, BatchOp, Pre, Refusal, SETTINGS_FILE};

use crate::http::{Answer, Client};
use crate::local::walk;

/// A batch's limits: base64 makes bytes a third bigger on the wire, and the
/// Worker takes 100 MB a request.
const BATCH_FILES: usize = 100;
const BATCH_BYTES: usize = 32 * 1024 * 1024;

pub fn run(client: &Client, dir: &Path, dry_run: bool) -> Result<(), String> {
    let (mut files, skipped) = collect(dir)?;
    if !files.iter().any(|(p, _)| p == SETTINGS_FILE) {
        let read = |name: &str| std::fs::read_to_string(dir.join(".obsidian").join(name)).ok();
        if let Some(kdl) = vault_kdl(read("daily-notes.json"), read("app.json"), read("templates.json")) {
            println!("writing {SETTINGS_FILE} from .obsidian/:\n{kdl}");
            files.push((SETTINGS_FILE.to_string(), kdl.into_bytes()));
        }
    }
    for s in &skipped {
        println!("skip   {s}");
    }

    let listing = client.listing()?;
    let remote: HashMap<&str, &str> = listing.files.iter().map(|f| (f.path.as_str(), f.hash.as_str())).collect();
    let mut todo = Vec::new();
    let (mut same, mut differ) = (0, Vec::new());
    for (path, bytes) in files {
        match remote.get(path.as_str()) {
            Some(h) if *h == hash(&bytes) => same += 1,
            Some(_) => differ.push(path),
            None => todo.push((path, bytes)),
        }
    }
    for p in &differ {
        println!("differs on the server, left alone: {p}");
    }
    let total: usize = todo.iter().map(|(_, b)| b.len()).sum();
    println!("{} to upload ({total} bytes), {same} already there, {} differ", todo.len(), differ.len());
    if dry_run || todo.is_empty() {
        return Ok(());
    }

    let mut uploaded = 0;
    for chunk in batches(todo) {
        let ops: Vec<BatchOp> = chunk.iter().map(|(p, b)| BatchOp::put(p.clone(), Pre::Absent, b)).collect();
        match client.batch(ops)? {
            Answer::Done(c) => {
                uploaded += c.applied.len();
                println!("uploaded {uploaded} (seq {})", c.seq);
            }
            Answer::Refused(r) => return Err(refusal(&r, &chunk)),
        }
    }
    println!("done: {uploaded} files");
    Ok(())
}

fn refusal(r: &Refusal, chunk: &[(String, Vec<u8>)]) -> String {
    match r {
        Refusal::CaseCollision { path, existing, .. } => {
            format!("{path} differs only in case from {existing} on the server; rename one and run again")
        }
        Refusal::Conflict { path, .. } => format!("{path} appeared on the server during the import; run again"),
        other => format!("refused ({} files in that batch, none written): {other:?}", chunk.len()),
    }
}

/// Files as (vault path, bytes).
type Files = Vec<(String, Vec<u8>)>;

/// Every syncable file under `dir` with its bytes, and the names skipped
/// as invalid (hidden paths other than `.cce/` are ignored silently, as
/// cce-vault ignores them).
fn collect(dir: &Path) -> Result<(Files, Vec<String>), String> {
    let (paths, skipped) = walk(dir)?;
    let mut files = Vec::with_capacity(paths.len());
    for rel in paths {
        let bytes = std::fs::read(dir.join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
        files.push((rel, bytes));
    }
    Ok((files, skipped))
}

fn batches(files: Files) -> Vec<Files> {
    let mut out: Vec<Files> = Vec::new();
    let mut size = 0;
    for f in files {
        let full = out.last().is_none_or(|b| b.len() >= BATCH_FILES || size + f.1.len() > BATCH_BYTES);
        if full {
            out.push(Vec::new());
            size = 0;
        }
        size += f.1.len();
        out.last_mut().unwrap().push(f);
    }
    out
}

fn kdl_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `.cce/vault.kdl` from Obsidian's three settings files, or `None` when
/// none of them sets anything.
fn vault_kdl(daily: Option<String>, app: Option<String>, templates: Option<String>) -> Option<String> {
    let json = |s: Option<String>| s.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let field = |v: &Option<serde_json::Value>, k: &str| {
        v.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let (daily, app, templates) = (json(daily), json(app), json(templates));
    let mut out = String::new();
    let daily_lines: Vec<String> = [("folder", "folder"), ("format", "format"), ("template", "template")]
        .iter()
        .filter_map(|(json_key, kdl_key)| field(&daily, json_key).map(|v| format!("    {kdl_key} {}\n", kdl_string(&v))))
        .collect();
    if !daily_lines.is_empty() {
        out.push_str("daily {\n");
        out.extend(daily_lines);
        out.push_str("}\n");
    }
    if let Some(folder) = field(&app, "attachmentFolderPath") {
        out.push_str(&format!("attachments {{ folder {}; }}\n", kdl_string(&folder)));
    }
    if let Some(folder) = field(&templates, "folder") {
        out.push_str(&format!("templates {{ folder {}; }}\n", kdl_string(&folder)));
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_translate_to_kdl() {
        let kdl = vault_kdl(
            Some(r#"{"folder":"Daily","format":"YYYY-MM-DD","template":"Templates/Day"}"#.into()),
            Some(r#"{"attachmentFolderPath":"./"}"#.into()),
            Some(r#"{"folder":"Templates"}"#.into()),
        )
        .unwrap();
        assert_eq!(
            kdl,
            "daily {\n    folder \"Daily\"\n    format \"YYYY-MM-DD\"\n    template \"Templates/Day\"\n}\n\
             attachments { folder \"./\"; }\ntemplates { folder \"Templates\"; }\n"
        );
        assert_eq!(vault_kdl(None, Some("{}".into()), Some("not json".into())), None);
        assert_eq!(vault_kdl(None, Some(r#"{"attachmentFolderPath":"a\"b"}"#.into()), None).unwrap(), "attachments { folder \"a\\\"b\"; }\n");
    }

    #[test]
    fn collect_skips_hidden_folders_but_keeps_settings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for (rel, body) in [
            ("Note.md", "x"),
            ("Tasks/To Do.md", "- [ ] y"),
            (".obsidian/app.json", "{}"),
            (".git/HEAD", "ref"),
            (".cce/vault.kdl", "daily {}"),
            ("Notes/.draft.md", "z"),
        ] {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        }
        let (files, skipped) = collect(p).unwrap();
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec![".cce/vault.kdl", "Note.md", "Tasks/To Do.md"]);
        assert!(skipped.is_empty(), "hidden files are ignored, not reported: {skipped:?}");
    }

    #[test]
    fn batches_respect_both_limits() {
        let files: Vec<(String, Vec<u8>)> = (0..250).map(|i| (format!("{i}.md"), vec![0; 10])).collect();
        let b = batches(files);
        assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), vec![100, 100, 50]);
        let big = vec![("a".into(), vec![0; BATCH_BYTES]), ("b".into(), vec![0; 1])];
        assert_eq!(batches(big).len(), 2);
    }
}
