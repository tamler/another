#!/bin/sh
# Run the README quickstart verbatim in a fresh HOME, as a new user would.
#
#   ANOTHER_BIN=target/release/another scripts/quickstart-ci.sh
#
# Until v0.1.0 is published, `releases/latest` has nothing to download, so the install
# step is served locally: a `curl` shim answers the quickstart's install.sh URL with this
# checkout's scripts/install.sh, and ANOTHER_VERSION/ANOTHER_DOWNLOAD_BASE point the script
# at a fake release built from ANOTHER_BIN (same archive layout and SHA256SUMS as
# release.yml). Every quickstart command still runs exactly as written.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
: "${ANOTHER_BIN:?set ANOTHER_BIN to the binary to install}"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

# The quickstart's `another serve &` needs port 7466. With another server there, its doctor
# step would pass against that server instead.
if curl -fsS -m 1 http://127.0.0.1:7466/health >/dev/null 2>&1; then
  echo "a server is already on 127.0.0.1:7466; run this on a machine without one" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
release="$work/release"
name="another-$version-$target"
mkdir -p "$release/$name" "$work/shim" "$work/home"
cp "$ANOTHER_BIN" "$release/$name/another"
(cd "$release" && tar -czf "$name.tar.gz" "$name" && rm -r "$name")
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$release" && sha256sum "$name.tar.gz" > SHA256SUMS)
else
  (cd "$release" && shasum -a 256 "$name.tar.gz" > SHA256SUMS)
fi
cp "$root/scripts/install.sh" "$release/install.sh"

real_curl=$(command -v curl)
cat > "$work/shim/curl" <<SHIM
#!/bin/sh
# Serve the not-yet-published release download locally; pass everything else through.
for a in "\$@"; do
  case "\$a" in
    https://github.com/tamler/another/releases/latest/download/*) set -- ;;
  esac
done
if [ \$# -eq 0 ]; then exec cat "$release/install.sh"; fi
exec "$real_curl" "\$@"
SHIM
chmod +x "$work/shim/curl"

# A new user: empty HOME, no `another` on PATH (only the shim and the system).
export HOME="$work/home"
export PATH="$work/shim:/usr/local/bin:/usr/bin:/bin"
export ANOTHER_VERSION="$version" ANOTHER_DOWNLOAD_BASE="file://$release"
unset ANOTHER_HOME ANOTHER_AGENT ANOTHER_LANE
if command -v another >/dev/null 2>&1; then
  echo "another is already on PATH ($(command -v another)); the install step would be skipped" >&2
  exit 1
fi
cd "$HOME"
python3 "$root/scripts/run-doc-commands.py" "$root/README.md" \
  "## Quickstart" "### Other ways to install"
