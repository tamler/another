#!/bin/sh
# A spend budget with an approval gate. Borrowing the GPU account asks the person first (a
# decision); the lease only starts once they approve, and a denied request never starts.
# No manager agent: the person approves from the CLI on this machine.
# shellcheck source=examples/lib.sh
. "$(dirname "$0")/../lib.sh"

SHOP="$WORK/shop"
mkdir -p "$SHOP"
cd "$SHOP"

step "A node, one agent, the person who approves, and a GPU budget"
another init >/dev/null
another agent register claude --program claude --project "$SHOP"
another agent register owner --program human
another config decider owner
another spend account add gpu --kind gpu_pod --unit USD --budget 50 --cost-per-hour 2.5 \
  --requires-approval --description "Rented A100 pod"

step "Spend already made this month counts against the budget"
another spend record --agent claude@shop --account gpu --amount 30 --project "$SHOP" --note "last week's run"
expect_eq "$(q spend --json | jq '.accounts[] | select(.account.name == "gpu") | .remaining | floor')" "20" \
  "20 USD of 50 left"

step "The agent asks for a 3-hour lease: it waits for approval"
B=$(q spend borrow --agent claude@shop --account gpu --hours 3 --note "fine-tune run" --json)
LEASE=$(printf '%s' "$B" | jq -r '.lease.lease_id')
DECISION=$(printf '%s' "$B" | jq -r '.decision_id')
must "$LEASE" "borrow returned no lease"
must "$DECISION" "borrow asked no decision"
expect_eq "$(printf '%s' "$B" | jq -r '.lease.status')" "pending_approval" "the lease has not started"
another decision show "$DECISION"

step "The person approves on this machine; the lease starts"
another decision answer "$DECISION" approve
expect_eq "$(q spend leases --json | jq -r --arg id "$LEASE" '.[] | select(.lease_id == $id) | .status')" \
  "active" "the lease is active"

step "A second request is denied: it never starts and costs nothing"
B2=$(q spend borrow --agent claude@shop --account gpu --hours 8 --note "a bigger run" --json)
LEASE2=$(printf '%s' "$B2" | jq -r '.lease.lease_id')
DECISION2=$(printf '%s' "$B2" | jq -r '.decision_id')
must "$DECISION2" "second borrow asked no decision"
another decision answer "$DECISION2" deny --text "Not this month; budget is tight"
STATUS2=$(q spend leases --all --json | jq -r --arg id "$LEASE2" '.[] | select(.lease_id == $id) | .status')
[ "$STATUS2" != "active" ] || die "a denied lease became active"
ok "the denied lease is $STATUS2"
another inbox --agent claude@shop

step "The agent ends the approved lease; its cost is recorded"
another spend release --agent claude@shop "$LEASE"
expect_eq "$(q spend leases --all --json | jq -r --arg id "$LEASE" '.[] | select(.lease_id == $id) | .status')" \
  "ended" "the lease ended"
another spend

printf '\nexample spend-approval: PASS\n' >&2
