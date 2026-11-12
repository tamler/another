"""Validate the public output, inert structured data and SEO; stdlib only."""
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import struct
from urllib.parse import urlsplit
import xml.etree.ElementTree as ET

from stamp_icons import icon_version, stamped_href

ROOT = Path(__file__).parent / "dist"
ORIGIN = "https://toanother.one"
ROUTES = {
    "/", "/docs/", "/docs/getting-started/", "/docs/install/", "/compare/", "/guides/",
    "/guides/claude-code-codex/", "/guides/mcp-agent-messaging/",
    "/guides/avoid-agent-file-conflicts/", "/guides/local-agent-inbox/",
    "/docs/install/macos/", "/docs/install/linux/", "/docs/install/windows/",
    "/docs/faq/", "/docs/concepts/", "/compare/alternatives/",
    "/guides/git-worktrees/", "/guides/hand-off-tasks/", "/guides/share-model-server/",
    "/guides/wake-idle-agents/", "/guides/agents-on-two-machines/",
    "/guides/multiple-claude-code-sessions/",
    "/changelog/", "/building/",
}
GUIDES = {route for route in ROUTES if route.startswith("/guides/") and route != "/guides/"}
# Nested pages outside /guides/ carry an explicit trail from the home page.
TRAILS = {
    "/docs/getting-started/": ["/", "/docs/", "/docs/getting-started/"],
    "/docs/install/": ["/", "/docs/", "/docs/install/"],
    "/docs/install/macos/": ["/", "/docs/", "/docs/install/", "/docs/install/macos/"],
    "/docs/install/linux/": ["/", "/docs/", "/docs/install/", "/docs/install/linux/"],
    "/docs/install/windows/": ["/", "/docs/", "/docs/install/", "/docs/install/windows/"],
    "/docs/faq/": ["/", "/docs/", "/docs/faq/"],
    "/docs/concepts/": ["/", "/docs/", "/docs/concepts/"],
    "/compare/alternatives/": ["/", "/compare/", "/compare/alternatives/"],
}
EXPECTED = {
    "index.html", "docs/index.html", "docs/getting-started/index.html",
    "docs/install/index.html", "compare/index.html", "404.html", "style.css",
    "_headers", "fonts/FragmentMono-Regular.woff2", "fonts/OFL.txt", "og.png",
    "twitter.png", "sitemap.xml", "robots.txt", "llms.txt", "favicon.svg",
    "favicon.ico", "apple-touch-icon.png", "icon-192.png", "icon-512.png", "site.webmanifest",
    "guides/index.html", "guides/claude-code-codex/index.html", "guides/mcp-agent-messaging/index.html",
    "guides/avoid-agent-file-conflicts/index.html", "guides/local-agent-inbox/index.html",
    "docs/install/macos/index.html", "docs/install/linux/index.html", "docs/install/windows/index.html",
    "docs/faq/index.html", "docs/concepts/index.html", "compare/alternatives/index.html",
    "guides/git-worktrees/index.html", "guides/hand-off-tasks/index.html", "guides/share-model-server/index.html",
    "guides/wake-idle-agents/index.html", "guides/agents-on-two-machines/index.html",
    "guides/multiple-claude-code-sessions/index.html",
    "changelog/index.html", "building/index.html",
    "illustrations/path-holds.svg", "illustrations/handoff.svg", "illustrations/mid-task-notice.svg",
}
# The page's own JSON-LD type, beside the software it describes.
def page_type(route):
    return {"/": "WebSite", "/guides/": "CollectionPage", "/docs/faq/": "FAQPage"}.get(route, "TechArticle")


actual = {str(p.relative_to(ROOT)) for p in ROOT.rglob("*") if p.is_file()}
assert actual == EXPECTED, f"public allowlist mismatch: {actual ^ EXPECTED}"
assert not any(p.is_symlink() for p in ROOT.rglob("*")), "no public symlinks"


