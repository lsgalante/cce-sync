//! The vault's MCP server: what Claude (the iOS and desktop apps, claude.ai,
//! Claude Code) calls through the custom connector. Streamable HTTP,
//! stateless: one JSON-RPC request in, one JSON response out, no event
//! stream. The host (the Durable Object) authenticates the request and
//! holds the vault lock around [`Mcp::handle`].
//!
//! Reads go through an in-memory `cce_vault::Index` of the vault's notes,
//! canvases and settings (attachments are empty placeholders, so links to
//! them resolve without their bytes in memory). It is rebuilt only when the
//! vault's seq has moved since it was built.
//!
//! Writes go through the index's own write calls (`write_text`, `append`,
//! `rename` with its link rewrite, `set_task`, `delete`), whose journal
//! becomes one batch for [`Vault::apply`] from the device `claude`: every
//! change Claude makes is a revision, restorable, and listed under that
//! device. A rename becomes a server-side move, so an attachment's bytes
//! never pass through the index; a placeholder is never uploaded.
//!
//! Tables (`.csv`, `.tsv`) and workbooks (`.xlsx`, feature `xlsx`) are
//! attachments, so their tools go around the index: each write is one `Put`
//! from `claude` conditional on the rev the tool read ([`crate::table`]
//! checks the rows first; [`crate::xlsx`] refuses a workbook whose parts
//! IronCalc would drop).

use std::collections::{BTreeMap, BTreeSet};

use cce_vault::index::Written;
use cce_vault::{FileKind, Index};
use chrono::NaiveDate;
use serde_json::{json, Value};

use crate::core::{ApplyError, Operation, Store, Vault};
use crate::proto::{b64_encode, hash, FileMeta, Kind, Pre, Refusal, Rev, Seq};
use crate::table;
#[cfg(feature = "xlsx")]
use crate::xlsx;

/// The protocol revisions this server speaks, newest first. An
/// `initialize` naming one of them gets it back; anything else gets the
/// newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// The device Claude's writes are recorded under.
pub const DEVICE: &str = "claude";

/// Images up to this size are returned as image content.
const IMAGE_MAX: u64 = 5 * 1024 * 1024;

/// Tables up to this size are shown whole; a larger one is read by rows.
const TABLE_MAX: usize = 256 * 1024;

const INSTRUCTIONS: &str = "\
This is the user's personal notes vault: Markdown notes with wikilinks, kept in sync with \
their laptop and phone. You have full read and write access.

- Notes link with wikilinks in their shortest form, [[Note Name]] (add a folder only when two notes \
share a name). Embeds are ![[name]]; #tags and YAML frontmatter properties are used.
- Tasks are lines like `- [ ] text`, ticked as `- [x] text`. A due date is written at the end as \
`📅 YYYY-MM-DD`.
- Daily notes: use daily_note / append_daily with the user's LOCAL date (YYYY-MM-DD); the server's \
clock is UTC.
- Read a note before changing it. Prefer edit_note (replace an exact passage) or append_note for \
small changes; write_note replaces a whole note and needs the rev you read.
- Tables are .csv and .tsv files: read_table reads them, append_rows adds rows, edit_table replaces \
an exact passage, write_table replaces the whole file and needs the rev you read. Every row must \
have as many fields as the header. search does not look inside tables.
- Every change is versioned. history and restore_version undo anything, including deletes.
- Paths are vault paths like `Folder/Note.md`. Most tools also accept a bare note name and resolve \
it the way a wikilink would.";

#[cfg(feature = "xlsx")]
const XLSX_INSTRUCTIONS: &str = "\n\
- Workbooks are .xlsx files: read_spreadsheet shows a sheet's values and formulas, set_cells and \
append_spreadsheet_rows change it (inputs are typed as in the sheet: `=` starts a formula), \
create_spreadsheet makes a new one. A workbook with charts, pictures, pivot tables, Excel tables, \
comments, macros, data validation or hyperlinks can be read but not written here.";

fn instructions() -> String {
    #[cfg(feature = "xlsx")]
    return format!("{INSTRUCTIONS}{XLSX_INSTRUCTIONS}");
    #[cfg(not(feature = "xlsx"))]
    INSTRUCTIONS.to_string()
}

/// What the host sends back: an HTTP status and, unless it is 202, a JSON
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Option<String>,
}

impl Reply {
    fn json(v: Value) -> Reply {
        Reply { status: 200, body: Some(v.to_string()) }
    }
}

/// Where `daily_note` gets the day's events: the server's Google bridge.
pub trait AgendaSource {
    /// The agenda of a local day (`YYYY-MM-DD`); `None` when there is
    /// none to give (no bridge, or Google unreachable).
    fn agenda(&self, date: &str) -> impl std::future::Future<Output = Option<crate::agenda::Agenda>>;
}

/// No agenda (tests, a host without the bridge).
pub struct NoAgenda;

impl AgendaSource for NoAgenda {
    async fn agenda(&self, _: &str) -> Option<crate::agenda::Agenda> {
        None
    }
}

/// The MCP server's state across requests: the cached index.
#[derive(Default)]
pub struct Mcp {
    cache: Option<(Seq, Index)>,
}

/// A tool's failure, shown to Claude as an error result.
struct ToolError(String);

impl<E: std::fmt::Display> From<E> for ToolError {
    fn from(e: E) -> Self {
        ToolError(e.to_string())
    }
}

type ToolResult = Result<Value, ToolError>;

fn text(s: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": s.into() }] })
}

