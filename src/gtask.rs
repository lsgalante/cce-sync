//! `#gtask`: vault tasks promoted to Google Tasks and kept in step both
//! ways (the bridge, phase 6). No I/O: the server lists Google's tasks,
//! reads the notes, asks [`plan`] what to do, and carries it out.
//!
//! A task line tagged `#gtask` is promoted: the bridge creates a Google
//! task from it and writes the Google id onto the line as
//! `[gtask:: <id>]`. The id ties the two for good; nothing is ever matched
//! by text. Then, per pass:
//!
//! - **Title, due date and done state sync both ways** against a base (the
//!   values both sides last agreed on). A field changed on one side only is
//!   copied to the other. Changed on both since the base, the later change
//!   wins (the note's last write against Google's `updated`); the losing
//!   note text stays in the note's history.
//! - **Deleted in Google:** the line keeps the task but loses `#gtask` and
//!   its id.
//! - **The line deleted in the vault** (no line anywhere carries the id):
//!   the Google task is deleted.
//! - **The tag removed but the id kept:** the link is dropped (the id goes
//!   from the line), and the Google task stays.
//! - **The same id on two lines** (a copied line): the first keeps it; the
//!   copies lose the id and, still tagged, are promoted as tasks of their own.
//! - Google tasks made in Google with no line stay in Google.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The tag that promotes a task.
pub const TAG: &str = "#gtask";
/// The due-date marker the vault's tasks use.
pub const DUE: &str = "📅";

/// What both sides agree a task is (also a Google task's synced fields).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields {
    pub title: String,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub due: Option<String>,
    pub done: bool,
}

/// A task line that takes part: tagged, or carrying an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    /// 0-based.
    pub line: usize,
    /// The line as it stands, for finding it again before an edit.
    pub raw: String,
    pub fields: Fields,
    pub tagged: bool,
    pub id: Option<String>,
}

/// A Google task, as the bridge needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub fields: Fields,
    pub deleted: bool,
    /// Google's `updated`, ms since the epoch.
    pub updated_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Make a Google task from the line, then [`Act::Attach`] its id.
    Create { path: String, raw: String, fields: Fields },
    /// Write `[gtask:: id]` onto the line `raw` in `path` (a task made
    /// earlier whose id did not reach the note), and take `fields` as base.
    Attach { path: String, raw: String, id: String, fields: Fields },
    /// Set the Google task's fields.
    Push { id: String, fields: Fields },
    /// Rewrite the line carrying `id` with these fields.
    Pull { path: String, id: String, fields: Fields },
    /// Drop `#gtask` and the id from the line (deleted in Google).
    Untag { path: String, id: String },
    /// Drop the id from the line; the Google task stays (the tag was
    /// removed, or the line is a copy).
    Unlink { path: String, raw: String, id: String },
    /// Delete the Google task (its line is gone).
    Delete { id: String },
    /// Record the agreed fields for `id`.
    SetBase { id: String, fields: Fields },
    /// Forget `id`'s base.
    Forget { id: String },
}

/// The participating task lines of one note: tagged `#gtask`, or carrying
/// a `[gtask:: id]`. Lines inside fenced code are not tasks.
pub fn scan(path: &str, text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for (i, raw) in text.lines().enumerate() {
        let t = raw.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
            continue;
        }
        let Some(task) = parse(raw) else { continue };
        if task.tagged || task.id.is_some() {
            out.push(Line { path: path.to_string(), line: i, raw: raw.to_string(), fields: task.fields, tagged: task.tagged, id: task.id });
        }
    }
    out
}

struct Parsed {
    prefix: String,
    status: char,
    fields: Fields,
    tagged: bool,
    id: Option<String>,
}

/// A task line: `<indent><- or * or + or 1.> [<status>] <text>`.
fn parse(raw: &str) -> Option<Parsed> {
    let indent = raw.len() - raw.trim_start().len();
    let rest = &raw[indent..];
    let bullet_len = if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        2
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || !(rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ")) {
            return None;
        }
        digits + 2
    };
    let after = &rest[bullet_len..];
    let mut chars = after.chars();
    if chars.next() != Some('[') {
        return None;
    }
    let status = chars.next()?;
    if chars.next() != Some(']') {
        return None;
    }
    let text = &after[2 + status.len_utf8()..];
    if !(text.is_empty() || text.starts_with(' ')) {
        return None;
    }
    let (fields, tagged, id) = read_text(text, status);
    Some(Parsed { prefix: raw[..indent + bullet_len].to_string(), status, fields, tagged, id })
}

