# cce-sync

The vault's sync engine. One library shared by the server
(`cce-sync-server`, a Cloudflare Durable Object), the laptop (this crate's
`cce-sync` daemon) and the phone (`cce-vault-web`, which drives `client`
from the browser, with lazy attachments on top). The plan is the doc
"cce-sync: design"; phases 1-6 are here (3: the MCP tools Claude's
connector calls, served by cce-sync-server; 4: the phone, whose side is in
cce-vault-web and whose sign-in and static site are in cce-sync-server;
6: the Google bridge's rules, `agenda` and `gtask`, which the server runs).

Read `../cce-compositor/WORKSPACE.md` first (multi-repo rule, standalone
builds, concurrent sessions) and `../cce-vault/CLAUDE.md` for the vault's
own rules (paths, hidden files, link resolution).

## Layout

The library (no I/O, builds for wasm32):

| Module | What it owns |
| --- | --- |
| `proto` | The wire protocol: endpoints (module doc), JSON shapes, headers, `Pre` (If-Match / If-None-Match), `Refusal`s and their HTTP status, path rules (`check_path`, `fold`, `encode_path`), `hash` (SHA-256 hex), `Kind`, the note-text budget (`Status`) |
| `merge` | `merge3`: the three-way line merge of a note |
| `core` | The server's rules over a `Store`: `Vault::apply` (a batch of puts, deletes, moves, all or none), `changes`, `listing`, `read`, `history` |
| `client` | A device's rules: `SyncState` (seq + each file's base rev and hash), what a listing or a feed page means locally (`Step`s), what to push, what each answer means. Two-device scenarios run against `core` in its tests |
| `mcp` (feature `mcp`) | The MCP server Claude's connector calls: JSON-RPC (stateless Streamable HTTP), the tools, an in-memory `cce_vault::Index` rebuilt when the seq moves, writes as one batch from the device `claude` |
| `agenda` | The Google bridge's agenda: which of Google Calendar's events fall on a local day (cut by local time text, no zone database), `Agenda::lines` for a daily note's ```` ```agenda ```` block |
| `gtask` | `#gtask` lines kept in step with Google Tasks: `scan` a note, `plan` the acts against Google's tasks and the bases, `edit_note` the lines (found by id, or exact text) |
| `table` | `.csv`/`.tsv` files for the MCP tools: the RFC 4180 reader (`records`), `check` (a header, every row its width), `appended` |
| `xlsx` (feature `xlsx`) | `.xlsx` workbooks for the MCP tools, on IronCalc 0.8.3 (cce-spreadsheet's engine): load/save, `create`, `render` (a grid of shown values + the formulas), `set`, `append`, A1 addresses, and `unkept` (the parts a save would drop) |
| `mem` | `MemStore`, the in-memory `Store` the tests run on |
| `tests.rs` | Scenarios for `core` |

The binary (`daemon` feature, on by default; the wasm users depend with
`default-features = false`, cce-sync-server adding `features = ["mcp"]`):

| File | What it owns |
| --- | --- |
| `main.rs` | Commands, config (`sync { server device }` in config.kdl, the vault from cce-vault's config), the token (env, keyring, file) |
| `daemon.rs` | The service: event loop (watcher, live socket, timer, backoff), the status socket, the state file, conflict notifications |
| `pass.rs` | One pass: pull, carry out steps, push rounds; `restore` |
| `local.rs` | The folder: what syncs, hashes with a size+mtime cache, conditional writes (cce-vault's atomic write), removes that prune empty folders |
| `http.rs` | The server's API, blocking (ureq) |
| `import.rs` | `import`: a folder up, create-only; an old vault's `.obsidian/` settings → `.cce/vault.kdl` (the one place anything reads `.obsidian/`, for bringing a vault over) |
| `cce-sync.service` | The user unit (`WantedBy=default.target`, see below) |

## Invariants — each was a design decision, keep them

Server side (`core`):

- **No edit is lost silently.** Every device write carries a precondition
  (`Pre::Rev` or `Pre::Absent`); the server refuses a stale one with a
  `Conflict` unless it can merge. `Pre::Any` is only for the server's own
  writers (Claude's tools, phase 3). Never add an overwrite mode for devices.
- **A merge never drops or reorders a line either side wrote.** `merge3`
  refuses when the two sides' changes *touch*, not only when they overlap
  (two appends to the same daily note conflict). The same change on both
  sides is taken once. `a_merge_never_drops_a_line_either_side_wrote` is
  the property test; a merge change must keep it passing.
- **Only notes merge** (`Kind::merges`). Canvases are JSON: a line merge can
  produce invalid JSON.
- **Revisions are never reused.** A delete is a revision (a tombstone row
  keeps the counter), so an old `If-Match` can never succeed against a
  re-created file.
- **Writing bytes a file already has is `unchanged`**, not a new revision:
  a retried request is harmless.
- **Paths are unique ignoring case** (`fold`): link resolution is
  case-insensitive. A move to a new case of its own name is allowed.
- **Hidden paths don't sync, except `.cce/`** (the vault settings,
  `.cce/vault.kdl`). Same rule as cce-vault's "hidden means ignored".
- **A batch lands whole or not at all.** `core` stages against an overlay
  and calls `Store::commit` once; a store must make that one call atomic
  (the Durable Object does by not awaiting inside it). Blobs are written
  while staging — content-addressed, so a refused batch only leaves
  unreferenced blobs.
- **One batch at a time.** `Vault` assumes no concurrent `apply`; the
  server holds a lock around it.

Device side (`client`, and the daemon carrying it out):

- **A remote change is applied only to a file not edited here.** An edited
  file is pushed from its base instead, so the server merges or refuses.
- **Every local write is conditional on the hash the plan saw**, checked
  again right before the write (a download takes time). A file that changed
  meanwhile is left for the next pass. This is what keeps an edit in
  cce-notes from being overwritten.
- **Refused: the server's version takes the path; this device's is kept as
  `Name (conflict YYYY-MM-DD HHMM).md`** beside it, and uploaded.
- **A merge is downloaded only over the exact bytes that were sent.** If the
  file changed again, the base stays put, so the next push merges from the
  right base (`a_merge_that_arrives_after_another_edit_still_converges`).
- **The state is saved after the pass**, never during: a crash re-runs the
  pull from the old seq, which is harmless because every step is
  conditional.
- **Own writes are recognised by hash, not timing.** The watcher wakes a
  pass after our own downloads; it finds nothing to do.
- **A first pass over a folder that already matches the server writes
  nothing** (it adopts each file whose hash matches). Phase 2's gate.

MCP (`mcp`):

- **Claude's writes are ordinary revisions from the device `claude`.** They
  go through `Vault::apply` like a device's, so history, `restore_version`
  and the laptop's conflict rules all apply. They use `Pre::Any` because
  the tool runs under the vault lock on the current text; `write_note`
  makes Claude name the rev it read before replacing a whole note.
- **Attachments are placeholders in the index** (empty bytes, so links
  resolve). The journal-to-batch step (`ops_from_journal`) turns a rename
  into a server-side `Move`, so an attachment's bytes never pass through
  the index, and refuses any other write of one. Never upload a
  placeholder.
- **The index is rebuilt when the vault's seq moved**, and kept current
  after Claude's own commit. A refused commit drops it (it holds writes the
  vault does not).
- **Dates come from Claude.** The server runs on UTC; the daily-note tools
  require the user's local date, and the server instructions say so.
- **Tables go around the index, conditionally.** `.csv`/`.tsv` are
  attachments (placeholders in the index, no merge), so `read_table`,
  `append_rows`, `edit_table` and `write_table` read the store and write
  one `Put` with `Pre::Rev` of the rev they read (`Pre::Absent` to
  create), never `Pre::Any`. A write must leave a well-formed table well
  formed (`guard_table`); one already ragged by hand only has to stay
  readable, so a bad row elsewhere never blocks a good edit.
- **A workbook IronCalc can't keep whole is never written.** IronCalc
  drops charts, pictures, pivot tables, Excel tables, comments, macros,
  data validation and hyperlinks on save; `set_cells` and
  `append_spreadsheet_rows` refuse a workbook that has any (`unkept`) and
  point at cce-spreadsheet. Reading it is fine. Workbook writes are
  `Pre::Rev` of the rev read, like tables. Only the server enables
  `xlsx`, and its wasm32 build patches `ironcalc` to a vendored copy
  without zip's C codecs (`cce-sync-server/vendor/ironcalc/VENDORED.md`);
  keep the version equal to cce-spreadsheet's.
- **`daily_note` carries the day's agenda** from the host's
  `AgendaSource` (`handle_with`), after the note and marked as not part
  of it; Claude must not write it into the note.
- **Daily-note settings come from cce-vault** (`DailyConfig::load`, which
  reads `daily { folder format template }` in `.cce/vault.kdl`), the same
  reader cce-notes and the phone use. Never parse the file here again.

Google bridge (`agenda`, `gtask`; the server runs them):

- **Google owns events; the vault only points at them.** Nothing from the
  calendar is written into notes: the ```` ```agenda ```` block stays empty
  in the file and is filled when drawn.
- **A task is tied to Google by its id on the line** (`[gtask:: id]`),
  never by its text. Every line edit finds its line by that id (or, for
  attaching the id, by the exact text the plan saw) and is skipped when
  the line is gone; a task created in Google whose id did not reach its
  line is adopted by (path, text) next pass, never created twice.
- **Fields sync against a base**, changed on one side copied to the other,
  a clash going to the later change (the note's last write against
  Google's `updated`). A base moves only once both sides hold it: after a
  line edit lands, not when it is planned.
- **Deleting is asymmetric on purpose**: deleted in Google, the line keeps
  the task (untagged); the line deleted, the Google task goes. A removed
  tag unlinks without deleting anything.

## The daemon

```sh
cce-sync login                        # token into the keyring (secret service)
cce-sync sync --prefer-local          # ONCE, first, if the folder changed since the import
systemctl --user enable --now cce-sync.service
cce-sync status                       # the daemon, then the server
journalctl --user -u cce-sync -f
```

- Wakes on the folder changing (notify, debounced 600 ms, at most 4 s), on
  a live-socket notice from the server, every 5 minutes, and on `cce-sync
  sync`. A failed pass backs off (5 s doubling to 5 min) and shows
  `offline`.
- State: `$XDG_STATE_HOME/cce/sync/<id>.json` (per server + vault pair).
  Socket: `$XDG_RUNTIME_DIR/cce/cce-sync.sock`, lines `status` (JSON),
  `sync`, `ping`, and `agenda YYYY-MM-DD` (the day's events as JSON, or
  `error …`; answered off the loop, for cce-notes, which holds no token);
  it is also the single-instance lock.
- **Not bound to `cce-session.target`**, unlike the GUI daemons: it holds no
  Wayland connection, and the vault should sync whenever the user is logged
  in, as Dropbox did. Conflicts go through `notify-send`.
- During the migration the vault can stay inside the Dropbox folder: Dropbox
  and cce-sync then both write it, and the folder bridges the phone (still
  on Dropbox) and the server. Both are conflict-safe.

**The first pass after an import.** The daemon's first pass has no base
for any file, so where the folder and the server differ it cannot tell
which is newer, and treats it as a conflict: the server's version takes
the path and the folder's is kept as a conflict copy. After an import the
folder kept changing (Obsidian, Dropbox), the folder is the newer side, so
run `cce-sync sync --prefer-local` once before starting the daemon: for
files with no base that differ, the folder's version replaces the server's
(which stays in history). It refuses while the daemon runs, and is never
automatic (`a_folder_newer_than_its_import_can_win_on_request`).

Other commands: `grants` and `revoke <grant>` (what Claude has been
allowed through OAuth, and ending it), `sync` (one pass, or asks the daemon), `history <note>`,
`restore <note> <rev>` (the old bytes become a new revision; history is
never rewritten), `import <dir>`, `agenda [date]` (the day's events from
the bridge).

## Build and test

```sh
cargo test -p cce-sync                       # library, client scenarios, CLI
cargo test -p cce-sync --lib --features xlsx  # plus the workbook tools
cargo build -p cce-sync --lib --no-default-features --target wasm32-unknown-unknown
dev/e2e-daemon.sh                            # two daemons against a fresh `wrangler dev`
```

A sync rule change gets a scenario first: server rules in `src/tests.rs`,
device rules in `client`'s tests. `dev/e2e-daemon.sh` runs two real daemons
(laptop, phone) on generated folders against a local server
(`cce-sync-server`: `rm -rf .wrangler && wrangler dev --port 8787`, with
`SYNC_TOKEN=dev-token-123` in `.dev.vars`). Never point either test at the
deployed server or the real vault.

Install: `make install` (release build, then `ccebuild install --no-build
cce-sync`). `login` needs a secret service (gnome-keyring or KeePassXC);
without one, write the token to `~/.config/cce/cce-sync.token` (mode 600).

## Not done yet (later phases)

The laptop on OAuth instead of the shared token; a status-bar segment
reading the socket; history in
cce-notes; linking a note from its Google Calendar event's description
(the bridge's scope is read-only on Calendar).