fn rpc_ok(id: &Value, result: Value) -> Reply {
    Reply::json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn rpc_err(id: &Value, code: i64, message: &str) -> Reply {
    Reply::json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

impl Mcp {
    pub fn new() -> Mcp {
        Mcp::default()
    }

    /// Handle one POST to `/mcp`, with no agenda source.
    pub async fn handle<S: Store>(&mut self, vault: &mut Vault<S>, body: &[u8], now: i64) -> Reply {
        self.handle_with(vault, body, now, &NoAgenda).await
    }

    /// Handle one POST to `/mcp`; `daily_note` adds the day's events from
    /// `agenda` (the server's Google bridge).
    pub async fn handle_with<S: Store, A: AgendaSource>(&mut self, vault: &mut Vault<S>, body: &[u8], now: i64, agenda: &A) -> Reply {
        let Ok(msg) = serde_json::from_slice::<Value>(body) else {
            return rpc_err(&Value::Null, -32700, "parse error");
        };
        if !msg.is_object() {
            return rpc_err(&Value::Null, -32600, "one JSON-RPC message per request (batches are not supported)");
        }
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let Some(id) = msg.get("id").cloned() else {
            // A notification (initialized, cancelled) or a response: nothing to answer.
            return Reply { status: 202, body: None };
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
                let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
                rpc_ok(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "cce-sync", "title": "Vault", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": instructions(),
                    }),
                )
            }
            "ping" => rpc_ok(&id, json!({})),
            "tools/list" => rpc_ok(&id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if !tools().iter().any(|t| t["name"] == name) {
                    return rpc_err(&id, -32602, &format!("unknown tool: {name}"));
                }
                let result = match self.call(vault, &name, &args, now, agenda).await {
                    Ok(v) => v,
                    Err(ToolError(e)) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                };
                rpc_ok(&id, result)
            }
            _ => rpc_err(&id, -32601, &format!("method not found: {method}")),
        }
    }

    /// The index, rebuilt when the vault moved on since it was built.
    async fn index<S: Store>(&mut self, vault: &Vault<S>) -> Result<&mut Index, ToolError> {
        let seq = vault.store.seq()?;
        if self.cache.as_ref().is_none_or(|(s, _)| *s != seq) {
            let mut files = Vec::new();
            for row in vault.store.live_files()? {
                let meta = row.meta().expect("live");
                let bytes = if meta.kind == Kind::Attachment {
                    Vec::new() // a placeholder: links resolve, bytes stay in the store
                } else {
                    vault.store.blob_get(&meta.hash).await?.ok_or_else(|| ToolError(format!("{}: blob missing", meta.path)))?
                };
                files.push((meta.path, bytes));
            }
            self.cache = Some((seq, Index::in_memory(files)));
        }
        Ok(&mut self.cache.as_mut().expect("just built").1)
    }

    /// Turn what the index wrote into one batch, apply it as `claude`,
    /// and keep the index current. On a refusal the index is dropped, since
    /// it now holds writes the vault does not.
    async fn commit<S: Store>(&mut self, vault: &mut Vault<S>, now: i64) -> Result<Vec<String>, ToolError> {
        let Some((_, index)) = self.cache.as_mut() else { return Ok(Vec::new()) };
        let written = index.take_written();
        let ops = match ops_from_journal(index, vault, written) {
            Ok(ops) => ops,
            Err(e) => {
                self.cache = None;
                return Err(e);
            }
        };
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let touched: Vec<String> = ops
            .iter()
            .map(|op| match op {
                Operation::Put { path, .. } | Operation::Delete { path, .. } => path.clone(),
                Operation::Move { to, .. } => to.clone(),
            })
            .collect();
        match vault.apply(ops, DEVICE, now).await {
            Ok(c) => {
                if let Some(cache) = self.cache.as_mut() {
                    cache.0 = c.seq;
                }
                Ok(touched)
            }
            Err(e) => {
                self.cache = None;
                Err(apply_error(e))
            }
        }
    }

    /// Write a table's or workbook's bytes as `claude`, conditional on
    /// `pre`, and return its rev. The index holds attachments as
    /// placeholders, so rewriting one it already has leaves it current; a
    /// new one waits for the next rebuild.
    async fn put_attachment<S: Store>(&mut self, vault: &mut Vault<S>, path: &str, bytes: Vec<u8>, pre: Pre, now: i64) -> Result<Rev, ToolError> {
        let before = vault.store.seq()?;
        let existed = vault.store.file(path)?.and_then(|r| r.meta()).is_some();
        let c = vault.apply(vec![Operation::Put { path: path.to_string(), pre, bytes }], DEVICE, now).await.map_err(apply_error)?;
        if let Some(cache) = self.cache.as_mut().filter(|c| existed && c.0 == before) {
            cache.0 = c.seq;
        }
        Ok(c.applied[0].rev)
    }

    async fn call<S: Store, A: AgendaSource>(&mut self, vault: &mut Vault<S>, name: &str, a: &Value, now: i64, agenda: &A) -> ToolResult {
        match name {
            "search" => {
                let q = req_str(a, "query")?;
                let limit = opt_u64(a, "limit").unwrap_or(20) as usize;
                let index = self.index(vault).await?;
                if let Some(tag) = q.strip_prefix('#').filter(|t| !t.contains(' ')) {
                    let paths = index.tagged(tag);
                    return Ok(text(lines_or(paths.iter().take(limit).map(|p| p.to_string()), "no notes with that tag")));
                }
                let hits = index.search(q, limit);
                let mut out = Vec::new();
                for h in hits {
                    out.push(format!("{} ({} matching lines)", h.path, h.total));
                    for l in h.lines {
                        out.push(format!("  {}: {}", l.line + 1, l.text.trim()));
                    }
                }
                Ok(text(lines_or(out.into_iter(), "no matches")))
            }
            "find_notes" => {
                let q = req_str(a, "query")?;
                let limit = opt_u64(a, "limit").unwrap_or(20) as usize;
                let index = self.index(vault).await?;
                let found = index.find(q, limit).into_iter().map(|m| {
                    if m.matched == cce_vault::index::stem(&m.path) { m.path } else { format!("{} (as {})", m.path, m.matched) }
                });
                Ok(text(lines_or(found, "nothing found")))
            }
            "list_folder" => {
                let folder = opt_str(a, "path").unwrap_or("").trim_matches('/').to_string();
                let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
                let listing = vault.listing()?;
                let mut dirs = BTreeSet::new();
                let mut files = Vec::new();
                for f in &listing.files {
                    let Some(rest) = f.path.strip_prefix(&prefix) else { continue };
                    match rest.split_once('/') {
                        Some((d, _)) => {
                            dirs.insert(format!("{prefix}{d}/"));
                        }
                        None => files.push(format!("{}  ({} bytes, rev {})", f.path, f.size, f.rev)),
                    }
                }
                Ok(text(lines_or(dirs.into_iter().chain(files), "empty or no such folder")))
            }
            "read_note" => {
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                let meta = meta_of(vault, &path)?;
                if meta.kind == Kind::Attachment {
                    return Err(ToolError(format!("{path} is an attachment: use read_attachment")));
                }
                let body = index.read_text(&path)?;
                let (start, end) = (opt_u64(a, "start_line"), opt_u64(a, "end_line"));
                let shown = if start.is_some() || end.is_some() {
                    let from = start.unwrap_or(1).max(1) as usize;
                    let to = end.map_or(usize::MAX, |e| e as usize);
                    body.split_inclusive('\n').enumerate().filter(|(i, _)| *i + 1 >= from && *i < to).map(|(_, l)| l).collect()
                } else {
                    body.clone()
                };
                let mut header = vec![format!("path: {path}"), format!("rev: {}", meta.rev)];
                if let Some(note) = index.note(&path) {
                    if !note.properties.is_empty() {
                        header.push(format!("properties: {}", serde_json::to_string(&note.properties)?));
                    }
                    let out: BTreeSet<String> =
                        index.outgoing(&path).into_iter().map(|(l, t)| t.map_or(format!("{} (unresolved)", l.target), str::to_string)).collect();
                    if !out.is_empty() {
                        header.push(format!("links to: {}", out.into_iter().collect::<Vec<_>>().join(", ")));
                    }
                }
                let back: BTreeSet<&str> = index.backlinks(&path).into_iter().map(|b| b.source).collect();
                if !back.is_empty() {
                    header.push(format!("linked from: {}", back.into_iter().collect::<Vec<_>>().join(", ")));
                }
                Ok(text(format!("{}\n---\n{shown}", header.join("\n"))))
            }
            "backlinks" => {
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                let mut by: BTreeMap<&str, usize> = BTreeMap::new();
                for b in index.backlinks(&path) {
                    *by.entry(b.source).or_default() += 1;
                }
                Ok(text(lines_or(by.into_iter().map(|(s, n)| format!("{s} ({n})")), "no backlinks")))
            }
            "unresolved_links" => {
                let index = self.index(vault).await?;
                // Keyed by the lowercased target; shown as first written.
                let out = index.unresolved().into_iter().map(|(target, from)| {
                    let shown = from.first().map_or(target.as_str(), |b| b.link.target.as_str()).to_string();
                    let src: BTreeSet<&str> = from.iter().map(|b| b.source).collect();
                    format!("[[{shown}]] in {}", src.into_iter().collect::<Vec<_>>().join(", "))
                });
                Ok(text(lines_or(out, "every link resolves")))
            }
            "tags" => {
                let index = self.index(vault).await?;
                Ok(text(lines_or(index.tags().into_iter().map(|(t, n)| format!("#{t} ({n})")), "no tags")))
            }
            "tasks" => {
                let status = opt_str(a, "status").unwrap_or("open");
                let due_before = opt_date(a, "due_before")?;
                let in_path = opt_str(a, "path");
                let tag = opt_str(a, "tag").map(|t| t.trim_start_matches('#').to_lowercase());
                let index = self.index(vault).await?;
                let tagged: Option<BTreeSet<String>> = tag.as_deref().map(|t| index.tagged(t).iter().map(|s| s.to_string()).collect());
                let mut out = Vec::new();
                for (path, t) in index.tasks() {
                    let open = t.status == ' ';
                    let keep = match status {
                        "open" => open,
                        "done" => !open,
                        _ => true,
                    } && in_path.is_none_or(|p| path == p || path.starts_with(&format!("{}/", p.trim_end_matches('/'))))
                        && due_before.is_none_or(|d| t.due.is_some_and(|due| due <= d))
                        && tagged.as_ref().is_none_or(|s| s.contains(path) || tag.as_ref().is_some_and(|tg| t.text.to_lowercase().contains(&format!("#{tg}"))));
                    if keep {
                        out.push(format!("{path}:{}  [{}] {}", t.line + 1, t.status, t.text.trim()));
                    }
                }
                Ok(text(lines_or(out.into_iter(), "no tasks")))
            }
            "daily_note" => {
                let date = req_date(a, "date")?;
                let index = self.index(vault).await?;
                let path = daily_config(index).path_for(date);
                let note = match index.read_text(&path) {
                    Ok(body) => format!("path: {path}\nrev: {}\n---\n{body}", meta_of(vault, &path)?.rev),
                    Err(_) => format!("path: {path}\n(no note for {date} yet; append_daily creates it from the template)"),
                };
                let events = match agenda.agenda(&date.to_string()).await {
                    Some(a) if a.connected => {
                        format!("\n---\nagenda (Google Calendar, {}; not part of the note):\n{}", a.time_zone, a.lines().join("\n"))
                    }
                    _ => String::new(),
                };
                Ok(text(format!("{note}{events}")))
            }
            "history" => {
                let index = self.index(vault).await?;
                let path = resolve_any(index, vault, req_str(a, "path")?)?;
                let h = vault.history(&path)?;
                let out = h.versions.iter().map(|v| {
                    let what = if v.hash.is_some() { format!("{} bytes", v.size) } else { "deleted".into() };
                    format!("rev {}  {}  by {}  {what}", v.rev, iso(v.at), v.device)
                });
                Ok(text(lines_or(out, "no history")))
            }
            "read_version" => {
                let path = req_str(a, "path")?.to_string();
                let rev = req_u64(a, "rev")?;
                let v = vault.store.version(&path, rev)?.ok_or_else(|| ToolError(format!("{path} has no rev {rev}")))?;
                let h = v.hash.ok_or_else(|| ToolError(format!("rev {rev} of {path} is its deletion")))?;
                let bytes = vault.store.blob_get(&h).await?.ok_or_else(|| ToolError("blob missing".into()))?;
                Ok(text(format!("path: {path}\nrev: {rev}\n---\n{}", String::from_utf8_lossy(&bytes))))
            }
            "read_attachment" => {
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                let meta = meta_of(vault, &path)?;
                let mime = image_mime(&path);
                match mime {
                    Some(m) if meta.size <= IMAGE_MAX => {
                        let bytes = vault.store.blob_get(&meta.hash).await?.ok_or_else(|| ToolError("blob missing".into()))?;
                        Ok(json!({ "content": [
                            { "type": "text", "text": format!("{path} ({} bytes)", meta.size) },
                            { "type": "image", "data": b64_encode(&bytes), "mimeType": m },
                        ]}))
                    }
                    _ if table::delimiter(&path).is_some() => Ok(text(format!("{path}: {} bytes, rev {} (a table: use read_table)", meta.size, meta.rev))),
                    #[cfg(feature = "xlsx")]
                    _ if xlsx::is_workbook(&path) => Ok(text(format!("{path}: {} bytes, rev {} (a workbook: use read_spreadsheet)", meta.size, meta.rev))),
                    _ => Ok(text(format!("{path}: {} bytes, rev {} (not an image this tool returns)", meta.size, meta.rev))),
                }
            }
            "edit_note" => {
                let old = req_str(a, "old_text")?;
                let new = req_str(a, "new_text")?;
                let all = a.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                let body = index.read_text(&path)?;
                let n = body.matches(old).count();
                if old.is_empty() || n == 0 {
                    return Err(ToolError(format!("old_text not found in {path}; read the note and quote it exactly")));
                }
                if n > 1 && !all {
                    return Err(ToolError(format!("old_text occurs {n} times in {path}; quote more of it, or set replace_all")));
                }
                let edited = if all { body.replace(old, new) } else { body.replacen(old, new, 1) };
                index.write_text(&path, &edited)?;
                self.commit(vault, now).await?;
                Ok(text(format!("edited {path} ({} replacement{}), now rev {}", n.min(if all { n } else { 1 }), if all && n > 1 { "s" } else { "" }, meta_of(vault, &path)?.rev)))
            }
            "write_note" => {
                let content = req_str(a, "content")?;
                let expected = opt_u64(a, "expected_rev");
                let path = clean_path(req_str(a, "path")?)?;
                let current = vault.store.file(&path)?.and_then(|r| r.meta());
                match (&current, expected) {
                    (Some(m), None) => {
                        return Err(ToolError(format!("{path} exists (rev {}): pass expected_rev to replace it", m.rev)))
                    }
                    (Some(m), Some(e)) if m.rev != e => {
                        return Err(ToolError(format!("{path} is at rev {}, not {e}: it changed since you read it", m.rev)))
                    }
                    (None, Some(e)) if e != 0 => return Err(ToolError(format!("{path} does not exist (it may have been deleted)"))),
                    _ => {}
                }
                let index = self.index(vault).await?;
                index.write_text(&path, content)?;
                self.commit(vault, now).await?;
                let rev = meta_of(vault, &path)?.rev;
                Ok(text(format!("{} {path}, now rev {rev}", if current.is_some() { "replaced" } else { "created" })))
            }
            "append_note" => {
                let add = req_str(a, "text")?;
                let heading = opt_str(a, "heading");
                let index = self.index(vault).await?;
                let path = match resolve(index, req_str(a, "path")?) {
                    Ok(p) => p,
                    Err(_) => clean_path(req_str(a, "path")?)?,
                };
                append(index, &path, add, heading)?;
                self.commit(vault, now).await?;
                Ok(text(format!("appended to {path}, now rev {}", meta_of(vault, &path)?.rev)))
            }
            "append_daily" => {
                let date = req_date(a, "date")?;
                let add = req_str(a, "text")?;
                let heading = opt_str(a, "heading");
                let index = self.index(vault).await?;
                let cfg = daily_config(index);
                let path = cfg.path_for(date);
                let created = index.read_text(&path).is_err();
                if created {
                    let body = template(index, &cfg.template).unwrap_or_default();
                    let at = date.and_hms_opt(9, 0, 0).expect("valid time");
                    let title = cce_vault::index::stem(&path).to_string();
                    index.write_text(&path, &cce_vault::daily::expand_template(&body, &title, &at, &cfg.format))?;
                }
                append(index, &path, add, heading)?;
                self.commit(vault, now).await?;
                Ok(text(format!("{}appended to {path}, now rev {}", if created { "created and " } else { "" }, meta_of(vault, &path)?.rev)))
            }
            "set_task" => {
                let line = req_u64(a, "line")?;
                let done = a.get("done").and_then(Value::as_bool).ok_or_else(|| ToolError("done (true/false) is required".into()))?;
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                if line == 0 {
                    return Err(ToolError("lines are numbered from 1".into()));
                }
                index.set_task(&path, line as usize - 1, if done { 'x' } else { ' ' })?;
                self.commit(vault, now).await?;
                Ok(text(format!("{} {path}:{line}", if done { "ticked" } else { "unticked" })))
            }
            "rename_note" => {
                let to = clean_path(req_str(a, "to")?)?;
                let index = self.index(vault).await?;
                let from = resolve(index, req_str(a, "from")?)?;
                let plan = index.rename(&from, &to)?;
                let touched = self.commit(vault, now).await?;
                let others = touched.iter().filter(|p| **p != plan.to).count();
                Ok(text(format!("renamed {from} to {}; links rewritten in {others} file{}", plan.to, if others == 1 { "" } else { "s" })))
            }
            "delete_note" => {
                let index = self.index(vault).await?;
                let path = resolve(index, req_str(a, "path")?)?;
                index.delete(&path)?;
                self.commit(vault, now).await?;
                Ok(text(format!("deleted {path} (restore_version brings it back)")))
            }
            "read_table" => {
                let (path, delim) = table_path(req_str(a, "path")?)?;
                let (meta, body) = read_table(vault, &path).await?;
                let mut header = vec![format!("path: {path}"), format!("rev: {}", meta.rev)];
                let recs = match table::records(&body, delim) {
                    Ok(r) => r,
                    Err(e) => {
                        header.push(format!("warning: {e}"));
                        if body.len() > TABLE_MAX {
                            return Err(ToolError(format!("{path} can't be read as a table ({e}) and is too large to show whole")));
                        }
                        return Ok(text(format!("{}\n---\n{body}", header.join("\n"))));
                    }
                };
                header.push(format!("rows: {} (not counting the header)", recs.len().saturating_sub(1)));
                if let Some(h) = recs.first() {
                    if let Some((n, r)) = recs.iter().enumerate().skip(1).find(|(_, r)| r.fields != h.fields) {
                        header.push(format!("warning: row {n} (line {}) has {} fields; the header has {}", r.line, r.fields, h.fields));
                    }
                }
                let (start, end) = (opt_u64(a, "start_row"), opt_u64(a, "end_row"));
                let shown = if start.is_some() || end.is_some() {
                    let from = start.unwrap_or(1).max(1) as usize;
                    let to = end.map_or(usize::MAX, |e| e as usize);
                    let rows = recs.iter().enumerate().filter(|(i, _)| *i == 0 || (*i >= from && *i <= to));
                    rows.map(|(_, r)| &body[r.start..r.end]).collect::<Vec<_>>().join("\n")
                } else if body.len() > TABLE_MAX {
                    header.push(format!("{} bytes: too large to show whole; read it in parts with start_row and end_row", body.len()));
                    recs.first().map_or(String::new(), |h| body[h.start..h.end].to_string())
                } else {
                    body
                };
                Ok(text(format!("{}\n---\n{shown}", header.join("\n"))))
            }
            "write_table" => {
                let (path, delim) = table_path(req_str(a, "path")?)?;
                let content = req_str(a, "content")?;
                let expected = opt_u64(a, "expected_rev");
                let recs = table::check(content, delim).map_err(|e| ToolError(format!("{path}: {e}")))?;
                let current = vault.store.file(&path)?.and_then(|r| r.meta());
                let pre = match (&current, expected) {
                    (Some(m), None) => return Err(ToolError(format!("{path} exists (rev {}): pass expected_rev to replace it", m.rev))),
                    (Some(m), Some(e)) if m.rev != e => {
                        return Err(ToolError(format!("{path} is at rev {}, not {e}: it changed since you read it", m.rev)))
                    }
                    (Some(m), Some(_)) => Pre::Rev(m.rev),
                    (None, Some(e)) if e != 0 => return Err(ToolError(format!("{path} does not exist (it may have been deleted)"))),
                    (None, _) => Pre::Absent,
                };
                let rev = self.put_attachment(vault, &path, content.as_bytes().to_vec(), pre, now).await?;
                let what = if current.is_some() { "replaced" } else { "created" };
                Ok(text(format!("{what} {path} ({} rows), now rev {rev}", recs.len() - 1)))
            }
            "append_rows" => {
                let (path, delim) = table_path(req_str(a, "path")?)?;
                let rows = req_str(a, "rows")?;
                if rows.trim().is_empty() {
                    return Err(ToolError("rows is empty".into()));
                }
                let (meta, body) = read_table(vault, &path).await?;
                let after = table::appended(&body, rows);
                guard_table(&path, &body, &after, delim)?;
                let added = table::records(rows, delim).map_or(0, |r| r.len());
                let rev = self.put_attachment(vault, &path, after.into_bytes(), Pre::Rev(meta.rev), now).await?;
                Ok(text(format!("appended {added} row{} to {path}, now rev {rev}", if added == 1 { "" } else { "s" })))
            }
            "edit_table" => {
                let (path, delim) = table_path(req_str(a, "path")?)?;
                let old = req_str(a, "old_text")?;
                let new = req_str(a, "new_text")?;
                let all = a.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
                let (meta, body) = read_table(vault, &path).await?;
                let n = body.matches(old).count();
                if old.is_empty() || n == 0 {
                    return Err(ToolError(format!("old_text not found in {path}; read the table and quote it exactly")));
                }
                if n > 1 && !all {
                    return Err(ToolError(format!("old_text occurs {n} times in {path}; quote more of it, or set replace_all")));
                }
                let after = if all { body.replace(old, new) } else { body.replacen(old, new, 1) };
                guard_table(&path, &body, &after, delim)?;
                let rev = self.put_attachment(vault, &path, after.into_bytes(), Pre::Rev(meta.rev), now).await?;
                Ok(text(format!("edited {path} ({} replacement{}), now rev {rev}", if all { n } else { 1 }, if all && n > 1 { "s" } else { "" })))
            }
            #[cfg(feature = "xlsx")]
            "read_spreadsheet" => {
                let path = workbook_path(req_str(a, "path")?)?;
                let (meta, _, model) = read_workbook(vault, &path).await?;
                let sheet = xlsx::sheet(&model, opt_str(a, "sheet"))?;
                let area = opt_str(a, "range").map(xlsx::range).transpose()?;
                let mut sheets = Vec::new();
                for (i, name) in xlsx::sheet_names(&model).iter().enumerate() {
                    sheets.push(match xlsx::used(&model, i as u32)? {
                        Some((t, l, b, r)) => format!("{name} ({}{t}:{}{b})", xlsx::column_name(l), xlsx::column_name(r)),
                        None => format!("{name} (empty)"),
                    });
                }
                let grid = xlsx::render(&model, sheet, area)?;
                let name = &xlsx::sheet_names(&model)[sheet as usize];
                Ok(text(format!("path: {path}\nrev: {}\nsheets: {}\nsheet: {name}\n---\n{grid}", meta.rev, sheets.join(", "))))
            }
            #[cfg(feature = "xlsx")]
            "set_cells" => {
                let path = workbook_path(req_str(a, "path")?)?;
                let cells = a
                    .get("cells")
                    .and_then(Value::as_object)
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| ToolError("cells (an object like {\"B4\": \"=SUM(B1:B3)\"}) is required".into()))?;
                let cells: Vec<(String, String)> = cells.iter().map(|(k, v)| (k.clone(), input(v))).collect();
                let (meta, bytes, mut model) = read_workbook(vault, &path).await?;
                if let Some(e) = opt_u64(a, "expected_rev").filter(|e| *e != meta.rev) {
                    return Err(ToolError(format!("{path} is at rev {}, not {e}: it changed since you read it", meta.rev)));
                }
                refuse_unkept(&path, &bytes)?;
                let sheet = xlsx::sheet(&model, opt_str(a, "sheet"))?;
                xlsx::set(&mut model, sheet, &cells)?;
                let rev = self.put_attachment(vault, &path, xlsx::save(&model)?, Pre::Rev(meta.rev), now).await?;
                let mut out = vec![format!("set {} cell{} in {path}, now rev {rev}", cells.len(), if cells.len() == 1 { "" } else { "s" })];
                for (addr, _) in cells.iter().take(50) {
                    let (row, col) = xlsx::cell(addr)?;
                    out.push(xlsx::describe(&model, sheet, row, col)?);
                }
                Ok(text(out.join("\n")))
            }
            #[cfg(feature = "xlsx")]
            "append_spreadsheet_rows" => {
                let path = workbook_path(req_str(a, "path")?)?;
                let rows = rows_arg(a, "rows")?;
                if rows.is_empty() {
                    return Err(ToolError("rows is empty".into()));
                }
                let (meta, bytes, mut model) = read_workbook(vault, &path).await?;
                refuse_unkept(&path, &bytes)?;
                let sheet = xlsx::sheet(&model, opt_str(a, "sheet"))?;
                let first = xlsx::append(&mut model, sheet, &rows)?;
                let rev = self.put_attachment(vault, &path, xlsx::save(&model)?, Pre::Rev(meta.rev), now).await?;
                let last = first + rows.len() as i32 - 1;
                Ok(text(format!("appended rows {first}-{last} to {path}, now rev {rev}")))
            }
            #[cfg(feature = "xlsx")]
            "create_spreadsheet" => {
                let path = workbook_path(req_str(a, "path")?)?;
                let given = a.get("sheets").and_then(Value::as_array).ok_or_else(|| ToolError("sheets is required".into()))?;
                let mut sheets = Vec::new();
                for (i, s) in given.iter().enumerate() {
                    let name = s.get("name").and_then(Value::as_str).map_or_else(|| format!("Sheet{}", i + 1), str::to_string);
                    sheets.push((name, rows_arg(s, "rows").unwrap_or_default()));
                }
                if vault.store.file(&path)?.and_then(|r| r.meta()).is_some() {
                    return Err(ToolError(format!("{path} exists: change it with set_cells or append_spreadsheet_rows")));
                }
                let model = xlsx::create(&sheets)?;
                let rev = self.put_attachment(vault, &path, xlsx::save(&model)?, Pre::Absent, now).await?;
                Ok(text(format!("created {path} ({} sheet{}), rev {rev}", sheets.len(), if sheets.len() == 1 { "" } else { "s" })))
            }
            "restore_version" => {
                let path = req_str(a, "path")?.to_string();
                let rev = req_u64(a, "rev")?;
                let v = vault.store.version(&path, rev)?.ok_or_else(|| ToolError(format!("{path} has no rev {rev}")))?;
                let h = v.hash.ok_or_else(|| ToolError(format!("rev {rev} of {path} is its deletion")))?;
                let bytes = vault.store.blob_get(&h).await?.ok_or_else(|| ToolError("blob missing".into()))?;
                let c = vault
                    .apply(vec![Operation::Put { path: path.clone(), pre: Pre::Any, bytes }], DEVICE, now)
                    .await
                    .map_err(|e| ToolError(format!("{e:?}")))?;
                Ok(text(format!("restored {path} to the text of rev {rev}, now rev {}", c.applied[0].rev)))
            }
            _ => Err(ToolError(format!("unknown tool {name}"))),
        }
    }
}

