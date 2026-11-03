#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p dist/docs dist/fonts
cp index.html style.css 404.html _headers dist/
cp docs/index.html dist/docs/index.html
cp fonts/FragmentMono-Regular.woff2 fonts/OFL.txt dist/fonts/
python3 check.py
