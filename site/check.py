"""Validate the public output, inert structured data and SEO; stdlib only."""
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import struct
from urllib.parse import urlsplit
import xml.etree.ElementTree as ET

ROOT = Path(__file__).parent / "dist"
ORIGIN = "https://toanother.one"
ROUTES = {
    "/", "/docs/", "/docs/getting-started/", "/docs/install/", "/compare/",
    "/guides/claude-code-codex/", "/guides/mcp-agent-messaging/",
    "/guides/avoid-agent-file-conflicts/", "/guides/local-agent-inbox/",
}
EXPECTED = {
    "index.html", "docs/index.html", "docs/getting-started/index.html",
    "docs/install/index.html", "compare/index.html", "404.html", "style.css",
    "_headers", "fonts/FragmentMono-Regular.woff2", "fonts/OFL.txt", "og.png",
    "twitter.png", "sitemap.xml", "robots.txt", "llms.txt", "favicon.svg",
    "guides/claude-code-codex/index.html", "guides/mcp-agent-messaging/index.html",
    "guides/avoid-agent-file-conflicts/index.html", "guides/local-agent-inbox/index.html",
}
actual = {str(p.relative_to(ROOT)) for p in ROOT.rglob("*") if p.is_file()}
assert actual == EXPECTED, f"public allowlist mismatch: {actual ^ EXPECTED}"
assert not any(p.is_symlink() for p in ROOT.rglob("*")), "no public symlinks"


class Page(HTMLParser):
    def __init__(self, path):
        super().__init__(convert_charrefs=True)
        self.path, self.ids, self.links = path, set(), []
        self.meta, self.canonicals, self.structured = {}, [], []
        self.title, self.in_title, self.in_script, self.script_data = "", False, False, ""
        self.text = ""
        self.headings = self.mains = 0
        self.feed(path.read_text())
        self.close()
        assert not self.in_script, f"{path}: unclosed JSON-LD"
        assert self.headings == self.mains == 1, f"{path}: needs one h1 and main"

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        assert tag not in {"iframe", "object", "embed", "base"}, f"{self.path}: forbidden {tag}"
        assert not any(k.startswith("on") or k in {"style", "srcdoc"} for k in attrs), f"{self.path}: inline code/style"
        if tag == "script":
            assert attrs == {"type": "application/ld+json"}, f"{self.path}: only inline JSON-LD allowed"
            assert not self.in_script, f"{self.path}: nested script"
            self.in_script, self.script_data = True, ""
        self.in_title |= tag == "title"
        if tag == "meta":
            key = attrs.get("name", attrs.get("property"))
            if key:
                assert key not in self.meta, f"{self.path}: duplicate metadata {key}"
                self.meta[key] = attrs.get("content", "")
        if tag == "link" and attrs.get("rel") == "canonical":
            self.canonicals.append(attrs.get("href"))
        if "id" in attrs:
            assert attrs["id"] not in self.ids, f"{self.path}: duplicate id"
            self.ids.add(attrs["id"])
        self.headings += tag == "h1"
        self.mains += tag == "main"
        for key in ("href", "src"):
            if key in attrs:
                self.links.append(attrs[key])
        for key in ("src", "srcset", "poster", "data"):
            if key in attrs:
                assert key == "src" and attrs[key].startswith("/") and not attrs[key].startswith("//"), f"{self.path}: external/unsupported asset"
        if tag == "link" and attrs.get("rel") in {"stylesheet", "preload", "icon"}:
            assert attrs.get("href", "").startswith("/") and not attrs["href"].startswith("//"), f"{self.path}: external asset"

    def handle_endtag(self, tag):
        if tag == "title":
            self.in_title = False
        if tag == "script":
            assert self.in_script, f"{self.path}: unexpected script close"
            self.structured.append(json.loads(self.script_data))
            self.in_script = False

    def handle_data(self, data):
        if not self.in_script:
            self.text += data
        if self.in_title:
            self.title += data
        if self.in_script:
            self.script_data += data


