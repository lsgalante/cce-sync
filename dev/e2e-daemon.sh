#!/usr/bin/env bash
# Two cce-sync daemons on two folders against one server: the laptop and,
# standing in for the phone, a second device. Checks that edits travel
# live, merges, conflict copies, deletes, offline catch-up, history and
# restore, and that a first pass over an imported folder writes nothing.
#
#   (cd ../cce-sync-server && rm -rf .wrangler && wrangler dev --port 8787)
#   dev/e2e-daemon.sh [http://127.0.0.1:8787] [token]
#
# Needs an EMPTY server (a fresh `wrangler dev`). Never the deployed one,
# never the real vault: it works on generated folders under a temp dir.
set -euo pipefail

SERVER=${1:-http://127.0.0.1:8787}
TOKEN=${2:-dev-token-123}
HERE=$(cd "$(dirname "$0")" && pwd)
# The cce-sync binary: $CCE_SYNC_CLI, else the workspace's shared target
# (cce-sync is a member of the root workspace), else a standalone build.
CLI=${CCE_SYNC_CLI:-}
if [ -z "$CLI" ]; then
    for c in "$HERE/../../target/debug/cce-sync" "$HERE/../target/debug/cce-sync"; do
        [ -x "$c" ] && { CLI=$c; break; }
    done
fi
[ -x "${CLI:-}" ] || { echo "no cce-sync binary: build it (cargo build -p cce-sync) or set CCE_SYNC_CLI" >&2; exit 1; }
WORK=$(mktemp -d)
PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [ "$fails" -eq 0 ] && [ -z "${KEEP:-}" ]; then rm -rf "$WORK"; else echo "kept $WORK"; fi
}
fails=0
trap cleanup EXIT
ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }

export CCE_SYNC_SERVER=$SERVER CCE_SYNC_TOKEN=$TOKEN
A=$WORK/laptop/vault B=$WORK/phone/vault
mkdir -p "$A" "$B"

# Each device gets its own config, state and runtime dirs (the status
# socket lives in the runtime dir, and it is the single-instance lock).
dev() { # dev <laptop|phone> <cce-sync args...>
    local d=$1; shift
    XDG_CONFIG_HOME=$WORK/$d/config XDG_STATE_HOME=$WORK/$d/state XDG_RUNTIME_DIR=$WORK/$d/run \
        "$CLI" --device "$d" --vault "$WORK/$d/vault" "$@"
}
start() { # start <laptop|phone>: the daemon itself in the background, so $! is its pid
    local d=$1
    mkdir -p "$WORK/$d/run"
    XDG_CONFIG_HOME=$WORK/$d/config XDG_STATE_HOME=$WORK/$d/state XDG_RUNTIME_DIR=$WORK/$d/run \
        "$CLI" --device "$d" --vault "$WORK/$d/vault" daemon 2>>"$WORK/$d.log" &
    PIDS+=($!)
    eval "PID_$d=$!"
    for _ in $(seq 50); do
        # Captured first: under pipefail, grep -q quitting early fails the pipe.
        out=$(dev "$d" status 2>/dev/null || true)
        grep -q 'daemon *\(synced\|pending\)' <<<"$out" && return
        sleep 0.2
    done
    echo "daemon $d did not come up"; cat "$WORK/$d.log"; exit 1
}
stop() { kill "$(eval echo \$PID_$1)"; wait "$(eval echo \$PID_$1)" 2>/dev/null || true; }

# wait_for <description> <command...>: poll for up to 15 s.
wait_for() {
    local what=$1; shift
    for _ in $(seq 75); do
        if "$@" >/dev/null 2>&1; then ok "$what"; return; fi
        sleep 0.2
    done
    bad "$what"
}
same() { [ "$(cat "$1" 2>/dev/null)" = "$2" ]; }
gone() { [ ! -e "$1" ]; }

