"""Check the explicit static output and local HTML links; no dependencies."""
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).parent / "dist"
EXPECTED = {"index.html", "docs/index.html", "404.html", "style.css", "_headers", "fonts/FragmentMono-Regular.woff2", "fonts/OFL.txt"}
assert {str(p.relative_to(ROOT)) for p in ROOT.rglob("*") if p.is_file()} == EXPECTED


class Page(HTMLParser):
    def __init__(self, path):
        super().__init__(convert_charrefs=True)
        self.path, self.ids, self.links = path, set(), []
        self.headings = self.mains = 0
        self.feed(path.read_text())
        assert self.headings == self.mains == 1, f"{path}: needs one h1 and main"

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        assert tag != "script", f"{self.path}: no scripts needed"
        if "id" in attrs:
            assert attrs["id"] not in self.ids, f"{self.path}: duplicate id"
            self.ids.add(attrs["id"])
        self.headings += tag == "h1"
        self.mains += tag == "main"
        for key in ("href", "src"):
            if key in attrs:
                self.links.append(attrs[key])


pages = {p: Page(p) for p in ROOT.rglob("*.html")}
for path, page in pages.items():
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
print("PASS: public file allowlist, 3 static pages, local links and anchors; no scripts.")