/// The batch for what the index wrote, collapsed to each path's final
/// state. A path removed and another put with the same bytes (or an
/// attachment placeholder) is a move.
fn ops_from_journal<S: Store>(index: &Index, vault: &Vault<S>, written: Vec<Written>) -> Result<Vec<Operation>, ToolError> {
    let mut order: Vec<String> = Vec::new();
    let mut fin: BTreeMap<String, bool> = BTreeMap::new();
    for w in written {
        let (path, present) = match w {
            Written::Put(p) => (p, true),
            Written::Removed(p) => (p, false),
        };
        if !fin.contains_key(&path) {
            order.push(path.clone());
        }
        fin.insert(path, present);
    }
    let stored = |p: &str| vault.store.file(p).ok().flatten().and_then(|r| r.meta());
    let mut removed: Vec<FileMeta> = order.iter().filter(|p| !fin[*p]).filter_map(|p| stored(p)).collect();
    let mut ops = Vec::new();
    let mut puts = Vec::new();
    for path in order.iter().filter(|p| fin[*p]) {
        let bytes = index.read_bytes(path)?;
        let placeholder = Kind::of(path) == Kind::Attachment;
        // A move: the bytes left one path for this one.
        let from = removed.iter().position(|m| {
            (placeholder && m.kind == Kind::Attachment && stored(path).is_none())
                || (!placeholder && m.hash == hash(&bytes) && Kind::of(&m.path) == Kind::of(path))
        });
        if let Some(i) = from {
            let m = removed.remove(i);
            ops.push(Operation::Move { from: m.path, to: path.clone(), pre: Pre::Any });
            continue;
        }
        if placeholder {
            // Attachments never travel through the index.
            return Err(ToolError(format!("{path}: attachments can't be written through the index")));
        }
        puts.push(Operation::Put { path: path.clone(), pre: Pre::Any, bytes });
    }
    // A renamed note whose own links were re-rooted moved and changed: the
    // move above went by bytes, so pair any remaining removal with a new
    // note of the same kind the rename produced, as move + put.
    let mut later = Vec::new();
    for op in puts {
        if let Operation::Put { path, bytes, .. } = &op {
            if stored(path).is_none() {
                if let Some(i) = removed.iter().position(|m| m.kind == Kind::of(path)) {
                    let m = removed.remove(i);
                    ops.push(Operation::Move { from: m.path, to: path.clone(), pre: Pre::Any });
                    later.push(Operation::Put { path: path.clone(), pre: Pre::Any, bytes: bytes.clone() });
                    continue;
                }
            }
        }
        later.push(op);
    }
    ops.extend(later);
    ops.extend(removed.into_iter().map(|m| Operation::Delete { path: m.path, pre: Pre::Any }));
    Ok(ops)
}

