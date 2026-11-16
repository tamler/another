#!/usr/bin/env bash
# End-to-end checks of an installed `another` binary, run by .github/workflows/install-e2e.yml
# on Linux, macOS and Windows (Git Bash).
#
#   scripts/e2e-install.sh fresh   NEW_BIN
#   scripts/e2e-install.sh upgrade OLD_BIN NEW_BIN
#
# fresh:   a first run on an empty home, messages and holds, client hook packs installed,
#          reinstalled (no change) and removed, the daemon definition in dry-run.
# upgrade: a home written by OLD_BIN (an older schema) is opened by NEW_BIN, which must
#          migrate it and keep its agents, mail and holds; OLD_BIN must then refuse it.
#
# Everything happens under a temporary HOME and ANOTHER_HOME; nothing outside is touched.
set -euo pipefail

fail() {
  echo "e2e-install: FAIL: $*" >&2
  exit 1
}
step() { echo "== $*"; }

# A path as the binary under test expects it (Windows form under Git Bash).
native() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}

# The executable for a path given without its .exe suffix.
exe() {
  if [ -e "$1.exe" ]; then printf '%s\n' "$1.exe"; else printf '%s\n' "$1"; fi
}

expect() { # description, fixed text, output
  grep -qF -- "$2" <<<"$3" || fail "$1: expected '$2' in output:
$3"
}

reject() { # description, fixed text, output
  if grep -qF -- "$2" <<<"$3"; then
    fail "$1: did not expect '$2' in output:
$3"
  fi
}

work=$(mktemp -d)
trap 'rm -rf "$work" 2>/dev/null || true' EXIT
mkdir -p "$work/home" "$work/shop"
HOME=$(native "$work/home")
ANOTHER_HOME=$(native "$work/data")
export HOME ANOTHER_HOME
unset ANOTHER_AGENT ANOTHER_LOG
git -C "$work/shop" init -q
shop=$(native "$work/shop")

# Agents, a message and a hold, written by $1.
seed() {
  local bin=$1
  "$bin" init >/dev/null
  "$bin" agent register --program claude --project "$shop" alice >/dev/null
  "$bin" agent register --program codex --project "$shop" bob >/dev/null
  "$bin" send -f alice@shop -t bob@shop -s "e2e hello" -b "seeded" >/dev/null
  "$bin" hold acquire -a alice@shop --project "$shop" src/a.rs >/dev/null
}

# What seed wrote, read back by $1, and the hold still enforced.
verify() {
  local bin=$1 out
  out=$("$bin" --json inbox --agent bob@shop)
  expect "bob's inbox ($bin)" '"subject": "e2e hello"' "$out"
  out=$("$bin" --json holds)
  expect "live holds ($bin)" '"pattern": "src/a.rs"' "$out"
  expect "live holds ($bin)" '"agent": "alice@shop"' "$out"
  if out=$("$bin" hold acquire -a bob@shop --project "$shop" src/a.rs 2>&1); then
    fail "bob took alice's hold ($bin): $out"
  fi
  expect "conflicting hold ($bin)" "held by alice@shop" "$out"
}

fresh() {
  local new
  new=$(exe "$1")
  step "version"
  expect "--version" "another " "$("$new" --version)"

  step "first run on an empty home"
  seed "$new"
  verify "$new"
  "$new" --json board >/dev/null || fail "board on a fresh home"

  step "hook packs: install"
  local out
  out=$("$new" --json hook install --client all)
  expect "first hook install" '"change": "create"' "$out"
  for f in .claude/settings.json .codex/hooks.json .cursor/hooks.json; do
    [ -s "$work/home/$f" ] || fail "hook install did not write ~/$f"
    grep -qF "another-hook" "$work/home/$f" || fail "~/$f does not run the Another hook"
  done
  out=$("$new" hook status)
  expect "hook status" "claude" "$out"

  step "hook packs: reinstall changes nothing"
  out=$("$new" --json hook install --client all)
  reject "second hook install" '"change": "create"' "$out"
  reject "second hook install" '"change": "update"' "$out"

  step "hook packs: uninstall"
  "$new" hook uninstall --client all >/dev/null
  if grep -rlF "another-hook" "$work/home" --include='*.json' --include='*.toml' 2>/dev/null; then
    fail "client configs still name the Another hook after uninstall"
  fi

  # `another daemon` manages launchd only until #142 adds systemd and Task Scheduler;
  # elsewhere the command must at least refuse cleanly.
  if [ "$(uname -s)" = Darwin ]; then
    step "daemon definition (dry run; nothing is loaded)"
    out=$("$new" --json daemon install --dry-run 2>&1) || fail "daemon dry run failed: $out"
    expect "daemon dry run" '"dry_run": true' "$out"
    [ ! -e "$work/home/Library/LaunchAgents" ] || fail "dry run wrote a LaunchAgent"
  else
    step "daemon definition: not supported on $(uname -s) yet, so it must refuse"
    if out=$("$new" daemon install --dry-run 2>&1); then
      echo "daemon install --dry-run succeeded here: $out"
    else
      expect "daemon refusal" "another:" "$out"
    fi
  fi

  step "fresh: ok"
}

upgrade() {
  local old new out
  old=$(exe "$1")
  new=$(exe "$2")
  step "home written by the old build"
  seed "$old"
  verify "$old"

  step "opened by the new build: migrated, nothing lost"
  verify "$new"
  "$new" --json board >/dev/null || fail "board after the upgrade (schema not current)"
  local id
  id=$("$new" --json inbox --agent bob@shop | sed -n 's/.*"message_id": "\([0-9A-Z]*\)".*/\1/p' | head -1)
  [ -n "$id" ] || fail "no message id after the upgrade"
  "$new" reply -f bob@shop -b "after upgrade" "$id" >/dev/null
  out=$("$new" --json inbox --agent alice@shop)
  expect "reply written by the new build" "after upgrade" "$out"

  step "the old build refuses the newer schema"
  if out=$("$old" --json holds 2>&1); then
    fail "the old build opened a newer schema: $out"
  fi
  expect "old build on the new schema" "newer than this another build supports" "$out"

  step "upgrade: ok"
}

case "${1:-}" in
  fresh)
    [ $# -eq 2 ] || fail "usage: $0 fresh NEW_BIN"
    fresh "$2"
    ;;
  upgrade)
    [ $# -eq 3 ] || fail "usage: $0 upgrade OLD_BIN NEW_BIN"
    upgrade "$2" "$3"
    ;;
  *)
    fail "usage: $0 fresh NEW_BIN | upgrade OLD_BIN NEW_BIN"
    ;;
esac
