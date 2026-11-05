#!/bin/sh
set -eu
cd "$(dirname "$0")"
# Replace only generated output, so removed pages cannot remain deployable.
rm -rf dist
mkdir -p dist/docs/getting-started dist/docs/install dist/compare dist/fonts
mkdir -p dist/guides/claude-code-codex dist/guides/mcp-agent-messaging dist/guides/avoid-agent-file-conflicts dist/guides/local-agent-inbox
cp index.html style.css 404.html _headers favicon.svg og.png twitter.png sitemap.xml robots.txt llms.txt dist/
cp docs/index.html dist/docs/index.html
cp docs/getting-started/index.html dist/docs/getting-started/index.html
cp docs/install/index.html dist/docs/install/index.html
cp compare/index.html dist/compare/index.html
cp guides/index.html dist/guides/index.html
cp guides/claude-code-codex/index.html dist/guides/claude-code-codex/index.html
cp guides/mcp-agent-messaging/index.html dist/guides/mcp-agent-messaging/index.html
cp guides/avoid-agent-file-conflicts/index.html dist/guides/avoid-agent-file-conflicts/index.html
cp guides/local-agent-inbox/index.html dist/guides/local-agent-inbox/index.html
cp fonts/FragmentMono-Regular.woff2 fonts/OFL.txt dist/fonts/
python3 check.py
python3 check_checksum.py
