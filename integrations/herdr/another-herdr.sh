#!/bin/sh
# Another helper for the Herdr plugin. Requires `another` on PATH. POSIX sh only.
# Herdr runs this with the plugin directory as the working directory; HERDR_BIN_PATH points
# at the herdr binary, which `another wake --via herdr` uses to prompt agents by name.
set -u

if ! command -v another >/dev/null 2>&1; then
  echo "another is not installed or not on PATH" >&2
  exit 1
fi

case "${1:-}" in
  wake)
    # One wake pass: prompt the idle Herdr session of every agent that has mail no hook has
    # announced yet. Sessions are found through the rows their hooks make inside Herdr.
    exec another wake --all
    ;;
  summary)
    exec another activity --summary --since 1h
    ;;
  activity)
    # A live view: refresh the last hour of activity every 10 seconds.
    while :; do
      clear 2>/dev/null || printf '\033[2J\033[H'
      printf 'Another: activity (last hour) - %s\n\n' "$(date '+%H:%M:%S')"
      another activity --since 1h --limit 40 || true
      sleep 10
    done
    ;;
  *)
    echo "usage: another-herdr.sh wake|summary|activity" >&2
    exit 2
    ;;
esac