pages = {p: Page(p) for p in ROOT.rglob("*.html")}
for path, page in pages.items():
    for command in re.findall(r"cargo install[^\n]*", page.text):
        if "--git" in command:
            assert "--locked" in command, f"{path}: Git install must use the tested lockfile"
    for href in page.links:
        url = urlsplit(href)
        if url.scheme or url.netloc:
            assert url.scheme == "https", f"{path}: unexpected URL {href}"
            continue
        if path.name == "404.html":
            assert not url.path or url.path.startswith("/"), f"{path}: 404 links must work at depth: {href}"
        target = (ROOT / url.path.lstrip("/")) if url.path.startswith("/") else path.parent / url.path
        if not url.path:
            target = path
        if target.is_dir():
            target /= "index.html"
        assert target.is_file(), f"{path}: missing local target {href}"
        if url.fragment:
            assert url.fragment in pages[target].ids, f"{path}: missing anchor {href}"

titles, descriptions, canonical_urls = set(), set(), set()
for path, page in pages.items():
    if path.name == "404.html":
        assert page.meta.get("robots") == "noindex" and not page.canonicals
        continue
    route = "/" + path.relative_to(ROOT).as_posix().removesuffix("index.html")
    canonical = ORIGIN + route
    assert route in ROUTES and page.canonicals == [canonical], f"{path}: canonical"
    assert page.title and page.title not in titles, f"{path}: unique title"
    titles.add(page.title)
    description = page.meta.get("description", "")
    assert 130 <= len(description) <= 170 and description not in descriptions, f"{path}: unique description near 150 characters ({len(description)})"
    descriptions.add(description)
    assert "noindex" not in page.meta.get("robots", ""), f"{path}: unexpectedly noindex"
    required = {
        "og:type": "website", "og:site_name": "Another", "og:title": page.title,
        "og:url": canonical, "og:image": ORIGIN + "/og.png",
        "og:image:width": "1200", "og:image:height": "630",
        "twitter:card": "summary_large_image", "twitter:title": page.title,
        "twitter:image": ORIGIN + "/twitter.png",
    }
    for key, value in required.items():
        assert page.meta.get(key) == value, f"{path}: {key}"
    for key in ("og:description", "twitter:description", "og:image:alt", "twitter:image:alt"):
        assert page.meta.get(key), f"{path}: missing {key}"
    assert len(page.structured) == 1, f"{path}: one JSON-LD graph"
    data = page.structured[0]
    assert data["@context"] == "https://schema.org"
    graph = {item["@type"]: item for item in data["@graph"]}
    assert set(graph) == {"SoftwareApplication", "SoftwareSourceCode"}
    assert graph["SoftwareApplication"]["name"] == "Another"
    assert graph["SoftwareSourceCode"]["codeRepository"] == "https://github.com/tamler/another"
    assert all(item["license"] == "https://www.apache.org/licenses/LICENSE-2.0" for item in graph.values())
    canonical_urls.add(canonical)

assert canonical_urls == {ORIGIN + route for route in ROUTES}, "every public route needs metadata"
locations = [e.text for e in ET.parse(ROOT / "sitemap.xml").iter("{http://www.sitemaps.org/schemas/sitemap/0.9}loc")]
assert len(locations) == len(set(locations)) == len(ROUTES) and set(locations) == canonical_urls
robots = (ROOT / "robots.txt").read_text()
assert robots == "User-agent: *\nAllow: /\n\nSitemap: https://toanother.one/sitemap.xml\n"
llms = (ROOT / "llms.txt").read_text()
assert llms.startswith("# Another\n\n> ") and "## Optional\n" in llms
for url in re.findall(r"\]\((https://[^)]+)\)", llms):
    if url.startswith(ORIGIN):
        assert url in canonical_urls, f"llms: unknown page {url}"
assert canonical_urls <= set(re.findall(r"\]\((https://[^)]+)\)", llms))
for name in ("og.png", "twitter.png"):
    png = (ROOT / name).read_bytes()
    assert png[:8] == b"\x89PNG\r\n\x1a\n" and png[12:16] == b"IHDR", f"{name}: PNG"
    assert struct.unpack(">II", png[16:24]) == (1200, 630), f"{name}: dimensions"
css = (ROOT / "style.css").read_text()
assert "@import" not in css
for asset in re.findall(r"url\([\"']?([^\"')]+)", css):
    assert asset.startswith("/") and not asset.startswith("//") and (ROOT / asset.lstrip("/")).is_file()
headers = (ROOT / "_headers").read_text()
assert "default-src 'none'" in headers and "script-src" not in headers, "executable scripts must stay blocked"
print(f"PASS: {len(EXPECTED)} allowed public files; {len(pages)} HTML pages; {len(ROUTES)} unique canonical/metadata/JSON-LD records; links/anchors; sitemap/robots/llms; 1200x630 PNGs; no executable scripts or external assets.")
