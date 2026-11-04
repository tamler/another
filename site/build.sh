#!/bin/sh
set -eu
cd "$(dirname "$0")"
# Replace only generated output, so removed pages cannot remain deployable.
rm -rf dist
mkdir -p dist/docs/getting-started dist/docs/install dist/compare dist/fonts
cp index.html style.css 404.html _headers favicon.svg og.png twitter.png sitemap.xml robots.txt llms.txt dist/
cp docs/index.html dist/docs/index.html
cp docs/getting-started/index.html dist/docs/getting-started/index.html
cp docs/install/index.html dist/docs/install/index.html
cp compare/index.html dist/compare/index.html
cp fonts/FragmentMono-Regular.woff2 fonts/OFL.txt dist/fonts/
python3 check.py