class Page(HTMLParser):
    def __init__(self, path):
        super().__init__(convert_charrefs=True)
        self.path, self.ids, self.links = path, set(), []
        self.in_main_nav, self.main_nav_links = False, set()
        self.in_main, self.main_links = False, set()
        self.in_h1, self.h1 = False, ""
        self.meta, self.canonicals, self.structured = {}, [], []
        self.title, self.in_title, self.in_script, self.script_data = "", False, False, ""
        self.text = ""
        self.headings = self.mains = 0
        self.relations = []
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
        if tag == "link":
            self.relations.append((attrs.get("rel", ""), attrs.get("href", "")))
        if tag == "link" and attrs.get("rel") == "canonical":
            self.canonicals.append(attrs.get("href"))
        if "id" in attrs:
            assert attrs["id"] not in self.ids, f"{self.path}: duplicate id"
            self.ids.add(attrs["id"])
        self.headings += tag == "h1"
        self.mains += tag == "main"
        self.in_main |= tag == "main"
        self.in_h1 |= tag == "h1"
        if tag == "nav" and attrs.get("aria-label") == "Main":
            self.in_main_nav = True
        if tag == "a" and self.in_main_nav:
            self.main_nav_links.add(attrs.get("href"))
        if tag == "a" and self.in_main:
            self.main_links.add(attrs.get("href"))
        for key in ("href", "src"):
            if key in attrs:
                self.links.append(attrs[key])
        for key in ("src", "srcset", "poster", "data"):
            if key in attrs:
                assert key == "src" and attrs[key].startswith("/") and not attrs[key].startswith("//"), f"{self.path}: external/unsupported asset"
        if tag == "link" and attrs.get("rel") in {"stylesheet", "preload", "icon", "apple-touch-icon", "manifest"}:
            assert attrs.get("href", "").startswith("/") and not attrs["href"].startswith("//"), f"{self.path}: external asset"

    def handle_endtag(self, tag):
        if tag == "nav":
            self.in_main_nav = False
        if tag == "main":
            self.in_main = False
        if tag == "h1":
            self.in_h1 = False
        if tag == "title":
            self.in_title = False
        if tag == "script":
            assert self.in_script, f"{self.path}: unexpected script close"
            self.structured.append(json.loads(self.script_data))
            self.in_script = False

    def handle_data(self, data):
        if not self.in_script:
            self.text += data
        if self.in_h1:
            self.h1 += data
        if self.in_title:
            self.title += data
        if self.in_script:
            self.script_data += data


