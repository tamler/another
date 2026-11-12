# Verify a release archive

No GitHub Release of Another has been published as of 12 Nov 2026. This page describes what the release workflow will publish and how to check it, and records a run of those checks against a local fixture archive, labeled as such. Until a release exists, install with Cargo or build from source ([README](../README.md#quickstart)).

## What the workflow publishes

Pushing a tag `vX.Y.Z` runs [`.github/workflows/release.yml`](../.github/workflows/release.yml). For each target it writes two files and uploads both to the release:

| File | Written by |
|---|---|
| `another-VERSION-TARGET.tar.gz` | `tar -czf` of a folder named `another-VERSION-TARGET` holding the `another` binary, `LICENSE`, and `README.md` |
| `another-VERSION-TARGET.tar.gz.sha256` | `shasum -a 256 another-VERSION-TARGET.tar.gz` |

`TARGET` is `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`. The checksum file is one line in the format both `shasum` and `sha256sum` write and read: the hex digest, two spaces, the archive's file name. The workflow does not publish a combined `SHA256SUMS` file; if one appears on a release page later, the same commands below read it, and `--ignore-missing` skips the archives you did not download.

Download the archive and its `.sha256` from the same release page, put them in one folder, and run the check from that folder. The file name in the checksum line must match the archive's name on disk, so do not rename the download.

## The commands

macOS ships `shasum`:

```sh
shasum -a 256 -c another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
```

Linux ships GNU `sha256sum`:

```sh
sha256sum -c another-0.1.0-aarch64-unknown-linux-musl.tar.gz.sha256
```

Both print `<archive>: OK` and exit 0 on a match. On a mismatch they print `<archive>: FAILED`, a warning, and exit 1. Check the exit status in a script, not just the text. A failed check means stop: delete the download, do not unpack it, do not run it. Fetch the files again; if the check fails twice, do not install from that release.

Only after the check passes:

```sh
tar -xzf another-0.1.0-aarch64-apple-darwin.tar.gz
```

On macOS use `shasum`, not the `sha256sum` that newer macOS versions also ship. The Darwin `sha256sum -c` exits 0 when the checksum file has no line it can read, so an empty or damaged `.sha256` passes with only a warning. `shasum -a 256 -c` exits 1 in that case. The run below shows both. The install page has a [paste-in snippet](https://toanother.one/docs/install/#verify) that avoids `-c` altogether and compares the digests itself.

## A recorded run against a fixture

This is fixture data, not a release. The archive was built the way the workflow builds one, from a text file standing in for the binary, and its `.sha256` was written with the same `shasum -a 256` command the workflow uses. Run on 12 Nov 2026 on macOS 27.2.1 with `shasum` 6.02 (Perl) and the Darwin `sha256sum` 1.0 in `/sbin`. Exit statuses are as printed by the shell.

```text
$ cat another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
52819bcb63b6504024c42b556c86ff073b3a73cca42e95567d7ffbdb11768d2b  another-0.1.0-aarch64-apple-darwin.tar.gz
$ shasum -a 256 -c another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
another-0.1.0-aarch64-apple-darwin.tar.gz: OK
[exit 0]
$ sha256sum -c another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
another-0.1.0-aarch64-apple-darwin.tar.gz: OK
[exit 0]
```

Then one byte was appended to the archive:

```text
$ shasum -a 256 -c another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
another-0.1.0-aarch64-apple-darwin.tar.gz: FAILED
shasum: WARNING: 1 computed checksum did NOT match
[exit 1]
$ sha256sum -c another-0.1.0-aarch64-apple-darwin.tar.gz.sha256
sha256sum: WARNING: 1 computed checksum did NOT match
another-0.1.0-aarch64-apple-darwin.tar.gz: FAILED
[exit 1]
```

A checksum line that names a file you do not have also fails, which is what happens after a rename:

```text
$ cat wrong-name.sha256
52819bcb63b6504024c42b556c86ff073b3a73cca42e95567d7ffbdb11768d2b  another-0.1.0-x86_64-apple-darwin.tar.gz
$ shasum -a 256 -c wrong-name.sha256
shasum: another-0.1.0-x86_64-apple-darwin.tar.gz: No such file or directory
another-0.1.0-x86_64-apple-darwin.tar.gz: FAILED open or read
shasum: WARNING: 1 listed file could not be read
[exit 1]
$ sha256sum -c wrong-name.sha256
sha256sum:  another-0.1.0-x86_64-apple-darwin.tar.gz: No such file or directory
[exit 1]
```

With a combined file that lists an archive you did not download, both tools exit 1 unless told to skip it:

```text
$ sha256sum -c SHA256SUMS
sha256sum:  another-0.1.0-x86_64-unknown-linux-musl.tar.gz: No such file or directory
another-0.1.0-aarch64-apple-darwin.tar.gz: OK
[exit 1]
$ sha256sum -c --ignore-missing SHA256SUMS
another-0.1.0-aarch64-apple-darwin.tar.gz: OK
[exit 0]
```

`shasum -a 256 -c SHA256SUMS` behaved the same way, including `--ignore-missing`.

A checksum file with no usable line is where the two macOS tools part ways. `shasum` refuses it and exits 1. The Darwin `sha256sum` warns and exits 0, so a script that trusts its exit status would go on to unpack an unverified archive:

```text
$ cat malformed.sha256
not-a-checksum  another-0.1.0-aarch64-apple-darwin.tar.gz
$ shasum -a 256 -c malformed.sha256
shasum: malformed.sha256: no properly formatted SHA checksum lines found
[exit 1]
$ sha256sum -c malformed.sha256
sha256sum: WARNING: 1 line is improperly formatted
[exit 0]
$ shasum -a 256 -c empty.sha256
shasum: empty.sha256: no properly formatted SHA checksum lines found
[exit 1]
$ sha256sum -c empty.sha256
[exit 0]
```

Not run: GNU coreutils `sha256sum` on Linux. The Darwin `sha256sum` used here is a separate implementation, and no Linux machine or container was available during the capture. The command line and the `OK` / `FAILED` output shape are what coreutils documents; the exit statuses above were observed on macOS only.

## What a matching checksum proves

A match proves the bytes you have are the bytes the checksum file names. It does not say who built them. The archives are unsigned, and the `.sha256` sits next to the archive on the same release page, so anyone who can replace one can replace the other. Verification protects against a truncated or corrupted download and against a file that was swapped on one of the two paths, not against a compromised publisher. If that distinction matters to you, build from source with `cargo install --locked --git https://github.com/tamler/another another`, which fetches the code from the repository and compiles it on your machine.