/// The vault's daily-note settings, from `.cce/vault.kdl` (cce-vault's
/// reading of it, the one every device uses).
fn daily_config(index: &Index) -> cce_vault::daily::DailyConfig {
    cce_vault::daily::DailyConfig::load(index)
}

fn template(index: &Index, t: &str) -> Option<String> {
    let t = t.trim().trim_start_matches('/');
    if t.is_empty() {
        return None;
    }
    let rel = if FileKind::of(t) == FileKind::Note { t.to_string() } else { format!("{t}.md") };
    index.read_text(&rel).ok()
}

/// Append `add` to a note: at the end, or at the end of the section under
/// `heading` (before the next heading of the same or a higher level).
fn append(index: &mut Index, path: &str, add: &str, heading: Option<&str>) -> Result<(), ToolError> {
    index.append_under(path, add, heading)?;
    Ok(())
}

/// A path as given, or a note name resolved the way a wikilink would be.
fn resolve(index: &Index, given: &str) -> Result<String, ToolError> {
    let given = given.trim().trim_start_matches('/');
    if index.entry(given).is_some() {
        return Ok(given.to_string());
    }
    index
        .lookup(given.trim_end_matches(".md"))
        .ok_or_else(|| ToolError(format!("no note or file called {given}; try find_notes")))
}

