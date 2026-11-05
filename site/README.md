# Another website

Static marketing and documentation pages for Another. Buildless HTML and CSS,
with a self-hosted monospace font and native sans-serif prose; no client
JavaScript, external asset requests, analytics code, package dependencies or framework.
Static JSON-LD describes the software; it does not execute. Cloudflare may inject
its configured insights beacon into served responses.

## Build and preview

From the repository root, with POSIX sh and Python 3:

```sh
sh site/build.sh
python3 -m http.server 8765 --bind 127.0.0.1 --directory site/dist
```

Open `http://127.0.0.1:8765`. The build checks its public file allowlist, HTML
landmarks, local links and anchors, unique SEO metadata, canonical URLs, JSON-LD,
sitemap coverage, robots/llms files and 1200×630 social images. It also runs the
install page's checksum snippet against local fixture archives, which needs
`sha256sum` or `shasum`. It does not replace browser or accessibility
review. The stdlib server returns its own error page for missing URLs; Pages uses
the provided `404.html`.

## Cloudflare Pages

- Build command: `sh site/build.sh`
- Output directory: `site/dist`
- No environment variables or runtime bindings required.

The build copies an explicit public file list. It excludes this README, check
scripts, source notes and `review/` screenshots. `_headers` sets a restrictive CSP
and disables access to camera, microphone and location.

Canonical origin: `https://toanother.one`. The existing Pages project is
`another-site-review`, production branch `main`. Deploy only after checks and
independent review; domain and DNS changes need separate authorization. Source
and capture files, including `og-source.jpg`, stay out of the public allowlist.

The seo2 round is local-preview and draft-PR work only; it does not authorize a
production deployment. Canonicals describe the intended public URLs. The checker
validates generated local robots.txt; Cloudflare may prepend managed crawler
policy to the production response. Local output checks do not prove that managed
policy matches the source, and no Cloudflare setting is changed by this round.

## Content and visual review

Product claims come from the repository README and docs; command excerpts come
from the actual binary. See [review/TRANSCRIPT.md](review/TRANSCRIPT.md) for source
and capture details. Keep the examples in sync with the CLI and source docs.

Fragment Mono gives the headline, wordmark and commands the same terminal voice;
prose uses Helvetica Neue/Helvetica/Arial with a generic sans-serif fallback.
The unmodified font and its OFL license are included locally; provenance and
hashes are in [fonts/README.md](fonts/README.md). Both themes use neutral paper/ink
and an ochre signal color. The single-line desktop title, balanced mobile title
and actual commands supply the identity; docs and 404 show their route beside
the wordmark. Review generic fallback rendering in both themes as well as native
rendering.

`review/` holds full-page screenshots of all pages at desktop and mobile widths,
with deliberate light and dark themes and fallback fonts, plus full-size detail
captures of inline type and the horizontally scrollable inbox columns. Screenshot
capture omits Puppeteer's default `--hide-scrollbars` flag. This verifies native
Chromium behavior, not physical iOS or other overlay-scrollbar settings.
Refresh affected screenshots and checks after visual changes. The cross-family
design review asks only whether any part looks or reads like generic AI output.
Every finding must be fixed and confirmed against refreshed captures before
acceptance; unresolved findings require a decision, not a completion claim.
