#!/bin/sh
# Run every example headless, one after another; exit 1 if any fails.
# ANOTHER_BIN selects the binary (default: `another` on PATH, else this checkout's build).
set -u
here=$(cd "$(dirname "$0")" && pwd)
failed=""
for ex in peer-lanes human-decision spend-approval remote-invite; do
  printf '\n######## %s\n' "$ex"
  if sh "$here/$ex/run.sh"; then :; else failed="$failed $ex"; fi
done
if [ -n "$failed" ]; then
  printf '\nFAILED:%s\n' "$failed" >&2
  exit 1
fi
printf '\nall examples passed\n'