/// Like [`resolve`], but also a path that only history knows (deleted).
fn resolve_any<S: Store>(index: &Index, vault: &Vault<S>, given: &str) -> Result<String, ToolError> {
    resolve(index, given).or_else(|e| {
        let p = given.trim().trim_start_matches('/');
        match vault.store.versions(p) {
            Ok(v) if !v.is_empty() => Ok(p.to_string()),
            _ => Err(e),
        }
    })
}

fn clean_path(p: &str) -> Result<String, ToolError> {
    let p = p.trim().trim_start_matches('/').to_string();
    crate::proto::check_path(&p).map_err(|e| ToolError(format!("{p}: {e}")))?;
    Ok(p)
}

fn meta_of<S: Store>(vault: &Vault<S>, path: &str) -> Result<FileMeta, ToolError> {
    vault.store.file(path)?.and_then(|r| r.meta()).ok_or_else(|| ToolError(format!("{path} does not exist")))
}

fn apply_error(e: ApplyError) -> ToolError {
    ToolError(match e {
        ApplyError::Refused(r) => refusal_text(&r),
        ApplyError::Store(s) => s.to_string(),
    })
}

/// A table's path as given, and its delimiter.
fn table_path(given: &str) -> Result<(String, char), ToolError> {
    let path = clean_path(given)?;
    let delim = table::delimiter(&path).ok_or_else(|| ToolError(format!("{path} is not a table (.csv or .tsv)")))?;
    Ok((path, delim))
}

/// A table's metadata and text.
async fn read_table<S: Store>(vault: &Vault<S>, path: &str) -> Result<(FileMeta, String), ToolError> {
    let (meta, bytes) = vault
        .read(path)
        .await?
        .ok_or_else(|| ToolError(format!("{path} does not exist; write_table creates it (header first)")))?;
    let body = String::from_utf8(bytes).map_err(|_| ToolError(format!("{path} is not UTF-8 text")))?;
    Ok((meta, body))
}

#[cfg(feature = "xlsx")]
fn workbook_path(given: &str) -> Result<String, ToolError> {
    let path = clean_path(given)?;
    if !xlsx::is_workbook(&path) {
        return Err(ToolError(format!("{path} is not a workbook (.xlsx)")));
    }
    Ok(path)
}

/// A workbook's metadata, bytes and model.
#[cfg(feature = "xlsx")]
async fn read_workbook<S: Store>(vault: &Vault<S>, path: &str) -> Result<(FileMeta, Vec<u8>, ironcalc::base::Model<'static>), ToolError> {
    let (meta, bytes) = vault
        .read(path)
        .await?
        .ok_or_else(|| ToolError(format!("{path} does not exist; create_spreadsheet makes a new one")))?;
    let model = xlsx::load(&bytes).map_err(|e| ToolError(format!("{path}: {e}")))?;
    Ok((meta, bytes, model))
}

/// Writing a workbook through IronCalc must not drop parts it doesn't keep.
#[cfg(feature = "xlsx")]
fn refuse_unkept(path: &str, bytes: &[u8]) -> Result<(), ToolError> {
    let lost = xlsx::unkept(bytes)?;
    if lost.is_empty() {
        return Ok(());
    }
    Err(ToolError(format!(
        "{path} has {}, which saving it here would drop: change it in cce-spreadsheet on the laptop instead (read_spreadsheet still reads it)",
        lost.join(", ")
    )))
}

/// A cell input from JSON: text as is, numbers and booleans as typed.
#[cfg(feature = "xlsx")]
fn input(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Rows of cell inputs: an array of arrays.
#[cfg(feature = "xlsx")]
fn rows_arg(a: &Value, k: &str) -> Result<Vec<Vec<String>>, ToolError> {
    let bad = || ToolError(format!("{k} must be an array of rows, each an array of cells"));
    let rows = a.get(k).and_then(Value::as_array).ok_or_else(bad)?;
    rows.iter().map(|r| r.as_array().map(|cells| cells.iter().map(input).collect()).ok_or_else(bad)).collect()
}

/// An edit must leave a well-formed table well formed. One that was
/// already ragged (edited by hand) only has to stay readable, so a bad row
/// elsewhere never blocks Claude's change.
fn guard_table(path: &str, before: &str, after: &str, delim: char) -> Result<(), ToolError> {
    let e = if table::check(before, delim).is_ok() {
        table::check(after, delim).err()
    } else {
        table::records(after, delim).err()
    };
    e.map_or(Ok(()), |e| Err(ToolError(format!("{path}: {e}; nothing was written"))))
}

fn refusal_text(r: &Refusal) -> String {
    match r {
        Refusal::CaseCollision { path, existing, .. } => format!("{path} differs only in case from {existing}"),
        Refusal::BadPath { path, reason, .. } => format!("{path}: {reason}"),
        Refusal::TooLarge { path, .. } => format!("{path}: too large"),
        Refusal::NotFound { path, .. } => format!("{path} does not exist"),
        Refusal::Conflict { path, .. } => format!("{path} changed meanwhile; try again"),
        Refusal::BadRequest { reason } => reason.clone(),
    }
}

fn image_mime(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn lines_or(lines: impl Iterator<Item = String>, empty: &str) -> String {
    let v: Vec<String> = lines.collect();
    if v.is_empty() {
        empty.to_string()
    } else {
        v.join("\n")
    }
}

fn req_str<'a>(a: &'a Value, k: &str) -> Result<&'a str, ToolError> {
    a.get(k).and_then(Value::as_str).ok_or_else(|| ToolError(format!("{k} is required")))
}

fn opt_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn opt_u64(a: &Value, k: &str) -> Option<u64> {
    a.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn req_u64(a: &Value, k: &str) -> Result<u64, ToolError> {
    opt_u64(a, k).ok_or_else(|| ToolError(format!("{k} (a number) is required")))
}

fn opt_date(a: &Value, k: &str) -> Result<Option<NaiveDate>, ToolError> {
    match opt_str(a, k) {
        None => Ok(None),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map(Some).map_err(|_| ToolError(format!("{k}: use YYYY-MM-DD"))),
    }
}

fn req_date(a: &Value, k: &str) -> Result<NaiveDate, ToolError> {
    opt_date(a, k)?.ok_or_else(|| ToolError(format!("{k} (the user's local date, YYYY-MM-DD) is required")))
}

/// One tool's definition.
fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str], read_only: bool, destructive: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": read_only,
            "openWorldHint": false,
        },
    })
}

fn s(d: &str) -> Value {
    json!({ "type": "string", "description": d })
}

fn n(d: &str) -> Value {
    json!({ "type": "integer", "description": d })
}