pages = {p: Page(p) for p in ROOT.rglob("*.html")}
ICON_VERSION = icon_version(ROOT)
ICON_LINKS = (
    ("icon", stamped_href("/favicon.ico", ICON_VERSION)),
    ("icon", stamped_href("/favicon.svg", ICON_VERSION)),
    ("apple-touch-icon", stamped_href("/apple-touch-icon.png", ICON_VERSION)),
    ("manifest", stamped_href("/site.webmanifest", ICON_VERSION)),
)
IMAGE_ALT = "Another. One inbox for coding agents."
for path, page in pages.items():
    for rel, href in ICON_LINKS:
        assert (rel, href) in page.relations, f"{path}: missing link rel={rel} href={href}"
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
    assert "/guides/" in page.main_nav_links, f"{path}: main nav must link to the guides hub"
    if path.name == "404.html":
        assert page.meta.get("robots") == "noindex" and not page.canonicals
        for key, value in {
            "og:image": ORIGIN + "/og.png",
            "og:image:width": "1200",
            "og:image:height": "630",
            "twitter:card": "summary_large_image",
            "twitter:image": ORIGIN + "/twitter.png",
        }.items():
            assert page.meta.get(key) == value, f"{path}: {key}"
        for key in ("og:image:alt", "twitter:image:alt"):
            assert page.meta.get(key) == IMAGE_ALT, f"{path}: {key}"
        continue
    route = "/" + path.relative_to(ROOT).as_posix().removesuffix("index.html")
    canonical = ORIGIN + route
    assert route in ROUTES and page.canonicals == [canonical], f"{path}: canonical"
    assert page.title and page.title not in titles, f"{path}: unique title"
    assert len(page.title) <= 60, f"{path}: title over 60 characters ({len(page.title)})"
    titles.add(page.title)
    description = page.meta.get("description", "")
    assert 130 <= len(description) <= 160 and description not in descriptions, f"{path}: unique description of 130 to 160 characters ({len(description)})"
    descriptions.add(description)
    assert "noindex" not in page.meta.get("robots", ""), f"{path}: unexpectedly noindex"
    required = {
        "og:type": "website", "og:site_name": "Another", "og:title": page.title,
        "og:description": description,
        "og:url": canonical, "og:image": ORIGIN + "/og.png",
        "og:image:width": "1200", "og:image:height": "630",
        "twitter:card": "summary_large_image", "twitter:title": page.title,
        "twitter:description": description,
        "twitter:image": ORIGIN + "/twitter.png",
    }
    for key, value in required.items():
        assert page.meta.get(key) == value, f"{path}: {key}"
    for key in ("og:image:alt", "twitter:image:alt"):
        assert page.meta.get(key) == IMAGE_ALT, f"{path}: {key}"
    assert len(page.structured) == 1, f"{path}: one JSON-LD graph"
    data = page.structured[0]
    assert data["@context"] == "https://schema.org"
    graph = {item["@type"]: item for item in data["@graph"]}
    software = {"SoftwareApplication", "SoftwareSourceCode"}
    in_guides = route.startswith("/guides/")
    trail_routes = TRAILS.get(route) or (["/", "/guides/"] + ([route] if route in GUIDES else []) if in_guides else None)
    own_type = page_type(route)
    expected_types = software | {own_type} | ({"BreadcrumbList"} if trail_routes else set())
    assert set(graph) == expected_types, f"{path}: JSON-LD types"
    own = graph[own_type]
    assert own.get("author") == {"@type": "Organization", "name": "The Another Authors"}, f"{path}: {own_type} author"
    assert own.get("url") == canonical, f"{path}: {own_type} url"
    assert own.get("description") == description, f"{path}: {own_type} description"
    if own_type in {"TechArticle", "CollectionPage"}:
        heading = " ".join(page.h1.split())
        assert own["headline" if own_type == "TechArticle" else "name"] == heading, f"{path}: {own_type} must name the visible h1"
        assert own["about"] == {"@id": ORIGIN + "/#application"}, f"{path}: {own_type} about"
    assert graph["SoftwareApplication"]["name"] == "Another"
    assert graph["SoftwareSourceCode"]["codeRepository"] == "https://github.com/tamler/another"
    assert all(graph[kind]["license"] == "https://www.apache.org/licenses/LICENSE-2.0" for kind in software)
    if route == "/docs/faq/":
        visible = " ".join(page.text.split())
        questions = graph["FAQPage"]["mainEntity"]
        assert questions, f"{path}: FAQPage needs questions"
        for question in questions:
            assert question["@type"] == "Question" and question["acceptedAnswer"]["@type"] == "Answer"
            for said in (question["name"], question["acceptedAnswer"]["text"]):
                assert said in visible, f"{path}: FAQ JSON-LD not on the page: {said}"
    if trail_routes:
        crumbs = graph["BreadcrumbList"]["itemListElement"]
        trail = [ORIGIN + step for step in trail_routes]
        assert [c["item"] for c in crumbs] == trail, f"{path}: breadcrumb trail"
        assert [c["position"] for c in crumbs] == list(range(1, len(trail) + 1)), f"{path}: breadcrumb positions"
        assert all(c["@type"] == "ListItem" and c["name"] for c in crumbs), f"{path}: breadcrumb items"
    canonical_urls.add(canonical)

