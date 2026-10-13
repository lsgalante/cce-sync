# cce-sync

The vault's sync engine. One library shared by the server
(`cce-sync-server`, a Cloudflare Durable Object), the laptop (this crate's
`cce-sync` daemon) and, from phase 4, the phone (`cce-vault-web`). The
plan is the doc "cce-sync: design"; phases 1 and 2 are here.

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
| `mem` | `MemStore`, the in-memory `Store` the tests run on |
| `tests.rs` | Scenarios for `core` |

The binary (`daemon` feature, on by default; the wasm users depend with
`default-features = false`):

| File | What it owns |
| --- | --- |
| `main.rs` | Commands, config (`sync { server device }` in config.kdl, the vault from cce-vault's config), the token (env, keyring, file) |
| `daemon.rs` | The service: event loop (watcher, live socket, timer, backoff), the status socket, the state file, conflict notifications |
| `pass.rs` | One pass: pull, carry out steps, push rounds; `restore` |
| `local.rs` | The folder: what syncs, hashes with a size+mtime cache, conditional writes (cce-vault's atomic write), removes that prune empty folders |
| `http.rs` | The server's API, blocking (ureq) |
| `import.rs` | `import`: a folder up, create-only; `.obsidian/` → `.cce/vault.kdl` |
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

## The daemon

```sh
cce-sync login                        # token into the keyring (secret service)
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
  `sync`, `ping`; it is also the single-instance lock.
- **Not bound to `cce-session.target`**, unlike the GUI daemons: it holds no
  Wayland connection, and the vault should sync whenever the user is logged
  in, as Dropbox did. Conflicts go through `notify-send`.
- During the migration the vault can stay inside the Dropbox folder: Dropbox
  and cce-sync then both write it, and the folder bridges the phone (still
  on Dropbox) and the server. Both are conflict-safe.

Other commands: `sync` (one pass, or asks the daemon), `history <note>`,
`restore <note> <rev>` (the old bytes become a new revision; history is
never rewritten), `import <dir>`.

## Build and test

```sh
cargo test -p cce-sync                       # library, client scenarios, CLI
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

OAuth instead of the shared token, and the MCP connector (phase 3); the
phone on this protocol (phase 4); a status-bar segment reading the socket;
history in cce-notes.
