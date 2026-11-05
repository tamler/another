# Shared helpers for the example scripts. Source it; don't run it.
#
# Every example runs in its own temporary ANOTHER_HOME (and project folder), so it never
# touches ~/.another. Set ANOTHER_BIN to use a specific binary; otherwise `another` on PATH,
# else this checkout's target/release or target/debug build.
set -eu

if [ -z "${ANOTHER_BIN:-}" ]; then
  here=$(cd "$(dirname "$0")/../.." && pwd)
  if command -v another >/dev/null 2>&1; then ANOTHER_BIN=$(command -v another)
  elif [ -x "$here/target/release/another" ]; then ANOTHER_BIN="$here/target/release/another"
  elif [ -x "$here/target/debug/another" ]; then ANOTHER_BIN="$here/target/debug/another"
  else echo "another not found: build it (cargo build) or set ANOTHER_BIN" >&2; exit 1
  fi
fi
command -v jq >/dev/null 2>&1 || { echo "these examples need jq" >&2; exit 1; }

WORK=$(mktemp -d)
export ANOTHER_HOME="$WORK/home"
# Each example names its own agents and lanes; don't inherit them from the caller's session.
unset ANOTHER_AGENT ANOTHER_LANE
PIDS=""
cleanup() {
  for p in $PIDS; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# Run the CLI, echoing the command so the transcript reads like a walkthrough.
another() {
  printf '\n$ another %s\n' "$*" >&2
  "$ANOTHER_BIN" "$@"
}

# Same, quietly, for --json calls whose output is parsed.
q() { "$ANOTHER_BIN" "$@"; }

step() { printf '\n== %s\n' "$*" >&2; }
ok() { printf 'ok: %s\n' "$*" >&2; }
die() { printf 'FAILED: %s\n' "$*" >&2; exit 1; }

# expect_eq ACTUAL EXPECTED WHAT
expect_eq() { [ "$1" = "$2" ] || die "$3: expected '$2', got '$1'"; ok "$3"; }

# A free TCP port on 127.0.0.1.
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

# Start `another serve` in the background with the given home and flags; wait until it answers.
serve_bg() {
  home=$1; port=$2; shift 2
  "$ANOTHER_BIN" --home "$home" serve --bind "127.0.0.1:$port" "$@" >"$WORK/serve-$port.log" 2>&1 &
  PIDS="$PIDS $!"
  i=0
  until curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -lt 100 ] || { cat "$WORK/serve-$port.log" >&2; die "server on $port did not start"; }
    sleep 0.1
  done
}

# must VALUE WHAT: fail when a captured value is empty or JSON null (a pipeline's exit
# status is the last command's, so a failed `another … --json | jq` shows up here).
must() { [ -n "$1" ] && [ "$1" != null ] || die "$2"; }