pub fn tools() -> Vec<Value> {
    let path = s("A vault path (Folder/Note.md) or a note name, resolved like a wikilink");
    let date = s("The user's local date, YYYY-MM-DD");
    #[allow(unused_mut)]
    let mut all = vec![
        tool("search", "Search notes", "Full-text search over every note (all words must match); a single #tag lists the notes with that tag.",
            json!({ "query": s("Words to find, or #tag"), "limit": n("Most files to return (default 20)") }), &["query"], true, false),
        tool("find_notes", "Find notes by name", "Fuzzy match on note names and aliases, like a quick switcher.",
            json!({ "query": s("Part of a name"), "limit": n("Most results (default 20)") }), &["query"], true, false),
        tool("list_folder", "List a folder", "The folders and files directly in a folder (the vault root when no path is given).",
            json!({ "path": s("A folder, e.g. Daily") }), &[], true, false),
        tool("read_note", "Read a note", "A note's text with its rev, properties, outgoing links and backlinks. Optionally a line range.",
            json!({ "path": path, "start_line": n("First line, from 1"), "end_line": n("Last line, inclusive") }), &["path"], true, false),
        tool("backlinks", "Backlinks", "The notes that link to a note, with how many links each has.",
            json!({ "path": path }), &["path"], true, false),
        tool("unresolved_links", "Unresolved links", "Links that point at notes that don't exist, and where they are.",
            json!({}), &[], true, false),
        tool("tags", "Tags", "Every tag in the vault with how many notes use it.", json!({}), &[], true, false),
        tool("tasks", "Tasks", "Tasks (`- [ ]` lines) across the vault, as path:line [status] text.",
            json!({
                "status": json!({ "type": "string", "enum": ["open", "done", "all"], "description": "Default open" }),
                "due_before": s("Only tasks due on or before this date, YYYY-MM-DD"),
                "path": s("Only in this note or folder"),
                "tag": s("Only tasks in notes with this tag, or carrying it"),
            }), &[], true, false),
        tool("daily_note", "Read a daily note", "The daily note for a date (its path even when it doesn't exist yet), and that day's events from Google Calendar when it is connected.",
            json!({ "date": date }), &["date"], true, false),
        tool("history", "History", "Every revision of a file, newest first, with who made it (laptop, phone, claude) and when; deleted files too.",
            json!({ "path": path }), &["path"], true, false),
        tool("read_version", "Read a revision", "The text of one revision of a file.",
            json!({ "path": s("The vault path"), "rev": n("The revision") }), &["path", "rev"], true, false),
        tool("read_attachment", "Read an attachment", "An image attachment as an image (up to 5 MB); other files as their size.",
            json!({ "path": path }), &["path"], true, false),
        tool("read_table", "Read a table", "A .csv or .tsv file's rows, with its rev and row count. Rows count from 1 after the header, which is always shown.",
            json!({ "path": s("The vault path, e.g. Finance/Ledger.csv"), "start_row": n("First row, from 1"), "end_row": n("Last row, inclusive") }),
            &["path"], true, false),
        tool("append_rows", "Append rows to a table", "Add rows at the end of a .csv or .tsv file. Each row needs the header's number of fields; quote a field holding the delimiter.",
            json!({ "path": s("The vault path"), "rows": s("The rows, one per line, in the file's format") }), &["path", "rows"], false, false),
        tool("edit_table", "Edit a table", "Replace an exact passage of a .csv or .tsv file (a cell, a row). old_text must match exactly once (or set replace_all). Read the table first.",
            json!({ "path": s("The vault path"), "old_text": s("The exact text to replace"), "new_text": s("What replaces it"),
                   "replace_all": json!({ "type": "boolean", "description": "Replace every occurrence" }) }),
            &["path", "old_text", "new_text"], false, false),
        tool("write_table", "Write a table", "Create a .csv or .tsv file, or replace one whole. Replacing needs expected_rev, the rev read_table showed.",
            json!({ "path": s("The vault path, e.g. Finance/Ledger.csv"), "content": s("The whole file, header first"),
                   "expected_rev": n("The rev read_table showed; leave out to create") }),
            &["path", "content"], false, true),
        tool("edit_note", "Edit a note", "Replace an exact passage of a note with new text. old_text must match exactly once (or set replace_all). Read the note first.",
            json!({ "path": path, "old_text": s("The exact text to replace"), "new_text": s("What replaces it"),
                   "replace_all": json!({ "type": "boolean", "description": "Replace every occurrence" }) }),
            &["path", "old_text", "new_text"], false, false),
        tool("write_note", "Write a note", "Create a note, or replace one whole. Replacing needs expected_rev, the rev you read, so an edit made since is never clobbered.",
            json!({ "path": s("The vault path, e.g. Ideas/New idea.md"), "content": s("The whole text"),
                   "expected_rev": n("The rev read_note showed; leave out to create") }),
            &["path", "content"], false, true),
        tool("append_note", "Append to a note", "Add text at the end of a note (creating it if missing), or at the end of the section under a heading.",
            json!({ "path": path, "text": s("The text to add"), "heading": s("Add under this heading instead of at the end") }),
            &["path", "text"], false, false),
        tool("append_daily", "Append to a daily note", "Add text to the daily note for a date, creating it from the daily template if needed.",
            json!({ "date": date, "text": s("The text to add"), "heading": s("Add under this heading") }),
            &["date", "text"], false, false),
        tool("set_task", "Tick or untick a task", "Set the task on a line of a note done or not done.",
            json!({ "path": path, "line": n("The task's line, from 1 (as tasks and read_note show)"),
                   "done": json!({ "type": "boolean" }) }), &["path", "line", "done"], false, false),
        tool("rename_note", "Rename or move a note", "Rename or move a note or file, rewriting every link to it across the vault in one change.",
            json!({ "from": path, "to": s("The new vault path, e.g. Archive/Old idea.md") }), &["from", "to"], false, false),
        tool("delete_note", "Delete a note", "Delete a note or file. It stays in history: restore_version brings it back.",
            json!({ "path": path }), &["path"], false, true),
        tool("restore_version", "Restore a revision", "Put an earlier revision's text back as a new revision (history is never rewritten). Works for deleted files.",
            json!({ "path": s("The vault path"), "rev": n("The revision to bring back") }), &["path", "rev"], false, true),
    ];
    #[cfg(feature = "xlsx")]
    all.extend(xlsx_tools());
    all
}