/// The task text's fields, with the markers taken out of the title.
fn read_text(text: &str, status: char) -> (Fields, bool, Option<String>) {
    let mut rest = text.to_string();
    let mut id = None;
    if let Some(start) = rest.find("[gtask::") {
        if let Some(len) = rest[start..].find(']') {
            id = Some(rest[start + 8..start + len].trim().to_string()).filter(|s| !s.is_empty());
            rest.replace_range(start..start + len + 1, "");
        }
    }
    let mut due = None;
    let mut words = Vec::new();
    let mut tagged = false;
    let mut it = rest.split_whitespace().peekable();
    while let Some(w) = it.next() {
        if w == TAG {
            tagged = true;
        } else if w == DUE && it.peek().is_some_and(|d| crate::agenda::parse_date(d).is_some()) {
            due = it.next().map(str::to_string);
        } else {
            words.push(w);
        }
    }
    let done = matches!(status, 'x' | 'X' | '-');
    (Fields { title: words.join(" "), due, done }, tagged, id)
}

/// `raw` with these fields, keeping its indent, bullet, tag and id (and a
/// status like `/` while done does not change).
pub fn rewrite(raw: &str, fields: &Fields) -> String {
    let Some(p) = parse(raw) else { return raw.to_string() };
    let status = if p.fields.done == fields.done { p.status } else if fields.done { 'x' } else { ' ' };
    render(&p.prefix, status, fields, p.tagged, p.id.as_deref())
}

/// `raw` with `[gtask:: id]` added.
pub fn attach(raw: &str, id: &str) -> String {
    let Some(p) = parse(raw) else { return raw.to_string() };
    render(&p.prefix, p.status, &p.fields, p.tagged, Some(id))
}

/// `raw` without `#gtask` and the id.
pub fn untag(raw: &str) -> String {
    let Some(p) = parse(raw) else { return raw.to_string() };
    render(&p.prefix, p.status, &p.fields, false, None)
}

/// `raw` without the id (the tag stays).
pub fn unlink(raw: &str) -> String {
    let Some(p) = parse(raw) else { return raw.to_string() };
    render(&p.prefix, p.status, &p.fields, p.tagged, None)
}

fn render(prefix: &str, status: char, f: &Fields, tagged: bool, id: Option<&str>) -> String {
    let mut s = format!("{prefix}[{status}] {}", f.title);
    if let Some(d) = &f.due {
        s.push_str(&format!(" {DUE} {d}"));
    }
    if tagged {
        s.push_str(&format!(" {TAG}"));
    }
    if let Some(id) = id {
        s.push_str(&format!(" [gtask:: {id}]"));
    }
    s
}

/// What to do this pass. `note_ms` is each note's last write (ms);
/// `pending` holds tasks created in Google whose id never reached their
/// line, by (path, line text).
pub fn plan(
    lines: &[Line],
    note_ms: &dyn Fn(&str) -> i64,
    remote: &BTreeMap<String, Remote>,
    bases: &BTreeMap<String, Fields>,
    pending: &BTreeMap<(String, String), String>,
) -> Vec<Act> {
    let mut acts = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for l in lines {
        match &l.id {
            None => {
                // Tagged, not yet in Google (scan only returns tagged lines
                // without an id).
                match pending.get(&(l.path.clone(), l.raw.clone())) {
                    Some(id) => acts.push(Act::Attach { path: l.path.clone(), raw: l.raw.clone(), id: id.clone(), fields: l.fields.clone() }),
                    None => acts.push(Act::Create { path: l.path.clone(), raw: l.raw.clone(), fields: l.fields.clone() }),
                }
            }
            Some(id) if !seen.insert(id.as_str()) => {
                acts.push(Act::Unlink { path: l.path.clone(), raw: l.raw.clone(), id: id.clone() });
            }
            Some(id) if !l.tagged => {
                acts.push(Act::Unlink { path: l.path.clone(), raw: l.raw.clone(), id: id.clone() });
                acts.push(Act::Forget { id: id.clone() });
            }
            Some(id) => match remote.get(id) {
                None => {
                    acts.push(Act::Untag { path: l.path.clone(), id: id.clone() });
                    acts.push(Act::Forget { id: id.clone() });
                }
                Some(r) if r.deleted => {
                    acts.push(Act::Untag { path: l.path.clone(), id: id.clone() });
                    acts.push(Act::Forget { id: id.clone() });
                }
                Some(r) => {
                    let base = bases.get(id).cloned().unwrap_or_else(|| r.fields.clone());
                    let ours_later = note_ms(&l.path) >= r.updated_ms;
                    let merged = merge(&l.fields, &r.fields, &base, ours_later);
                    if merged != r.fields {
                        acts.push(Act::Push { id: id.clone(), fields: merged.clone() });
                    }
                    if merged != l.fields {
                        acts.push(Act::Pull { path: l.path.clone(), id: id.clone(), fields: merged.clone() });
                    }
                    if bases.get(id) != Some(&merged) {
                        acts.push(Act::SetBase { id: id.clone(), fields: merged });
                    }
                }
            },
        }
    }
    for id in bases.keys() {
        if seen.contains(id.as_str()) || lines.iter().any(|l| l.id.as_deref() == Some(id)) {
            continue;
        }
        if remote.get(id).is_some_and(|r| !r.deleted) {
            acts.push(Act::Delete { id: id.clone() });
        }
        acts.push(Act::Forget { id: id.clone() });
    }
    acts
}

