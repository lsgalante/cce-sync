# cce-sync

The vault's sync engine. One library shared by the server
(`cce-sync-server`, a Cloudflare Durable Object), the laptop (this crate's
`cce-sync` binary) and, from phase 4, the phone (`cce-vault-web`). The
plan is the doc "cce-sync: design"; this is phase 1 of it.

Read `../cce-compositor/WORKSPACE.md` first (multi-repo rule, standalone
builds, concurrent sessions) and `../cce-vault/CLAUDE.md` for the vault's
own rules (paths, hidden files, link resolution).

## Layout

| Module | What it owns |
| --- | --- |
| `proto` | The wire protocol: endpoints (module doc), JSON shapes, headers, `Pre` (If-Match / If-None-Match), `Refusal`s and their HTTP status, path rules (`check_path`, `fold`, `encode_path`), `hash` (SHA-256 hex), `Kind`, the note-text budget (`Status`) |
| `merge` | `merge3`: the three-way line merge of a note |
| `core` | The server's rules over a `Store`: `Vault::apply` (a batch of puts, deletes, moves, all or none), `changes`, `listing`, `read`, `history` |
| `mem` | `MemStore`, the in-memory `Store` the tests run on |
| `tests.rs` | Scenarios for `core` |
| `main.rs`, `client.rs`, `import.rs` | The binary (`daemon` feature, on by default): `import`, `status` |

The library has **no I/O** and builds for wasm32; the binary is behind the
`daemon` feature so the wasm users (`default-features = false`) never see
ureq or walkdir.

## Invariants — each was a design decision, keep them

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

## Build and test

```sh
cargo test -p cce-sync                       # library + CLI
cargo build -p cce-sync --lib --no-default-features --target wasm32-unknown-unknown
```

A sync rule change gets a scenario in `src/tests.rs` first.

## The CLI

```sh
export CCE_SYNC_SERVER=https://vault.lucas.co   # or --server
echo '<token>' > ~/.config/cce/cce-sync.token && chmod 600 ~/.config/cce/cce-sync.token
cce-sync import ~/Vault --dry-run    # what it would upload
cce-sync import ~/Vault              # create-only: never overwrites
cce-sync status
```

`import` skips hidden folders, uploads with `If-None-Match: *` in batches
(100 files / 32 MB), and writes `.cce/vault.kdl` from `.obsidian/`'s
daily-notes, app (attachment folder) and templates settings when the folder
has none. Test against a **copy** of the vault and a local server
(`cce-sync-server`'s `dev/e2e.sh`), never the live one.

Install: `make install` (release build, then `ccebuild install --no-build
cce-sync`).

## Not done yet (later phases)

The daemon (watch, push, pull, live socket, conflict copies; phase 2) with
the client state machine generalised from cce-vault-web's `sync`; the token
in the keyring; OAuth (phase 3).
