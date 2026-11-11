#!/usr/bin/env python3
"""Stamp a content-hash query string onto icon and manifest URLs in the built site.

Name: stamp_icons
Description: After the public files are copied, rewrite HTML icon links and the
web manifest so a change to the icon bytes produces a new URL. Browsers and
LinkedIn keep a host's favicon for a long time and often ignore Cache-Control,
so a stable /favicon.ico path can keep showing an older mark, including the
games site that used to live on this domain. The files themselves stay at the
same paths, so a default /favicon.ico request still works.

Run from site/ after the dist copy, or via site/build.sh.
"""

from __future__ import annotations

from hashlib import sha256
from pathlib import Path
import json
import re
import sys

ROOT = Path(__file__).resolve().parent / "dist"

# Order is part of the hash. These are the public icon files the pages and
# manifest point at.
ICON_FILES = (
    "favicon.ico",
    "favicon.svg",
    "apple-touch-icon.png",
    "icon-192.png",
    "icon-512.png",
)

# Paths that get ?v=<hash> in HTML hrefs and in the manifest.
STAMPED_PATHS = ICON_FILES + ("site.webmanifest",)


def icon_version(root: Path) -> str:
    digest = sha256()
    for name in ICON_FILES:
        path = root / name
        if not path.is_file():
            raise SystemExit(f"stamp_icons: missing {path}")
        digest.update(name.encode("ascii"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()[:8]


def stamped_href(path: str, version: str) -> str:
    return f"{path}?v={version}"


def stamp_html(text: str, version: str) -> str:
    def replace(match: re.Match[str]) -> str:
        path = "/" + match.group(1)
        return f'href="{stamped_href(path, version)}"'

    pattern = r'href="/(' + "|".join(re.escape(name) for name in STAMPED_PATHS) + r')"'
    return re.sub(pattern, replace, text)


def stamp_manifest(text: str, version: str) -> str:
    data = json.loads(text)
    for item in data.get("icons", []):
        src = item.get("src", "")
        path = src.split("?", 1)[0]
        name = path.lstrip("/")
        if name in ICON_FILES:
            item["src"] = stamped_href(path if path.startswith("/") else f"/{name}", version)
    return json.dumps(data, indent=2) + "\n"


def stamp(root: Path) -> str:
    version = icon_version(root)
    html_files = sorted(root.rglob("*.html"))
    if not html_files:
        raise SystemExit(f"stamp_icons: no HTML under {root}")
    for path in html_files:
        original = path.read_text(encoding="utf-8")
        updated = stamp_html(original, version)
        if updated == original:
            raise SystemExit(f"stamp_icons: no icon hrefs rewritten in {path}")
        path.write_text(updated, encoding="utf-8")
    manifest = root / "site.webmanifest"
    if not manifest.is_file():
        raise SystemExit(f"stamp_icons: missing {manifest}")
    manifest.write_text(stamp_manifest(manifest.read_text(encoding="utf-8"), version), encoding="utf-8")
    html_stamped = ("favicon.ico", "favicon.svg", "apple-touch-icon.png", "site.webmanifest")
    for path in html_files + [manifest]:
        text = path.read_text(encoding="utf-8")
        for name in html_stamped if path.suffix == ".html" else ("icon-192.png", "icon-512.png"):
            stamped = stamped_href(f"/{name}", version)
            if path.suffix == ".html":
                needle = f'href="{stamped}"'
            else:
                needle = stamped
            if needle not in text:
                raise SystemExit(f"stamp_icons: {path} missing {stamped}")
            bare = f'href="/{name}"' if path.suffix == ".html" else f'"/{name}"'
            if bare in text.replace(needle if path.suffix == ".html" else needle, ""):
                raise SystemExit(f"stamp_icons: {path} still has an unstamped {name} URL")
    return version


def main() -> None:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT
    version = stamp(root)
    print(f"stamp_icons: stamped icon URLs with v={version}")


if __name__ == "__main__":
    main()