assert canonical_urls == {ORIGIN + route for route in ROUTES}, "every public route needs metadata"
hub_links = set(pages[ROOT / "guides/index.html"].links)
assert GUIDES <= hub_links, f"guides hub misses {GUIDES - hub_links}"
# Every route must be reachable by following links from the home page.
reached, frontier = {"/"}, ["/"]
while frontier:
    current = pages[ROOT / frontier.pop().lstrip("/") / "index.html"]
    for href in current.links:
        step = urlsplit(href)
        if not step.scheme and not step.netloc and step.path in ROUTES and step.path not in reached:
            reached.add(step.path)
            frontier.append(step.path)
assert reached == ROUTES, f"not reachable from the home page: {ROUTES - reached}"
# Header and footer navigation don't count: a page needs related pages pointing at it.
inbound = {route: set() for route in ROUTES}
for path, page in pages.items():
    if path.name == "404.html":
        continue
    source = "/" + path.relative_to(ROOT).as_posix().removesuffix("index.html")
    for href in page.main_links:
        step = urlsplit(href)
        if not step.scheme and not step.netloc and step.path in ROUTES and step.path != source:
            inbound[step.path].add(source)
thin = {route for route, sources in inbound.items() if route != "/" and len(sources) < 2}
assert not thin, f"fewer than two in-content links from other pages: {thin}"
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
PNG_SIZES = {
    "og.png": (1200, 630),
    "twitter.png": (1200, 630),
    "apple-touch-icon.png": (180, 180),
    "icon-192.png": (192, 192),
    "icon-512.png": (512, 512),
}
for name, size in PNG_SIZES.items():
    png = (ROOT / name).read_bytes()
    assert png[:8] == b"\x89PNG\r\n\x1a\n" and png[12:16] == b"IHDR", f"{name}: PNG"
    assert struct.unpack(">II", png[16:24]) == size, f"{name}: dimensions"
assert (ROOT / "og.png").read_bytes() != (ROOT / "twitter.png").read_bytes(), "social images must differ"
ico = (ROOT / "favicon.ico").read_bytes()
reserved, kind, count = struct.unpack_from("<HHH", ico, 0)
assert (reserved, kind, count) == (0, 1, 3), "favicon.ico header"
sizes = []
for index in range(count):
    width, height, _colors, _reserved, planes, bitcount, nbytes, offset = struct.unpack_from("<BBBBHHII", ico, 6 + 16 * index)
    blob = ico[offset:offset + nbytes]
    assert planes == 1 and bitcount == 32 and blob[:8] == b"\x89PNG\r\n\x1a\n", "favicon.ico entry"
    stored = struct.unpack(">II", blob[16:24])
    assert (width, height) == stored, "favicon.ico size"
    sizes.append(stored)
assert sizes == [(16, 16), (32, 32), (48, 48)], sizes
manifest = json.loads((ROOT / "site.webmanifest").read_text())
assert manifest["name"] == "Another" and manifest["display"] == "browser"
assert [(item["src"], item["sizes"], item["type"]) for item in manifest["icons"]] == [
    (stamped_href("/icon-192.png", ICON_VERSION), "192x192", "image/png"),
    (stamped_href("/icon-512.png", ICON_VERSION), "512x512", "image/png"),
]
svg = (ROOT / "favicon.svg").read_text()
assert 'fill="#f8f8f4"' in svg and 'fill="#292922"' in svg and 'fill="#705000"' in svg
assert "<text" not in svg, "favicon.svg should be the mark, not a letter"
css = (ROOT / "style.css").read_text()
assert "@import" not in css
for asset in re.findall(r"url\([\"']?([^\"')]+)", css):
    assert asset.startswith("/") and not asset.startswith("//") and (ROOT / asset.lstrip("/")).is_file()
headers = (ROOT / "_headers").read_text()
assert "default-src 'none'" in headers and "script-src" not in headers, "executable scripts must stay blocked"
assert "manifest-src 'self'" in headers, "the web app manifest must be allowed from this site"
print(f"PASS: {len(EXPECTED)} allowed public files; {len(pages)} HTML pages; {len(ROUTES)} unique canonical/metadata/JSON-LD records; links/anchors; sitemap/robots/llms; icons and 1200x630 social images; no executable scripts or external assets.")