/// Field by field: one side changed since the base, take it; both, the later.
fn merge(ours: &Fields, theirs: &Fields, base: &Fields, ours_later: bool) -> Fields {
    fn pick<T: Clone + PartialEq>(o: &T, t: &T, b: &T, ours_later: bool) -> T {
        if o == b {
            t.clone()
        } else if t == b || ours_later {
            o.clone()
        } else {
            t.clone()
        }
    }
    Fields {
        title: pick(&ours.title, &theirs.title, &base.title, ours_later),
        due: pick(&ours.due, &theirs.due, &base.due, ours_later),
        done: pick(&ours.done, &theirs.done, &base.done, ours_later),
    }
}

/// A Google Tasks API task (JSON) as a [`Remote`], with its id.
pub fn remote_from_json(v: &serde_json::Value) -> Option<(String, Remote)> {
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str);
    let id = s("id")?.to_string();
    let fields = Fields {
        title: s("title").unwrap_or_default().trim().to_string(),
        due: s("due").and_then(|d| d.get(..10)).map(str::to_string),
        done: s("status") == Some("completed"),
    };
    let deleted = v.get("deleted").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let updated_ms = s("updated").and_then(crate::agenda::rfc3339_ms).unwrap_or(0);
    Some((id, Remote { fields, deleted, updated_ms }))
}

/// The JSON body that sets a Google task to `fields` (create or patch).
/// `notes` goes in on create only.
pub fn google_body(fields: &Fields, notes: Option<&str>) -> serde_json::Value {
    let mut b = serde_json::json!({
        "title": fields.title,
        "status": if fields.done { "completed" } else { "needsAction" },
        "due": fields.due.as_ref().map(|d| format!("{d}T00:00:00.000Z")),
    });
    if !fields.done {
        b["completed"] = serde_json::Value::Null;
    }
    if let Some(n) = notes {
        b["notes"] = n.into();
    }
    b
}

/// The text of `path` with each line edit applied: `edits` maps a line's
/// current text (found by id for an id, else exactly) to its new text. A
/// line that cannot be found is skipped, and the edit reported as not
/// applied (the note changed under the pass; the next one retries).
pub fn edit_note(text: &str, edits: &[LineEdit]) -> (String, Vec<bool>) {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let mut done = Vec::new();
    for e in edits {
        let at = match &e.find {
            Find::Id(id) => lines.iter().position(|l| parse(l).is_some_and(|p| p.id.as_deref() == Some(id.as_str()))),
            Find::Exact(raw) => lines.iter().position(|l| l == raw),
        };
        match at {
            Some(i) => {
                lines[i] = (e.with)(&lines[i]);
                done.push(true);
            }
            None => done.push(false),
        }
    }
    (lines.join("\n"), done)
}

/// One line edit for [`edit_note`].
pub struct LineEdit {
    pub find: Find,
    pub with: Box<dyn Fn(&str) -> String>,
}

