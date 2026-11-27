#!/bin/sh
set -eu

fail() {
    printf '%s\n' "$1" 'Install from source with Cargo:' \
        '  cargo install --locked --git https://github.com/tamler/another another' >&2
    exit 1
}

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
    Linux:x86_64) target=x86_64-unknown-linux-musl ;;
    *) fail 'No prebuilt binary for this operating system and architecture.' ;;
esac

command -v curl >/dev/null 2>&1 || fail 'The prebuilt installer needs curl.'
releases=https://github.com/tamler/another/releases
fetch() {
    curl --disable --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' "$@"
}

release=$(fetch --output /dev/null --write-out '%{url_effective}' "$releases/latest") ||
    fail 'Could not find or download a published release from GitHub.'
case "$release" in
    "$releases"/tag/v*) version=${release#"$releases/tag/v"} ;;
    *) fail 'GitHub did not return a published release.' ;;
esac
case "$version" in
    ''|*[!0-9A-Za-z.+-]*) fail 'GitHub returned an invalid release version.' ;;
esac

name=another-$version-$target
archive=$name.tar.gz
work=$(mktemp -d) || fail 'Could not create a temporary download directory.'
stage=
trap 'rm -rf "$work"; if [ -n "$stage" ]; then rm -rf "$stage"; fi' 0
trap 'exit 1' HUP INT TERM

url=$releases/download/v$version/$archive
if ! fetch --output "$work/$archive" "$url" ||
    ! fetch --output "$work/$archive.sha256" "$url.sha256"; then
    fail "Could not download $archive and its SHA-256 checksum."
fi

if command -v shasum >/dev/null 2>&1; then
    actual=$(cd "$work" && shasum -a 256 "$archive") || fail 'Could not hash the archive.'
elif command -v sha256sum >/dev/null 2>&1; then
    actual=$(cd "$work" && sha256sum "$archive") || fail 'Could not hash the archive.'
else
    fail 'The prebuilt installer needs shasum or sha256sum.'
fi
expected=$(cat "$work/$archive.sha256") || fail 'Could not read the SHA-256 checksum.'
[ "$expected" = "${actual%% *}  $archive" ] ||
    fail "SHA-256 verification failed for $archive."

tar -xzf "$work/$archive" -C "$work" "$name/another" ||
    fail "Could not unpack the binary from $archive."
bin_dir=${ANOTHER_INSTALL_DIR:-"$HOME/.local/bin"}
case "$bin_dir" in
    /*) ;;
    *) bin_dir=$PWD/$bin_dir ;;
esac
mkdir -p "$bin_dir" || fail "Could not create $bin_dir."
[ ! -d "$bin_dir/another" ] || fail "$bin_dir/another is a directory."
stage=$(mktemp -d "$bin_dir/.another-install.XXXXXX") ||
    fail "Could not stage the binary in $bin_dir."
# Rename a new file into place so macOS sees a new executable inode.
if ! install -m 755 "$work/$name/another" "$stage/another" ||
    ! mv -f "$stage/another" "$bin_dir/another"; then
    fail "Could not install the binary in $bin_dir."
fi
printf 'Installed %s\nAdd %s to PATH if needed.\n' "$bin_dir/another" "$bin_dir"
