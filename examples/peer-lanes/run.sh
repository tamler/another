#!/bin/sh
# Two peer agents split one repository with lanes and lane-aware holds. No manager agent:
# each agent creates and leads its own lane, and they sort out a conflict by mail.
# shellcheck source=examples/lib.sh
. "$(dirname "$0")/../lib.sh"

SHOP="$WORK/shop"
mkdir -p "$SHOP/src/checkout" "$SHOP/src/search"
cd "$SHOP"

step "A node and two peer agents in one repository"
another init >/dev/null
another agent register claude --program claude --project "$SHOP"
another agent register codex --program codex --project "$SHOP"

step "Each agent creates the lane it leads"
another lane create checkout --agent claude@shop --lead claude@shop --project "$SHOP" \
  --charter "Rework the checkout flow (src/checkout)"
another lane create search --agent codex@shop --lead codex@shop --project "$SHOP" \
  --charter "Add fuzzy search (src/search)"

step "Holds: each agent holds its own directory, in its own lane"
CHECKOUT_HOLD=$(q hold acquire --agent claude@shop 'src/checkout/**' --reason "checkout rework" --json \
  | jq -r '.granted[0].hold_id')
ok "claude holds src/checkout/** ($CHECKOUT_HOLD)"
another hold acquire --agent codex@shop 'src/search/**' --reason "fuzzy search"
expect_eq "$(q holds --by-lane --json | jq -r '[.[] | .lane] | sort | join(",")')" \
  "checkout,search" "both lanes hold their own paths"

step "Codex reaches into the checkout lane's files: refused, nothing granted"
set +e
another hold acquire --agent codex@shop src/checkout/cart.rs --reason "shared helper"
code=$?
set -e
expect_eq "$code" "3" "a cross-lane conflict exits 3"
expect_eq "$(q holds --json | jq '[.[] | select(.agent | startswith("codex"))] | length')" "1" \
  "codex still holds only src/search/**"

step "Claude, who leads the holding lane, is told about the conflict"
another inbox --agent claude@shop
q inbox --agent claude@shop --json | jq -e '[.messages[] | select(.subject | test("conflict"; "i"))] | length > 0' >/dev/null \
  || die "claude was not mailed about the conflict"
ok "the holding lane's lead got a conflict mail"

step "Peers sort it out: codex asks, claude finishes and releases"
another send --from codex@shop --to claude@shop --subject "Need src/checkout/cart.rs" \
  --body "I need to add a search hook to cart.rs. Can you release it when you are done?" --request
another lane report checkout --agent claude@shop --summary "Cart refactor done; releasing cart.rs" --progress 80
another hold release --agent claude@shop "$CHECKOUT_HOLD"
another hold acquire --agent claude@shop src/checkout/payment.rs --reason "payment step"
another hold acquire --agent codex@shop src/checkout/cart.rs --lane search --reason "search hook"
expect_eq "$(q holds --json | jq -r '[.[] | select(.agent | startswith("codex")) | .pattern] | sort | join(",")')" \
  "src/checkout/cart.rs,src/search/**" "codex now holds cart.rs for the search lane"

step "Both lanes finish with DONE reports"
another lane "done" checkout --agent claude@shop --outcome shipped --summary "New checkout flow" --pr https://example.test/pr/1
another lane "done" search --agent codex@shop --outcome shipped --summary "Fuzzy search" --pr https://example.test/pr/2
expect_eq "$(q lane list --all --json | jq -r '[.[] | .status] | unique | join(",")')" "done" "both lanes are done"
another lane show checkout --markdown

printf '\nexample peer-lanes: PASS\n' >&2