pub enum Find {
    Id(String),
    Exact(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(title: &str, due: Option<&str>, done: bool) -> Fields {
        Fields { title: title.into(), due: due.map(str::to_string), done }
    }

    #[test]
    fn lines_parse_and_render() {
        let text = "# Day\n- [ ] Call the studio 📅 2026-10-20 #gtask\n  * [x] Book flights #gtask [gtask:: abc]\n- [ ] not promoted\n```\n- [ ] in code #gtask\n```\n1. [/] Draft #work #gtask\n- [ ] untagged [gtask:: old]";
        let ls = scan("D.md", text);
        assert_eq!(ls.len(), 4);
        assert_eq!((ls[0].line, &ls[0].fields, ls[0].tagged, &ls[0].id), (1, &f("Call the studio", Some("2026-10-20"), false), true, &None));
        assert_eq!((&ls[1].fields, ls[1].id.as_deref()), (&f("Book flights", None, true), Some("abc")));
        assert_eq!((ls[2].line, &ls[2].fields.title), (7, &"Draft #work".to_string()));
        assert!(!ls[3].tagged && ls[3].id.as_deref() == Some("old"));

        assert_eq!(attach(&ls[0].raw, "g1"), "- [ ] Call the studio 📅 2026-10-20 #gtask [gtask:: g1]");
        assert_eq!(rewrite(&ls[1].raw, &f("Book the flights", Some("2026-11-01"), true)), "  * [x] Book the flights 📅 2026-11-01 #gtask [gtask:: abc]");
        assert_eq!(rewrite(&ls[2].raw, &f("Draft #work", None, false)), "1. [/] Draft #work #gtask", "a status kept while done is unchanged");
        assert_eq!(rewrite(&ls[2].raw, &f("Draft #work", None, true)), "1. [x] Draft #work #gtask");
        assert_eq!(untag(&ls[1].raw), "  * [x] Book flights");
        assert_eq!(unlink(&ls[1].raw), "  * [x] Book flights #gtask");
        // Rendering what was parsed gives the line back.
        assert_eq!(rewrite(&ls[0].raw, &ls[0].fields), ls[0].raw);
    }

    fn remote(fields: Fields, updated_ms: i64) -> Remote {
        Remote { fields, deleted: false, updated_ms }
    }

    fn one(raw: &str) -> Vec<Line> {
        scan("N.md", raw)
    }

    #[test]
    fn a_tagged_line_is_created_once() {
        let (none, no_base) = (BTreeMap::new(), BTreeMap::new());
        let acts = plan(&one("- [ ] Call #gtask"), &|_| 0, &none, &no_base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Create { path: "N.md".into(), raw: "- [ ] Call #gtask".into(), fields: f("Call", None, false) }]);
        // Created, but the id did not reach the note: adopt, don't create again.
        let pending = BTreeMap::from([(("N.md".to_string(), "- [ ] Call #gtask".to_string()), "g1".to_string())]);
        let acts = plan(&one("- [ ] Call #gtask"), &|_| 0, &none, &no_base, &pending);
        assert!(matches!(&acts[0], Act::Attach { id, .. } if id == "g1"));
    }

