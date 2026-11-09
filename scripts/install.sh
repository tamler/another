#!/bin/sh
# Install the `another` binary from a GitHub release, verified against its SHA256SUMS.
#
#   curl -fsSL https://github.com/tamler/another/releases/latest/download/install.sh | sh
#
# Environment (all optional):
#   ANOTHER_VERSION       release to install, e.g. 0.1.0 (default: the latest release)
#   ANOTHER_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   ANOTHER_DOWNLOAD_BASE base URL of the release files (default: the GitHub release for
#                         that version; used by tests and mirrors)
set -eu

REPO="tamler/another"
DIR="${ANOTHER_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${ANOTHER_VERSION:-}"

say() { printf 'another-install: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) TARGET=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  *) fail "no prebuilt binary for $(uname -s) $(uname -m); build from source: cargo install --locked --git https://github.com/$REPO another" ;;
esac

if [ -z "$VERSION" ]; then
  # The latest release, from the redirect GitHub serves (no API call, no rate limit).
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    fail "could not reach GitHub to find the latest release; set ANOTHER_VERSION"
  VERSION="${url##*/tag/v}"
  [ "$VERSION" != "$url" ] || fail "could not find the latest release; set ANOTHER_VERSION"
fi
VERSION="${VERSION#v}"
BASE="${ANOTHER_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download/v$VERSION}"
ARCHIVE="another-$VERSION-$TARGET.tar.gz"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

say "downloading another $VERSION for $TARGET"
curl -fsSL -o "$TMP/$ARCHIVE" "$BASE/$ARCHIVE" || fail "download failed: $BASE/$ARCHIVE"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" || fail "download failed: $BASE/SHA256SUMS"

# Verify before unpacking anything: the archive must be listed and must match.
grep " $ARCHIVE\$" "$TMP/SHA256SUMS" > "$TMP/expected" || fail "$ARCHIVE is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$TMP" && sha256sum -c expected >/dev/null 2>&1) || fail "checksum mismatch for $ARCHIVE; not installing"
elif command -v shasum >/dev/null 2>&1; then
  (cd "$TMP" && shasum -a 256 -c expected >/dev/null 2>&1) || fail "checksum mismatch for $ARCHIVE; not installing"
else
  fail "need sha256sum or shasum to verify the download"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
mkdir -p "$DIR"
# Replace by moving a fresh file into place (copying over a running or signed binary in place
# can leave macOS with a stale code signature).
cp "$TMP/another-$VERSION-$TARGET/another" "$DIR/.another.new"
chmod 0755 "$DIR/.another.new"
mv -f "$DIR/.another.new" "$DIR/another"

say "installed $DIR/another ($("$DIR/another" --version 2>/dev/null || echo "another $VERSION"))"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "add it to your PATH: export PATH=\"$DIR:\$PATH\"" ;;
esac
say "next: another init && another doctor"
