#!/usr/bin/env python3
"""Fail if site copy or docs/PROMOTION.md uses em/en dashes or banned phrases.

ASCII hyphens are fine. The check is the prose, not the font license.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TEXT_SUFFIXES = {".html", ".md", ".txt", ".css"}

# Phrases and the "it's not X, it's Y" shape. Matched case-insensitively.
BANNED = [
    (r"\bdelve\b", "delve"),
    (r"\bdelving\b", "delving"),
    (r"\bgame-?changer\b", "game-changer"),
    (r"\bseamless(?:ly)?\b", "seamless"),
    (r"\bunlock(?:s|ing|ed)?\b", "unlock"),
    (r"\bin today['\u2019]s fast-paced\b", "in today's fast-paced"),
    (r"\bit['\u2019]s worth noting\b", "it's worth noting"),
    (r"\bit is worth noting\b", "it is worth noting"),
    (r"\bcutting[- ]edge\b", "cutting-edge"),
    (r"\bnext-level\b", "next-level"),
    (r"\bworld-class\b", "world-class"),
    (r"\bstate-of-the-art\b", "state-of-the-art"),
    (r"\bsupercharge\b", "supercharge"),
    (r"\bstreamline\b", "streamline"),
    (r"\bempower\b", "empower"),
    (r"\btapestry\b", "tapestry"),
    (r"\bpivotal\b", "pivotal"),
    (r"\bin conclusion\b", "in conclusion"),
    (r"\bto summarize\b", "to summarize"),
    (r"\bat the end of the day\b", "at the end of the day"),
    (r"\bwhen it comes to\b", "when it comes to"),
    (r"\bin the world of\b", "in the world of"),
    (r"\blook no further\b", "look no further"),
    (
        r"\bit(?:['\u2019]s| is) not\b[^.\n]{0,80}?,\s*it(?:['\u2019]s| is)\b",
        "it's not X, it's Y",
    ),
]


def files():
    promo = ROOT / "docs" / "PROMOTION.md"
    if promo.is_file():
        yield promo
    else:
        raise SystemExit(f"missing {promo}")
    site = ROOT / "site"
    for path in sorted(site.rglob("*")):
        if not path.is_file():
            continue
        if "dist" in path.parts:
            continue
        if path.name == "OFL.txt":
            continue
        if path.suffix.lower() not in TEXT_SUFFIXES:
            continue
        yield path


def scan(path: Path, text: str) -> list[str]:
    errors = []
    for i, line in enumerate(text.splitlines(), 1):
        if "\u2014" in line or "\u2013" in line:
            errors.append(f"{path}:{i}: em dash or en dash")
        for pattern, name in BANNED:
            if re.search(pattern, line, re.IGNORECASE):
                errors.append(f"{path}:{i}: banned phrase ({name})")
    return errors


def check_repo() -> list[str]:
    found = []
    for path in files():
        found.extend(scan(path, path.read_text(encoding="utf-8")))
    return found


def self_test() -> None:
    samples = [
        ("clean.md", "Local-first, Rust 1.88, milestones 1-6.\n", []),
        ("em.md", "Agents \u2014 inbox\n", ["em dash or en dash"]),
        ("en.md", "Milestones 1\u20136\n", ["em dash or en dash"]),
        ("delve.md", "We delve into holds.\n", ["delve"]),
        ("shape.md", "It's not a bus, it's a mailbox.\n", ["it's not X, it's Y"]),
        ("curly.md", "It\u2019s not a lock, it\u2019s a signal.\n", ["it's not X, it's Y"]),
    ]
    for name, text, expect in samples:
        got = scan(Path(name), text)
        kinds = " ".join(got)
        if expect and not all(piece in kinds for piece in expect):
            raise SystemExit(f"self-test missed {expect} in {name}: {got}")
        if not expect and got:
            raise SystemExit(f"self-test false positive in {name}: {got}")
    print("self-test ok")


def main() -> int:
    if "--self-test" in sys.argv:
        self_test()
        return 0
    found = check_repo()
    if found:
        print("\n".join(found))
        print(f"{len(found)} copy problem(s)")
        return 1
    print("copy ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