    #[test]
    fn fields_sync_both_ways_and_the_later_wins_a_clash() {
        let base = BTreeMap::from([("g".to_string(), f("Call", None, false))]);
        // Ticked in the vault: Google follows.
        let acts = plan(&one("- [x] Call #gtask [gtask:: g]"), &|_| 10, &BTreeMap::from([("g".into(), remote(f("Call", None, false), 5))]), &base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Push { id: "g".into(), fields: f("Call", None, true) }, Act::SetBase { id: "g".into(), fields: f("Call", None, true) }]);
        // Due date set in Google: the line follows.
        let acts = plan(&one("- [ ] Call #gtask [gtask:: g]"), &|_| 10, &BTreeMap::from([("g".into(), remote(f("Call", Some("2026-10-20"), false), 5))]), &base, &BTreeMap::new());
        assert_eq!(acts[0], Act::Pull { path: "N.md".into(), id: "g".into(), fields: f("Call", Some("2026-10-20"), false) });
        // Different fields on each side: both kept.
        let acts = plan(&one("- [x] Call #gtask [gtask:: g]"), &|_| 10, &BTreeMap::from([("g".into(), remote(f("Call mum", None, false), 5))]), &base, &BTreeMap::new());
        assert_eq!(acts[0], Act::Push { id: "g".into(), fields: f("Call mum", None, true) });
        assert_eq!(acts[1], Act::Pull { path: "N.md".into(), id: "g".into(), fields: f("Call mum", None, true) });
        // The same field on both: the later wins.
        let r = BTreeMap::from([("g".into(), remote(f("Call Bob", None, false), 50))]);
        let acts = plan(&one("- [ ] Call Ann #gtask [gtask:: g]"), &|_| 10, &r, &base, &BTreeMap::new());
        assert_eq!(acts[0], Act::Pull { path: "N.md".into(), id: "g".into(), fields: f("Call Bob", None, false) });
        let acts = plan(&one("- [ ] Call Ann #gtask [gtask:: g]"), &|_| 99, &r, &base, &BTreeMap::new());
        assert_eq!(acts[0], Act::Push { id: "g".into(), fields: f("Call Ann", None, false) });
        // In step: nothing to do.
        let acts = plan(&one("- [ ] Call #gtask [gtask:: g]"), &|_| 10, &BTreeMap::from([("g".into(), remote(f("Call", None, false), 5))]), &base, &BTreeMap::new());
        assert!(acts.is_empty());
    }

    #[test]
    fn deletions_and_unlinking() {
        let base = BTreeMap::from([("g".to_string(), f("Call", None, false))]);
        let live = BTreeMap::from([("g".into(), remote(f("Call", None, false), 5))]);
        // Deleted in Google: the line keeps the task, untagged.
        let gone = BTreeMap::from([("g".into(), Remote { deleted: true, ..remote(f("Call", None, false), 5) })]);
        let acts = plan(&one("- [ ] Call #gtask [gtask:: g]"), &|_| 0, &gone, &base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Untag { path: "N.md".into(), id: "g".into() }, Act::Forget { id: "g".into() }]);
        // The line deleted: the Google task goes.
        let acts = plan(&[], &|_| 0, &live, &base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Delete { id: "g".into() }, Act::Forget { id: "g".into() }]);
        // The tag removed by hand: unlinked, Google keeps its task.
        let acts = plan(&one("- [ ] Call [gtask:: g]"), &|_| 0, &live, &base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Unlink { path: "N.md".into(), raw: "- [ ] Call [gtask:: g]".into(), id: "g".into() }, Act::Forget { id: "g".into() }]);
        // A copied line: the copy loses the id and is promoted anew.
        let lines = scan("N.md", "- [ ] Call #gtask [gtask:: g]\n- [ ] Call #gtask [gtask:: g]");
        let acts = plan(&lines, &|_| 0, &live, &base, &BTreeMap::new());
        assert_eq!(acts, vec![Act::Unlink { path: "N.md".into(), raw: "- [ ] Call #gtask [gtask:: g]".into(), id: "g".into() }]);
    }

    #[test]
    fn edits_find_their_line_by_id_or_text() {
        let text = "a\n- [ ] Call #gtask [gtask:: g]\n- [ ] New #gtask";
        let (out, done) = edit_note(
            text,
            &[
                LineEdit { find: Find::Id("g".into()), with: Box::new(|l| rewrite(l, &Fields { title: "Call".into(), due: None, done: true })) },
                LineEdit { find: Find::Exact("- [ ] New #gtask".into()), with: Box::new(|l| attach(l, "h")) },
                LineEdit { find: Find::Exact("- [ ] Gone #gtask".into()), with: Box::new(|l| attach(l, "i")) },
            ],
        );
        assert_eq!(out, "a\n- [x] Call #gtask [gtask:: g]\n- [ ] New #gtask [gtask:: h]");
        assert_eq!(done, vec![true, true, false]);
    }

    #[test]
    fn google_json_both_ways() {
        let v = serde_json::json!({"id":"g","title":" Call ","status":"completed","due":"2026-10-20T00:00:00.000Z","updated":"1970-01-01T00:00:02.000Z"});
        let (id, r) = remote_from_json(&v).unwrap();
        assert_eq!((id.as_str(), r.fields, r.updated_ms, r.deleted), ("g", f("Call", Some("2026-10-20"), true), 2000, false));
        let b = google_body(&f("Call", Some("2026-10-20"), false), Some("From N.md"));
        assert_eq!(b["due"], "2026-10-20T00:00:00.000Z");
        assert_eq!(b["status"], "needsAction");
        assert_eq!(b["notes"], "From N.md");
        assert!(google_body(&f("x", None, true), None)["due"].is_null());
    }
}
