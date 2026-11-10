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
scripts and source notes. `_headers` sets a restrictive CSP
and disables access to camera, microphone and location.

Canonical origin: `https://toanother.one`. The existing Pages project is
`another-site-review`, production branch `main`. Deploy only after checks and
independent review; domain and DNS changes need separate authorization. Source
and capture files, including `og-source.jpg` and `render-brand.py`, stay out of
the public allowlist. `render-brand.py` rebuilds the icon set and the social
images from the mark in that file. The words are JetBrains Mono ExtraBold
(see [fonts/jetbrains/README.md](fonts/jetbrains/README.md)). It needs `fonttools`
(`pip install fonttools`). The site build copies the
generated files and does not run the script.

`scripts/check-copy.py` rejects em dashes, en dashes, and a banned-phrase list
in this directory and in `docs/PROMOTION.md`. Canonicals describe the public
URLs. Deploying to Cloudflare Pages is a maintainer action. CI does not deploy.
The checker validates generated local robots.txt. Cloudflare may prepend managed
crawler policy to the production response. Local checks do not prove that
managed policy matches the source.

## Content

Product claims come from the repository README and docs. The Alice and Bob
handoff on the homepage and in the docs was captured from `another 0.1.0`
against a temporary project named `shop`. Ids in that capture stay as recorded.
The authenticated `another serve` line in the docs uses a comma before the node
id, matching the binary. Keep new examples in sync with the CLI.

Fragment Mono is the headline, wordmark and command face. Prose uses Helvetica
Neue, Helvetica, Arial, then a generic sans. The font and its OFL license are
local. Provenance and hashes are in [fonts/README.md](fonts/README.md). Both
themes use paper, ink, and an ochre signal. Docs and the 404 show their route
beside the wordmark. Screenshots belong on the pull request, not in this tree.
