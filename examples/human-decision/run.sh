#!/bin/sh
# An agent asks a person a structured question; the person answers over HTTP. First from
# this machine (the plain loopback server), then "from the phone": through an authenticated
# server with the person's own token, the way a phone shortcut or a tunnel would reach it.
# No manager agent: the person answers directly.
# shellcheck source=examples/lib.sh
. "$(dirname "$0")/../lib.sh"

SHOP="$WORK/shop"
mkdir -p "$SHOP"
cd "$SHOP"

step "A node, one agent, and the person who decides"
another init >/dev/null
another agent register claude --program claude --project "$SHOP"
another agent register jacob --program human
another config decider jacob

step "The agent asks (default deadline behaviour: keep_open)"
ID=$(q decision ask --agent claude@shop --project "$SHOP" \
  --question "Ship the checkout refactor behind a flag, or wait for review?" \
  --option ship="Ship behind a flag" --option wait="Wait for review" \
  --link https://example.test/pr/7 --json | jq -r '.decision_id')
must "$ID" "decision ask"
ok "decision $ID is open"
another decision show "$ID"

step "The person answers from this machine, over the loopback server"
PORT=$(free_port)
serve_bg "$ANOTHER_HOME" "$PORT"
printf '\n$ curl http://127.0.0.1:%s/decisions\n' "$PORT" >&2
curl -fsS "http://127.0.0.1:$PORT/decisions" | jq -e --arg id "$ID" '[.decisions[] | select(.decision_id == $id)] | length == 1' >/dev/null \
  || die "the open decision is not listed over HTTP"
ok "GET /decisions lists it"
printf '$ curl -X POST http://127.0.0.1:%s/decisions/%s/answer -d {"value":"wait"}\n' "$PORT" "$ID" >&2
curl -fsS -X POST "http://127.0.0.1:$PORT/decisions/$ID/answer" \
  -H 'content-type: application/json' -d '{"value": "wait", "text": "Let codex review first."}' \
  | jq -e '.status == "answered" and .answer.value == "wait"' >/dev/null || die "the HTTP answer was not applied"
ok "answered over HTTP"

step "The asking agent gets the answer by mail, in the decision's thread"
another inbox --agent claude@shop
q inbox --agent claude@shop --json | jq -e '[.messages[] | select(.subject | test("wait|answer"; "i"))] | length > 0' >/dev/null \
  || die "claude did not get the answer mail"
ok "claude was mailed the answer"

step "From the phone: an authenticated server and the person's own token"
ID2=$(q decision ask --agent claude@shop --project "$SHOP" \
  --question "Merge the search lane today?" --option yes="Merge today" --option no="Not yet" --json \
  | jq -r '.decision_id')
must "$ID2" "second decision ask"
TOKEN=$(q token create --agent jacob --project "$SHOP" --json | jq -r '.secret')
must "$TOKEN" "token for jacob"
APORT=$(free_port)
serve_bg "$ANOTHER_HOME" "$APORT" --auth
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$APORT/decisions")
expect_eq "$code" "401" "no token, no access"
curl -fsS -X POST "http://127.0.0.1:$APORT/decisions/$ID2/answer" \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"value": "yes"}' \
  | jq -e '.status == "answered" and .answer.value == "yes"' >/dev/null || die "the phone answer was not applied"
ok "answered with the person's token"
another decision show "$ID2"

step "The agent can't answer its own question"
ID3=$(q decision ask --agent claude@shop --project "$SHOP" \
  --question "Spend an hour on docs?" --option yes=Yes --option no=No --json | jq -r '.decision_id')
must "$ID3" "third decision ask"
CTOKEN=$(q token create --agent claude@shop --project "$SHOP" --json | jq -r '.secret')
must "$CTOKEN" "token for claude"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$APORT/decisions/$ID3/answer" \
  -H "authorization: Bearer $CTOKEN" -H 'content-type: application/json' -d '{"value": "yes", "answered_by": "jacob"}')
expect_eq "$code" "403" "the asker's own token is refused"
expect_eq "$(q decision show "$ID3" --json | jq -r '.status')" "open" "the decision stays open"

printf '\nexample human-decision: PASS\n' >&2
