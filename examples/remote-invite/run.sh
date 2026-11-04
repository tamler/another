#!/bin/sh
# A remote agent joins a hub with a single-use invite, across two nodes on this machine:
# the hub (its own ANOTHER_HOME, an authenticated server on a free port) and a "cloud"
# node (another ANOTHER_HOME) standing in for a cloud session. No manager agent: a local
# peer leads the lane, and the person on the hub issues the invite and kicks.
# shellcheck source=examples/lib.sh
. "$(dirname "$0")/../lib.sh"

HUB="$WORK/hub"
CLOUD="$WORK/cloud"
SHOP="$WORK/shop"
mkdir -p "$SHOP"
cd "$SHOP"

step "The hub: a node, a local peer leading a lane, and an authenticated server"
another --home "$HUB" init >/dev/null
another --home "$HUB" agent register claude --program claude --project "$SHOP"
another --home "$HUB" lane create docs --agent claude@shop --lead claude@shop --project "$SHOP" \
  --charter "Write the v0.1.0 docs"
PORT=$(free_port)
serve_bg "$HUB" "$PORT" --auth
HUB_URL="http://127.0.0.1:$PORT"

step "The person on the hub invites a remote agent into the lane"
CODE=$(q --home "$HUB" invite create --agent cloud-codex --program codex --project shop --lane docs \
  --expires 1h --json | jq -r '.code')
must "$CODE" "invite create returned no code"
ok "invite code issued (single use, 1 hour)"

step "The cloud node joins with the code"
another --home "$CLOUD" join "$HUB_URL" "$CODE" --hello
set +e
another --home "$WORK/someone-else" join "$HUB_URL" "$CODE" 2>&1
again=$?
set -e
[ "$again" -ne 0 ] || die "a used invite was accepted again"
ok "the code is single-use"

step "The remote agent works through the hub, inside its scope"
another --home "$CLOUD" remote status
another --home "$CLOUD" remote lanes
another --home "$CLOUD" remote report --summary "Drafted the install and upgrade sections" --progress 40
q --home "$HUB" lane show docs --json | jq -e '[.reports[] | select(.agent | startswith("cloud-codex"))] | length >= 1' >/dev/null \
  || die "the hub has no report from cloud-codex"
ok "the hub's lane has the remote agent's report"

step "The hub sees the remote agent, and the person kicks it"
another --home "$HUB" agent remotes
q --home "$HUB" agent remotes --json | jq -e '[.[] | select(.agent | startswith("cloud-codex"))] | length == 1' >/dev/null \
  || die "cloud-codex is not listed as a remote agent"
ok "cloud-codex is listed"
another --home "$HUB" agent kick cloud-codex@shop --reason "docs handed back"
set +e
another --home "$CLOUD" remote status 2>&1
after=$?
set -e
[ "$after" -ne 0 ] || die "a kicked agent's token still works"
ok "the kicked agent's token is refused"

printf '\nexample remote-invite: PASS\n' >&2
