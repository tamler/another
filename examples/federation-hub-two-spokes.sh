#!/bin/sh
# A federation of three nodes on one machine: a hub and two spokes (laptop, runner).
#
#   the hub serves /fed/* on loopback and issues one invite per spoke;
#   each spoke joins, then syncs (push its own events, pull everyone else's);
#   alice on the laptop mails bob on the runner, bob replies, and alice reads the reply.
#
# On real machines each node has its own data directory on its own host, and the hub is
# reached through a tunnel (docs/FEDERATION.md). Here three throwaway data directories
# stand in for the three hosts; your own ~/.another is never touched.
# Run it from anywhere: sh examples/federation-hub-two-spokes.sh
# ANOTHER_BIN picks the binary (default: `another` on PATH).
set -eu

another="${ANOTHER_BIN:-another}"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/another-federation.XXXXXX")
hub_pid=""
cleanup() {
    # Stop only the hub this script started, by its process id.
    if [ -n "$hub_pid" ]; then
        kill -TERM "$hub_pid" 2>/dev/null || true
        wait "$hub_pid" 2>/dev/null || true
    fi
    rm -rf "$scratch"
}
trap cleanup EXIT
unset ANOTHER_AGENT

# Run `another` as one node: on <node> <args...>
on() {
    node=$1
    shift
    printf '\n[%s] $ another %s\n' "$node" "$*"
    ANOTHER_HOME="$scratch/$node" "$another" "$@"
}

# Quietly, for values the script needs.
quiet() {
    node=$1
    shift
    ANOTHER_HOME="$scratch/$node" "$another" "$@"
}

field() {
    sed -n "s/.*\"$1\": *\"\\([^\"]*\\)\".*/\\1/p" | head -n 1
}

# Every node gets a unique name before pairing; agents elsewhere address it by name.
for node in hub laptop runner; do
    quiet "$node" init >/dev/null
    on "$node" node name "$node"
done

# The hub: loopback only, port chosen by the system, federation on.
ANOTHER_HOME="$scratch/hub" "$another" serve --federation --no-dashboard \
    --bind 127.0.0.1:0 2>"$scratch/hub.log" &
hub_pid=$!
url=""
tries=0
while [ -z "$url" ]; do
    tries=$((tries + 1))
    if [ "$tries" -gt 100 ] || ! kill -0 "$hub_pid" 2>/dev/null; then
        echo "the hub did not start:" >&2
        cat "$scratch/hub.log" >&2
        exit 1
    fi
    sleep 0.1
    url=$(sed -n 's|.*on \(http://127\.0\.0\.1:[0-9]*\)/mcp.*|\1|p' "$scratch/hub.log" | head -n 1)
done
printf '\nhub listening on %s\n' "$url"

# One single-use invite per spoke. The runner may not read the feed: it only gets mail.
invite_laptop=$(quiet hub pair invite --url "$url" --perms read_feed,send_mail --expires 10m |
    grep -o 'another-invite:[^ ]*' | head -n 1)
invite_runner=$(quiet hub pair invite --url "$url" --perms send_mail --expires 10m |
    grep -o 'another-invite:[^ ]*' | head -n 1)
[ -n "$invite_laptop" ] && [ -n "$invite_runner" ] || { echo "no invite code" >&2; exit 1; }
on laptop pair join "$invite_laptop"
on runner pair join "$invite_runner"
on hub peers

# Agents register on their own node; the directory reaches the others on sync.
on laptop agent register alice --program claude
on runner agent register bob --program codex
on runner sync
on laptop sync

# alice mails bob on another node: name@@node (no project) or name@project@node.
printf '\n[laptop] $ another --json send --from alice --to bob@@runner ...\n'
sent=$(quiet laptop --json send --from alice --to bob@@runner \
    --subject "GPU box free after 18:00?" --body "I need it for a two-hour eval run.")
message=$(printf '%s\n' "$sent" | field message_id)
[ -n "$message" ] || { echo "send failed: $sent" >&2; exit 1; }
on laptop sync
on runner sync
on runner inbox --agent bob
on runner reply --from bob --body "Yes, it is yours from 18:00 to 20:00." "$message"
on runner sync
on laptop sync
on laptop inbox --agent alice

# Check the outcome, so this script doubles as a smoke test.
inbox=$(quiet laptop --json inbox --agent alice)
printf '%s\n' "$inbox" | grep -q 'Re: GPU box free after 18:00?' || {
    echo "alice did not get bob's reply: $inbox" >&2
    exit 1
}
printf '\nfederation round trip complete: laptop -> hub -> runner -> hub -> laptop\n'
