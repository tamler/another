#!/bin/sh
# Two agents on one machine hand a piece of work over through Another:
#
#   alice holds the file she is changing and asks bob for a review (a claimable request);
#   bob sees it in his inbox, claims it, replies in the thread and completes it;
#   alice reads the reply and the "Done:" notice, acknowledges both and releases her hold.
#
# Everything runs in a throwaway data directory; your own ~/.another is never touched.
# Run it from anywhere: sh examples/two-agent-handoff.sh
# ANOTHER_BIN picks the binary (default: `another` on PATH).
set -eu

another="${ANOTHER_BIN:-another}"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/another-handoff.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
export ANOTHER_HOME="$scratch/home"
unset ANOTHER_AGENT

# Print a step, then run it.
step() {
    printf '\n$ another %s\n' "$*"
    "$another" "$@"
}

# The value of one string field in `another --json ...` output.
field() {
    field_all "$1" | head -n 1
}

# Every value of one string field, one per line.
field_all() {
    sed -n "s/.*\"$1\": *\"\\([^\"]*\\)\".*/\\1/p"
}

mkdir -p "$scratch/shop/src"
cd "$scratch/shop"

step init >/dev/null
step agent register alice --program claude --project .
step agent register bob --program codex --project .

# alice takes the file before she edits it; anyone else's edit hook is stopped meanwhile.
step hold acquire --agent alice@shop src/api.rs --reason "reworking the API"

# alice asks bob for a review. --request makes it claimable, so it is clear who owns it.
printf '\n$ another --json send --from alice@shop --to bob@shop --request ...\n'
sent=$("$another" --json send --from alice@shop --to bob@shop --request \
    --subject "Review the API change in src/api.rs" \
    --body "The handler now returns 404 for a missing order. Please check the error shape.")
message=$(printf '%s\n' "$sent" | field message_id)
request=$(printf '%s\n' "$sent" | field request_id)
[ -n "$message" ] && [ -n "$request" ] || { echo "send did not return ids: $sent" >&2; exit 1; }

# bob's session start (or his next tool call) shows the same thing his inbox does.
step inbox --agent bob@shop
step request claim --agent bob@shop "$request"
# bob marks alice's message read and acknowledged: it leaves his inbox.
step ack --agent bob@shop "$message"
printf '\n$ another --json reply --from bob@shop ... %s\n' "$message"
replied=$("$another" --json reply --from bob@shop \
    --body "Looks right. One nit: say which order id was missing." "$message")
reply=$(printf '%s\n' "$replied" | field message_id)
[ -n "$reply" ] || { echo "reply did not return an id: $replied" >&2; exit 1; }
# Completing the request also sends alice a "Done:" notice in the same thread.
step request complete --agent bob@shop --note "reviewed, one nit" "$request"

# alice reads the thread: bob's reply and the "Done:" notice.
step inbox --agent alice@shop
step thread "$message"
# She acknowledges both, by id, as herself.
alice_inbox=$("$another" --json inbox --agent alice@shop)
# Completing the request must have told alice: a "Done:" notice is in her inbox.
printf '%s\n' "$alice_inbox" | grep -q '"subject": *"Done: Review the API change in src/api.rs"' || {
    echo "alice got no Done: notice for the completed request: $alice_inbox" >&2
    exit 1
}
notices=$(printf '%s\n' "$alice_inbox" | field_all message_id | tr '\n' ' ')
# shellcheck disable=SC2086 # one id per word, by construction
step ack --agent alice@shop $notices
# With no hold ids, release lets go of all of alice's holds.
step hold release --agent alice@shop
step requests

# Check the outcome, so this script doubles as a smoke test.
state=$("$another" --json requests)
printf '%s\n' "$state" | grep -q '"status": *"done"' || {
    echo "the request is not done: $state" >&2
    exit 1
}
holds=$("$another" --json holds)
[ "$(printf '%s' "$holds" | tr -d ' \n')" = "[]" ] || {
    echo "a hold is still live: $holds" >&2
    exit 1
}
# bob's reply reached alice, and both inboxes are empty: everything was read and acknowledged.
case " $notices " in
*" $reply "*) ;;
*)
    echo "bob's reply $reply was not in alice's inbox: $notices" >&2
    exit 1
    ;;
esac
for who in alice@shop bob@shop; do
    inbox=$("$another" --json inbox --agent "$who")
    printf '%s\n' "$inbox" | grep -q '"unread": *0,' || {
        echo "$who still has unread mail: $inbox" >&2
        exit 1
    }
done
printf '\nhandoff complete: request %s is done, every message is acknowledged and no hold is left\n' "$request"
