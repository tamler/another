#!/usr/bin/env python3
"""Run the ```bash blocks of a markdown section verbatim, in order, in one shell.

    scripts/run-doc-commands.py FILE "START HEADING" ["END HEADING"]

The blocks run as one `bash -e` script, so state such as `cd` and background jobs (`&`)
carries from block to block; background jobs are killed at the end. A block that is
documented to fail is marked with an HTML comment just before it:

    <!-- doc-test: exit 3 -->

and must exit with exactly that code. Run it in a throwaway HOME: the commands are run as
written, hooks and config files included (CI does this in a fresh runner).
"""
import re
import subprocess
import sys


def blocks(path, start, end):
    text = open(path, encoding="utf-8").read()
    i = text.find(start)
    if i < 0:
        sys.exit(f"{path}: heading {start!r} not found")
    j = text.find(end, i + len(start)) if end else -1
    section = text[i : j if j > 0 else len(text)]
    out = []
    for m in re.finditer(r"(?:<!-- doc-test: exit (\d+) -->\s*\n)?```bash\n(.*?)```", section, re.S):
        out.append((int(m.group(1) or 0), m.group(2).rstrip("\n")))
    if not out:
        sys.exit(f"{path}: no bash blocks under {start!r}")
    return out


def script(bs):
    lines = [
        "set -e",
        "trap 'kill $(jobs -p) 2>/dev/null || true' EXIT",
        "doc_block() { printf '\\n\\033[1m$ %s\\033[0m\\n' \"$1\" >&2; }",
    ]
    for n, (code, body) in enumerate(bs, 1):
        quoted = body.replace("'", "'\\''")
        lines.append(f"doc_block '{quoted}'")
        if code == 0:
            lines.append(body)
            # Give a backgrounded server a moment before the next block uses it.
            if body.rstrip().endswith("&"):
                lines.append("sleep 1")
        else:
            lines.append("set +e")
            lines.append(f"( {body}\n)")
            lines.append("rc=$?")
            lines.append("set -e")
            lines.append(
                f'[ "$rc" -eq {code} ] || {{ echo "block {n}: expected exit {code}, got $rc" >&2; exit 1; }}'
            )
            lines.append(f'echo "(exited {code}, as documented)" >&2')
    lines.append("echo '\nall documented commands ran' >&2")
    return "\n".join(lines) + "\n"


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit(__doc__)
    bs = blocks(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) == 4 else None)
    sys.exit(subprocess.call(["bash", "-c", script(bs)]))


if __name__ == "__main__":
    main()