echo "== import, then a first pass that writes nothing"
mkdir -p "$A/Tasks" "$A/Daily" "$A/.obsidian"
printf -- '- [ ] Call the studio\n' > "$A/Tasks/To Do.md"
printf 'a\nb\nc\nd\ne\nf\ng\n' > "$A/Daily/D.md"
printf '{"attachmentFolderPath":"Attachments"}' > "$A/.obsidian/app.json"
head -c 1200000 /dev/urandom > "$A/photo.jpg"
dev laptop import "$A" | tail -1
# import uploaded .cce/vault.kdl without writing it to the folder; put the
# same bytes there, as the migration does, so the folders match exactly.
mkdir -p "$A/.cce" && curl -s -H "authorization: Bearer $TOKEN" "$SERVER/v1/files/.cce/vault.kdl" > "$A/.cce/vault.kdl"
before=$(find "$A" -type f -newer "$A/photo.jpg" | wc -l)
start laptop
sleep 1
grep -q 'pass:' "$WORK/laptop.log" && bad "the laptop's first pass wrote or sent something: $(grep pass: "$WORK/laptop.log")" || ok "the laptop's first pass changed nothing"
after=$(find "$A" -type f -newer "$A/photo.jpg" | wc -l)
[ "$before" = "$after" ] && ok "no file in the folder was touched" || bad "files touched ($before -> $after)"

echo "== a second device"
start phone
wait_for "the phone gets every file" same "$B/Tasks/To Do.md" "- [ ] Call the studio"
cmp -s "$A/photo.jpg" "$B/photo.jpg" && ok "the photo (R2) arrived intact" || bad "photo differs"

echo "== live edits"
printf -- '- [x] Call the studio\n' > "$A/Tasks/To Do.md"
wait_for "a laptop edit reaches the phone" same "$B/Tasks/To Do.md" "- [x] Call the studio"
mkdir -p "$B/Ideas" && printf 'from the phone\n' > "$B/Ideas/New.md"
wait_for "a phone note reaches the laptop" same "$A/Ideas/New.md" "from the phone"

echo "== offline edits merge"
stop phone
printf 'A\nb\nc\nd\ne\nf\ng\n' > "$A/Daily/D.md"
sleep 2
printf 'a\nb\nc\nd\ne\nf\nG\n' > "$B/Daily/D.md"
start phone
merged=$(printf 'A\nb\nc\nd\ne\nf\nG')
wait_for "the phone's offline edit merged with the laptop's" same "$B/Daily/D.md" "$merged"
wait_for "  and the laptop has the merge" same "$A/Daily/D.md" "$merged"

echo "== touching edits keep a conflict copy"
stop phone
printf 'A\nb\nc\nd\ne\nf\nG\n- laptop\n' > "$A/Daily/D.md"
sleep 2
printf 'A\nb\nc\nd\ne\nf\nG\n- phone\n' > "$B/Daily/D.md"
start phone
copy_on() { compgen -G "$1/Daily/D (conflict *).md" >/dev/null; }
wait_for "a conflict copy on the phone" copy_on "$B"
wait_for "  and on the laptop" copy_on "$A"
wait_for "  the laptop's text holds the path" same "$B/Daily/D.md" "$(printf 'A\nb\nc\nd\ne\nf\nG\n- laptop')"
[ "$(cat "$A"/Daily/D\ \(conflict*)" = "$(printf 'A\nb\nc\nd\ne\nf\nG\n- phone')" ] && ok "  the phone's text is in the copy" || bad "conflict copy text"

echo "== deletes"
rm "$B/Ideas/New.md"
wait_for "a phone delete reaches the laptop" gone "$A/Ideas/New.md"
wait_for "  and the emptied folder goes too" gone "$A/Ideas"
stop phone
printf 'edited\n' > "$A/Tasks/To Do.md"
sleep 2
rm "$B/Tasks/To Do.md"
start phone
wait_for "a delete loses to an edit made meanwhile" same "$B/Tasks/To Do.md" "edited"

echo "== history and restore"
hist=$(dev laptop history "$A/Tasks/To Do.md" | wc -l)
[ "$hist" -ge 3 ] && ok "history lists $hist revisions" || bad "history: $hist"
dev laptop restore "Tasks/To Do.md" 1 | tail -1
wait_for "restoring revision 1 reaches both folders" same "$B/Tasks/To Do.md" "- [ ] Call the studio"

echo "== status"
dev laptop status | head -3
out=$(dev laptop status)
grep -q 'live socket up' <<<"$out" && ok "the live socket is up" || bad "live socket down"
out=$(dev laptop daemon 2>&1 || true)
grep -q 'already running' <<<"$out" && ok "a second daemon refuses to start" || bad "second daemon"

echo
if [ "$fails" -eq 0 ]; then echo "all passed"; else echo "$fails failed"; echo "--- laptop log"; cat "$WORK/laptop.log"; echo "--- phone log"; cat "$WORK/phone.log"; exit 1; fi
