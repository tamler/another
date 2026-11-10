#!/bin/sh
set -eu
cd "$(dirname "$0")"
python3 ../scripts/check-copy.py
# Replace only generated output, so removed pages cannot remain deployable.
rm -rf dist
mkdir -p dist/docs/getting-started dist/docs/install dist/compare dist/fonts
mkdir -p dist/guides/claude-code-codex dist/guides/mcp-agent-messaging dist/guides/avoid-agent-file-conflicts dist/guides/local-agent-inbox
mkdir -p dist/docs/install/macos dist/docs/install/linux dist/docs/install/windows dist/docs/faq dist/docs/concepts dist/compare/alternatives
mkdir -p dist/guides/git-worktrees dist/guides/hand-off-tasks dist/guides/share-model-server dist/guides/wake-idle-agents dist/guides/agents-on-two-machines
mkdir -p dist/guides/multiple-claude-code-sessions dist/changelog dist/building
cp index.html style.css 404.html _headers favicon.svg favicon.ico apple-touch-icon.png icon-192.png icon-512.png site.webmanifest og.png twitter.png sitemap.xml robots.txt llms.txt dist/
cp docs/index.html dist/docs/index.html
cp docs/getting-started/index.html dist/docs/getting-started/index.html
cp docs/install/index.html dist/docs/install/index.html
cp compare/index.html dist/compare/index.html
cp guides/index.html dist/guides/index.html
cp guides/claude-code-codex/index.html dist/guides/claude-code-codex/index.html
cp guides/mcp-agent-messaging/index.html dist/guides/mcp-agent-messaging/index.html
cp guides/avoid-agent-file-conflicts/index.html dist/guides/avoid-agent-file-conflicts/index.html
cp guides/local-agent-inbox/index.html dist/guides/local-agent-inbox/index.html
cp docs/install/macos/index.html dist/docs/install/macos/index.html
cp docs/install/linux/index.html dist/docs/install/linux/index.html
cp docs/install/windows/index.html dist/docs/install/windows/index.html
cp docs/faq/index.html dist/docs/faq/index.html
cp docs/concepts/index.html dist/docs/concepts/index.html
cp compare/alternatives/index.html dist/compare/alternatives/index.html
cp guides/git-worktrees/index.html dist/guides/git-worktrees/index.html
cp guides/hand-off-tasks/index.html dist/guides/hand-off-tasks/index.html
cp guides/share-model-server/index.html dist/guides/share-model-server/index.html
cp guides/wake-idle-agents/index.html dist/guides/wake-idle-agents/index.html
cp guides/agents-on-two-machines/index.html dist/guides/agents-on-two-machines/index.html
cp guides/multiple-claude-code-sessions/index.html dist/guides/multiple-claude-code-sessions/index.html
cp changelog/index.html dist/changelog/index.html
cp building/index.html dist/building/index.html
cp fonts/FragmentMono-Regular.woff2 fonts/OFL.txt dist/fonts/
python3 check.py
python3 check_checksum.py