#[cfg(feature = "xlsx")]
fn xlsx_tools() -> Vec<Value> {
    let path = s("The vault path, e.g. Finance/Budget.xlsx");
    let sheet = s("The sheet's name (default: the first sheet)");
    let rows = json!({ "type": "array", "description": "Rows, each an array of cell inputs (text, numbers, or formulas starting with =)",
                       "items": { "type": "array", "items": { "type": ["string", "number", "boolean", "null"] } } });
    vec![
        tool("read_spreadsheet", "Read a spreadsheet", "A .xlsx workbook: its sheets and used ranges, then one sheet's displayed values as a grid (column letters across, row numbers down) and its formulas. Large sheets are cut at a whole row; pass a range for the rest.",
            json!({ "path": path, "sheet": sheet, "range": s("Only these cells, e.g. A1:F40") }), &["path"], true, false),
        tool("set_cells", "Set cells", "Set cells in a .xlsx sheet as if typed (= starts a formula; an empty string clears the cell), recalculate, and save. Answers with what each cell now shows.",
            json!({ "path": path, "sheet": sheet,
                    "cells": json!({ "type": "object", "description": "Cell address to input, e.g. {\"B4\": \"=SUM(B1:B3)\", \"A5\": \"Groceries\"}",
                                     "additionalProperties": { "type": ["string", "number", "boolean", "null"] } }),
                    "expected_rev": n("The rev read_spreadsheet showed; refused if it changed since") }),
            &["path", "cells"], false, false),
        tool("append_spreadsheet_rows", "Append spreadsheet rows", "Add rows under the used part of a .xlsx sheet, from its first used column; formulas recalculate.",
            json!({ "path": path, "sheet": sheet, "rows": rows.clone() }), &["path", "rows"], false, false),
        tool("create_spreadsheet", "Create a spreadsheet", "Make a new .xlsx workbook with named sheets and their rows.",
            json!({ "path": path, "sheets": json!({ "type": "array", "description": "The sheets, in order",
                    "items": { "type": "object", "properties": { "name": s("The sheet's name"), "rows": rows }, "required": ["name"] } }) }),
            &["path", "sheets"], false, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on_ready as run;
    use crate::mem::MemStore;

    fn vault(files: &[(&str, &str)]) -> Vault<MemStore> {
        let mut v = Vault::new(MemStore::default());
        let ops = files
            .iter()
            .map(|(p, t)| Operation::Put { path: p.to_string(), pre: Pre::Absent, bytes: t.as_bytes().to_vec() })
            .collect();
        run(v.apply(ops, "laptop", 1)).unwrap();
        v
    }

    fn call(m: &mut Mcp, v: &mut Vault<MemStore>, name: &str, args: Value) -> (bool, String) {
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": args } });
        let r = run(m.handle(v, req.to_string().as_bytes(), 1000));
        let body: Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        let result = &body["result"];
        let err = result["isError"].as_bool().unwrap_or(false);
        (err, result["content"][0]["text"].as_str().unwrap_or_default().to_string())
    }

    fn ok(m: &mut Mcp, v: &mut Vault<MemStore>, name: &str, args: Value) -> String {
        let (err, t) = call(m, v, name, args);
        assert!(!err, "{name} failed: {t}");
        t
    }

    fn file(v: &Vault<MemStore>, p: &str) -> Option<String> {
        run(v.read(p)).unwrap().map(|(_, b)| String::from_utf8(b).unwrap())
    }

    #[test]
    fn handshake() {
        let mut m = Mcp::new();
        let mut v = vault(&[]);
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } });
        let r = run(m.handle(&mut v, init.to_string().as_bytes(), 0));
        let b: Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        assert_eq!(b["result"]["protocolVersion"], "2025-06-18");
        assert!(b["result"]["instructions"].as_str().unwrap().contains("LOCAL date"));
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(run(m.handle(&mut v, note.to_string().as_bytes(), 0)), Reply { status: 202, body: None });
        let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let b: Value = serde_json::from_str(run(m.handle(&mut v, list.to_string().as_bytes(), 0)).body.as_deref().unwrap()).unwrap();
        assert_eq!(b["result"]["tools"].as_array().unwrap().len(), tools().len());
        let bad = run(m.handle(&mut v, b"[1]", 0));
        assert!(bad.body.unwrap().contains("-32600"));
        let unknown = json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" });
        assert!(run(m.handle(&mut v, unknown.to_string().as_bytes(), 0)).body.unwrap().contains("-32601"));
    }

    #[test]
    fn reads() {
        let mut m = Mcp::new();
        let mut v = vault(&[
            ("Ideas/Alpha.md", "---\ntags: [project]\n---\n# Alpha\nSee [[Beta]] and [[Missing]].\n- [ ] call Bob 📅 2026-10-20\n"),
            ("Beta.md", "beta text with apples\n- [x] done thing\n"),
            ("pic.png", "\u{89}PNG"),
        ]);
        let t = ok(&mut m, &mut v, "read_note", json!({ "path": "Alpha" }));
        assert!(t.starts_with("path: Ideas/Alpha.md\nrev: 1"), "{t}");
        assert!(t.contains("links to: Beta.md, Missing (unresolved)"), "{t}");
        assert!(ok(&mut m, &mut v, "read_note", json!({ "path": "Beta.md" })).contains("linked from: Ideas/Alpha.md"));
        assert!(ok(&mut m, &mut v, "search", json!({ "query": "apples" })).starts_with("Beta.md"));
        assert_eq!(ok(&mut m, &mut v, "search", json!({ "query": "#project" })), "Ideas/Alpha.md");
        assert!(ok(&mut m, &mut v, "find_notes", json!({ "query": "alp" })).starts_with("Ideas/Alpha.md"));
        assert_eq!(ok(&mut m, &mut v, "list_folder", json!({})), "Ideas/\nBeta.md  (39 bytes, rev 1)\npic.png  (5 bytes, rev 1)");
        assert_eq!(ok(&mut m, &mut v, "tasks", json!({})), "Ideas/Alpha.md:6  [ ] call Bob 📅 2026-10-20");
        assert_eq!(ok(&mut m, &mut v, "tasks", json!({ "due_before": "2026-10-01" })), "no tasks");
        assert!(ok(&mut m, &mut v, "unresolved_links", json!({})).contains("[[Missing]] in Ideas/Alpha.md"));
        assert!(ok(&mut m, &mut v, "tags", json!({})).contains("#project (1)"));
        let (err, t) = call(&mut m, &mut v, "read_note", json!({ "path": "Nope" }));
        assert!(err && t.contains("find_notes"));
        let req = json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "read_attachment", "arguments": { "path": "pic.png" } } });
        let body = run(m.handle(&mut v, req.to_string().as_bytes(), 0)).body.unwrap();
        assert!(body.contains("\"image/png\""), "{body}");
    }

    #[test]
    fn edits_are_revisions_by_claude() {
        let mut m = Mcp::new();
        let mut v = vault(&[("N.md", "one\ntwo\nthree\n")]);
        ok(&mut m, &mut v, "edit_note", json!({ "path": "N", "old_text": "two", "new_text": "TWO" }));
        assert_eq!(file(&v, "N.md").as_deref(), Some("one\nTWO\nthree\n"));
        let h = v.history("N.md").unwrap();
        assert_eq!((h.versions[0].rev, h.versions[0].device.as_str()), (2, "claude"));
        let (err, t) = call(&mut m, &mut v, "edit_note", json!({ "path": "N", "old_text": "zzz", "new_text": "x" }));
        assert!(err && t.contains("not found"));
        // write_note needs the rev it read, and refuses a stale one.
        let (err, t) = call(&mut m, &mut v, "write_note", json!({ "path": "N.md", "content": "new\n" }));
        assert!(err && t.contains("expected_rev"), "{t}");
        let (err, _) = call(&mut m, &mut v, "write_note", json!({ "path": "N.md", "content": "new\n", "expected_rev": 1 }));
        assert!(err);
        ok(&mut m, &mut v, "write_note", json!({ "path": "N.md", "content": "new\n", "expected_rev": 2 }));
        ok(&mut m, &mut v, "write_note", json!({ "path": "Ideas/Fresh.md", "content": "fresh\n" }));
        assert_eq!(file(&v, "Ideas/Fresh.md").as_deref(), Some("fresh\n"));
        // The index follows an edit made by a device in between.
        run(v.apply(vec![Operation::Put { path: "N.md".into(), pre: Pre::Rev(3), bytes: b"from the laptop\n".to_vec() }], "laptop", 2)).unwrap();
        assert!(ok(&mut m, &mut v, "read_note", json!({ "path": "N" })).ends_with("from the laptop\n"));
    }

    #[test]
    fn daily_note_carries_the_days_events() {
        struct Fake;
        impl AgendaSource for Fake {
            async fn agenda(&self, date: &str) -> Option<crate::agenda::Agenda> {
                Some(crate::agenda::Agenda {
                    date: date.into(),
                    connected: true,
                    time_zone: "Europe/Berlin".into(),
                    events: vec![crate::agenda::Event { id: "p/1".into(), title: "Standup".into(), start: "09:00".into(), end: "09:15".into(), ..Default::default() }],
                })
            }
        }
        let mut v = vault(&[("2026-10-13.md", "- met Ana\n")]);
        let mut m = Mcp::new();
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "daily_note", "arguments": { "date": "2026-10-13" } } });
        let r = run(m.handle_with(&mut v, req.to_string().as_bytes(), 1000, &Fake));
        let body: Value = serde_json::from_str(r.body.as_deref().unwrap()).unwrap();
        let t = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(t.contains("- met Ana\n\n---\nagenda (Google Calendar, Europe/Berlin; not part of the note):\n09:00–09:15  Standup"), "{t}");
        // Without a source, the note alone.
        assert!(!ok(&mut m, &mut v, "daily_note", json!({ "date": "2026-10-13" })).contains("agenda"));
    }

    #[test]
    fn appends_tasks_and_daily_notes() {
        let mut m = Mcp::new();
        let mut v = vault(&[
            ("Plan.md", "# Plan\n\n## Today\n- [ ] a\n\n## Later\n- [ ] b\n"),
            (".cce/vault.kdl", "daily {\n    folder \"Daily\"\n    template \"Templates/Day\"\n}\n"),
            ("Templates/Day.md", "# {{title}}\n\n## Log\n"),
        ]);
        ok(&mut m, &mut v, "append_note", json!({ "path": "Plan", "text": "- [ ] c", "heading": "Today" }));
        assert_eq!(file(&v, "Plan.md").as_deref(), Some("# Plan\n\n## Today\n- [ ] a\n- [ ] c\n\n## Later\n- [ ] b\n"));
        ok(&mut m, &mut v, "set_task", json!({ "path": "Plan", "line": 5, "done": true }));
        assert!(file(&v, "Plan.md").unwrap().contains("- [x] c"));
        let t = ok(&mut m, &mut v, "append_daily", json!({ "date": "2026-10-13", "text": "- met Ana", "heading": "Log" }));
        assert!(t.starts_with("created and appended to Daily/2026-10-13.md"), "{t}");
        assert_eq!(file(&v, "Daily/2026-10-13.md").as_deref(), Some("# 2026-10-13\n\n## Log\n- met Ana\n"));
        ok(&mut m, &mut v, "append_daily", json!({ "date": "2026-10-13", "text": "- lunch" }));
        assert!(ok(&mut m, &mut v, "daily_note", json!({ "date": "2026-10-13" })).ends_with("- met Ana\n- lunch\n"));
        let (err, t) = call(&mut m, &mut v, "append_daily", json!({ "text": "x" }));
        assert!(err && t.contains("local date"));
    }

    #[test]
    fn rename_rewrites_links_and_moves_on_the_server() {
        let mut m = Mcp::new();
        let mut v = vault(&[("Beta.md", "beta\n"), ("Alpha.md", "see [[Beta]]\n"), ("img/pic.png", "PNGBYTES"), ("C.md", "![[pic.png]]\n")]);
        let t = ok(&mut m, &mut v, "rename_note", json!({ "from": "Beta", "to": "Archive/Gamma.md" }));
        assert!(t.contains("links rewritten in 1 file"), "{t}");
        assert_eq!(file(&v, "Alpha.md").as_deref(), Some("see [[Gamma]]\n"));
        assert_eq!(file(&v, "Archive/Gamma.md").as_deref(), Some("beta\n"));
        assert!(file(&v, "Beta.md").is_none());
        let feed = v.changes(4, 100).unwrap();
        assert!(feed.changes.iter().any(|c| c.op == crate::proto::Op::Move && c.from.as_deref() == Some("Beta.md")));
        // An attachment moves on the server; its bytes never pass through.
        ok(&mut m, &mut v, "rename_note", json!({ "from": "img/pic.png", "to": "media/pic.png" }));
        assert_eq!(file(&v, "media/pic.png").as_deref(), Some("PNGBYTES"));
    }

    #[test]
    fn delete_and_restore() {
        let mut m = Mcp::new();
        let mut v = vault(&[("Gone.md", "keep me\n")]);
        ok(&mut m, &mut v, "delete_note", json!({ "path": "Gone" }));
        assert!(file(&v, "Gone.md").is_none());
        let h = ok(&mut m, &mut v, "history", json!({ "path": "Gone.md" }));
        assert!(h.starts_with("rev 2") && h.contains("deleted") && h.contains("by claude"), "{h}");
        ok(&mut m, &mut v, "restore_version", json!({ "path": "Gone.md", "rev": 1 }));
        assert_eq!(file(&v, "Gone.md").as_deref(), Some("keep me\n"));
        assert!(ok(&mut m, &mut v, "read_version", json!({ "path": "Gone.md", "rev": 1 })).ends_with("keep me\n"));
    }

    #[test]
    fn tables_read_and_write_with_a_rev_and_whole_rows() {
        let mut m = Mcp::new();
        let mut v = vault(&[("Finance/Ledger.csv", "Date,Payee,Amount\n2026-10-01,Rent,1200\n"), ("N.md", "see [[Ledger.csv]]\n")]);
        let t = ok(&mut m, &mut v, "read_table", json!({ "path": "Finance/Ledger.csv" }));
        assert_eq!(t, "path: Finance/Ledger.csv\nrev: 1\nrows: 1 (not counting the header)\n---\nDate,Payee,Amount\n2026-10-01,Rent,1200\n");
        assert!(ok(&mut m, &mut v, "read_attachment", json!({ "path": "Finance/Ledger.csv" })).contains("use read_table"));

        let t = ok(&mut m, &mut v, "append_rows", json!({ "path": "Finance/Ledger.csv", "rows": "2026-10-03,\"Shop, Inc\",42\n2026-10-04,Gas,30" }));
        assert_eq!(t, "appended 2 rows to Finance/Ledger.csv, now rev 2");
        let (err, t) = call(&mut m, &mut v, "append_rows", json!({ "path": "Finance/Ledger.csv", "rows": "2026-10-05,Oops" }));
        assert!(err && t.contains("has 2 fields; the header has 3") && t.contains("nothing was written"), "{t}");

        ok(&mut m, &mut v, "edit_table", json!({ "path": "Finance/Ledger.csv", "old_text": "Gas,30", "new_text": "Gas,31" }));
        let (err, t) = call(&mut m, &mut v, "edit_table", json!({ "path": "Finance/Ledger.csv", "old_text": "Gas,31", "new_text": "Gas" }));
        assert!(err && t.contains("fields"), "{t}");
        assert_eq!(
            file(&v, "Finance/Ledger.csv").as_deref(),
            Some("Date,Payee,Amount\n2026-10-01,Rent,1200\n2026-10-03,\"Shop, Inc\",42\n2026-10-04,Gas,31\n")
        );
        let t = ok(&mut m, &mut v, "read_table", json!({ "path": "Finance/Ledger.csv", "start_row": 2, "end_row": 2 }));
        assert!(t.contains("rev: 3\nrows: 3") && t.ends_with("---\nDate,Payee,Amount\n2026-10-03,\"Shop, Inc\",42"), "{t}");

        // Replacing needs the rev read; a stale one is refused.
        let (err, t) = call(&mut m, &mut v, "write_table", json!({ "path": "Finance/Ledger.csv", "content": "a,b\n" }));
        assert!(err && t.contains("pass expected_rev"), "{t}");
        let (err, t) = call(&mut m, &mut v, "write_table", json!({ "path": "Finance/Ledger.csv", "content": "a,b\n", "expected_rev": 2 }));
        assert!(err && t.contains("is at rev 3"), "{t}");
        assert_eq!(ok(&mut m, &mut v, "write_table", json!({ "path": "Finance/Ledger.csv", "content": "a,b\n1,2\n", "expected_rev": 3 })), "replaced Finance/Ledger.csv (1 rows), now rev 4");
        assert!(ok(&mut m, &mut v, "write_table", json!({ "path": "Home/Appliances.tsv", "content": "Name\tModel\nFridge\tX1\n" })).starts_with("created"));
        let (err, t) = call(&mut m, &mut v, "write_table", json!({ "path": "Notes.md", "content": "a\n" }));
        assert!(err && t.contains("not a table"), "{t}");

        // Every write is a revision by claude, and the notes still read.
        assert!(ok(&mut m, &mut v, "history", json!({ "path": "Finance/Ledger.csv" })).starts_with("rev 4  1970-01-01 00:00 UTC  by claude"));
        assert!(ok(&mut m, &mut v, "read_note", json!({ "path": "N" })).contains("links to: Finance/Ledger.csv"));
        assert!(ok(&mut m, &mut v, "list_folder", json!({ "path": "Home" })).contains("Home/Appliances.tsv"));
    }

    #[test]
    fn a_ragged_table_still_takes_a_good_edit() {
        let mut m = Mcp::new();
        let mut v = vault(&[("T.csv", "a,b\n1\n2,3\n")]);
        assert!(ok(&mut m, &mut v, "read_table", json!({ "path": "T.csv" })).contains("warning: row 1 (line 2) has 1 fields"));
        ok(&mut m, &mut v, "append_rows", json!({ "path": "T.csv", "rows": "4,5" }));
        ok(&mut m, &mut v, "edit_table", json!({ "path": "T.csv", "old_text": "1\n", "new_text": "1,0\n" }));
        assert_eq!(file(&v, "T.csv").as_deref(), Some("a,b\n1,0\n2,3\n4,5\n"));
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn workbooks_create_read_set_and_append() {
        let mut m = Mcp::new();
        let mut v = vault(&[]);
        let t = ok(&mut m, &mut v, "create_spreadsheet", json!({ "path": "Finance/Budget.xlsx", "sheets": [
            { "name": "Budget", "rows": [["Item", "Cost"], ["Rent", 1200], ["Food", "300"], ["Total", "=SUM(B2:B3)"]] },
            { "name": "Notes" },
        ] }));
        assert_eq!(t, "created Finance/Budget.xlsx (2 sheets), rev 1");
        let (err, t) = call(&mut m, &mut v, "create_spreadsheet", json!({ "path": "Finance/Budget.xlsx", "sheets": [{ "name": "S" }] }));
        assert!(err && t.contains("exists"), "{t}");

        let t = ok(&mut m, &mut v, "read_spreadsheet", json!({ "path": "Finance/Budget.xlsx" }));
        assert!(t.starts_with("path: Finance/Budget.xlsx\nrev: 1\nsheets: Budget (A1:B4), Notes (empty)\nsheet: Budget\n---\n\tA\tB\n1\tItem\tCost\n"), "{t}");
        assert!(t.contains("4\tTotal\t1500") && t.ends_with("formulas:\nB4 =SUM(B2:B3)"), "{t}");

        let t = ok(&mut m, &mut v, "append_spreadsheet_rows", json!({ "path": "Finance/Budget.xlsx", "rows": [["Gas", 40]] }));
        assert_eq!(t, "appended rows 5-5 to Finance/Budget.xlsx, now rev 2");
        let (err, t) = call(&mut m, &mut v, "set_cells", json!({ "path": "Finance/Budget.xlsx", "cells": { "B4": "=SUM(B2:B3)+B5" }, "expected_rev": 1 }));
        assert!(err && t.contains("is at rev 2"), "{t}");
        let t = ok(&mut m, &mut v, "set_cells", json!({ "path": "Finance/Budget.xlsx", "cells": { "B4": "=SUM(B2:B3)+B5" } }));
        assert_eq!(t, "set 1 cell in Finance/Budget.xlsx, now rev 3\nB4 = 1540 (=SUM(B2:B3)+B5)");
        assert!(ok(&mut m, &mut v, "read_spreadsheet", json!({ "path": "Finance/Budget.xlsx", "sheet": "budget", "range": "A5:B5" })).ends_with("---\n\tA\tB\n5\tGas\t40"));
        let (err, t) = call(&mut m, &mut v, "read_spreadsheet", json!({ "path": "Finance/Budget.xlsx", "sheet": "Nope" }));
        assert!(err && t.contains("the sheets are Budget, Notes"), "{t}");
        assert!(ok(&mut m, &mut v, "history", json!({ "path": "Finance/Budget.xlsx" })).starts_with("rev 3  1970-01-01 00:00 UTC  by claude"));
        assert!(ok(&mut m, &mut v, "read_attachment", json!({ "path": "Finance/Budget.xlsx" })).contains("use read_spreadsheet"));
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn a_workbook_with_parts_ironcalc_drops_is_read_but_never_written() {
        use std::io::Write;
        let model = crate::xlsx::create(&[("S".into(), vec![vec!["a".into()]])]).unwrap();
        let plain = crate::xlsx::save(&model).unwrap();
        // The same workbook with a chart part added.
        let mut src = zip::ZipArchive::new(std::io::Cursor::new(plain)).unwrap();
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..src.len() {
            out.raw_copy_file(src.by_index(i).unwrap()).unwrap();
        }
        out.start_file("xl/charts/chart1.xml", zip::write::FileOptions::default()).unwrap();
        out.write_all(b"<c:chartSpace/>").unwrap();
        let bytes = out.finish().unwrap().into_inner();
        let mut v = Vault::new(MemStore::default());
        run(v.apply(vec![Operation::Put { path: "C.xlsx".into(), pre: Pre::Absent, bytes: bytes.clone() }], "laptop", 1)).unwrap();
        let mut m = Mcp::new();
        assert!(ok(&mut m, &mut v, "read_spreadsheet", json!({ "path": "C.xlsx" })).ends_with("1\ta"));
        let (err, t) = call(&mut m, &mut v, "set_cells", json!({ "path": "C.xlsx", "cells": { "A2": "b" } }));
        assert!(err && t.contains("has charts, which saving it here would drop"), "{t}");
        let (err, _) = call(&mut m, &mut v, "append_spreadsheet_rows", json!({ "path": "C.xlsx", "rows": [["b"]] }));
        assert!(err);
        assert_eq!(run(v.read("C.xlsx")).unwrap().unwrap().1, bytes);
    }
}
