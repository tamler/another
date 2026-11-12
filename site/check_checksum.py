"""Run the install page's checksum snippet against local fixture archives; stdlib only.

The release workflow writes a `.sha256` file per archive. A combined-file
fixture also checks that the snippet does not fall back to it. No release is downloaded.
"""
import hashlib
from html.parser import HTMLParser
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

PAGE = Path(__file__).parent / "dist/docs/install/index.html"
LABEL = "Verify a release archive"
ARCHIVE = "another-0.1.0-aarch64-apple-darwin.tar.gz"
OTHER = "another-0.1.0-x86_64-unknown-linux-musl.tar.gz"
TOOLS = ("awk", "cat", "gzip", "printf", "sh", "tar")


class Snippet(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.found, self.inside, self.text = 0, False, ""

    def handle_starttag(self, tag, attrs):
        if tag == "pre" and dict(attrs).get("aria-label") == LABEL:
            self.found += 1
            self.inside = True

    def handle_endtag(self, tag):
        if tag == "pre":
            self.inside = False

    def handle_data(self, data):
        if self.inside:
            self.text += data


def extract():
    parser = Snippet()
    parser.feed(PAGE.read_text())
    assert parser.found == 1, f"{PAGE}: expected one '{LABEL}' block, found {parser.found}"
    # Interactive zsh, the macOS default, runs `#` as a command unless configured otherwise.
    assert "#" not in parser.text, "snippet must paste into interactive zsh: no comments"
    assert ARCHIVE in parser.text, f"snippet's example archive must stay {ARCHIVE}"
    return parser.text


def tool_dir(root, hasher):
    """A PATH holding only one hasher, so each branch of the snippet really runs."""
    bin_dir = root / f"bin-{hasher}"
    bin_dir.mkdir()
    for name in (*TOOLS, hasher):
        found = shutil.which(name)
        if found:
            (bin_dir / name).symlink_to(found)
    return bin_dir


def make_archive(path):
    source = path.parent / "payload"
    source.write_text(f"fixture payload for {path.name}\n")
    with tarfile.open(path, "w:gz") as tar:
        tar.add(source, arcname=f"{path.name.removesuffix('.tar.gz')}/another")
    source.unlink()


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture(directory, manifest, tamper=False):
    directory.mkdir()
    for name in (ARCHIVE, OTHER):
        make_archive(directory / name)
    lines = {name: f"{sha256(directory / name)}  {name}\n" for name in (ARCHIVE, OTHER)}
    if manifest == "per-archive":
        (directory / f"{ARCHIVE}.sha256").write_text(lines[ARCHIVE])
    if manifest == "per-archive-wrong-name":
        (directory / f"{ARCHIVE}.sha256").write_text(lines[OTHER])
    if manifest == "malformed":
        (directory / f"{ARCHIVE}.sha256").write_text(f"not-a-checksum  {ARCHIVE}\n")
    # Well-formed digests that are wrong, in layouts some `sha256sum -c` skip and still exit 0.
    if manifest == "wrong-leading-space":
        (directory / f"{ARCHIVE}.sha256").write_text(f" {'a' * 64}  {ARCHIVE}\n")
    if manifest == "wrong-tab":
        (directory / f"{ARCHIVE}.sha256").write_text(f"{'a' * 64}\t{ARCHIVE}\n")
    # Both orders: a recipe that trusts only the first or only the last line fails one of these.
    if manifest == "conflicting-wrong-first":
        (directory / f"{ARCHIVE}.sha256").write_text(f"{'a' * 64}  {ARCHIVE}\n{lines[ARCHIVE]}")
    if manifest == "conflicting-right-first":
        (directory / f"{ARCHIVE}.sha256").write_text(f"{lines[ARCHIVE]}{'a' * 64}  {ARCHIVE}\n")
    if manifest == "SHA256SUMS":
        (directory / "SHA256SUMS").write_text("".join(lines[name] for name in sorted(lines)))
    (directory / OTHER).unlink()
    if tamper:
        with open(directory / ARCHIVE, "ab") as handle:
            handle.write(b"tampered")


def run(shell, snippet, directory, bin_dir):
    if shell == "zsh":
        argv = [shutil.which("zsh"), "-f", "-i", "-s"]
    else:
        argv = [shutil.which(shell), "-s"]
    env = {"PATH": str(bin_dir), "HOME": str(directory), "LC_ALL": "C", "PS1": ""}
    return subprocess.run(argv, input=snippet, cwd=directory, env=env, text=True,
                          capture_output=True, timeout=60)


def main():
    snippet = extract()
    hashers = [name for name in ("sha256sum", "shasum") if shutil.which(name)]
    assert hashers, "need sha256sum or shasum to test the checksum snippet"
    shells = [name for name in ("sh", "bash", "zsh") if shutil.which(name)]
    cases = [
        ("per-archive", {"manifest": "per-archive"}, True),
        ("combined file only", {"manifest": "SHA256SUMS"}, False),
        ("per-archive tampered", {"manifest": "per-archive", "tamper": True}, False),
        ("per-archive names another file", {"manifest": "per-archive-wrong-name"}, False),
        ("malformed checksum line", {"manifest": "malformed"}, False),
        ("wrong digest after a leading space", {"manifest": "wrong-leading-space"}, False),
        ("wrong digest after a tab", {"manifest": "wrong-tab"}, False),
        ("conflicting lines, wrong digest first", {"manifest": "conflicting-wrong-first"}, False),
        ("conflicting lines, right digest first", {"manifest": "conflicting-right-first"}, False),
        ("no checksum file", {"manifest": None}, False),
    ]
    runs = 0
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp).resolve()
        for hasher in hashers:
            bin_dir = tool_dir(root, hasher)
            for shell in shells:
                for label, options, should_pass in cases:
                    directory = root / f"{hasher}-{shell}-{runs}"
                    fixture(directory, **options)
                    result = run(shell, snippet, directory, bin_dir)
                    where = f"{hasher}/{shell}/{label}: exit {result.returncode}\n{result.stdout}\n{result.stderr}"
                    unpacked = (directory / ARCHIVE.removesuffix(".tar.gz") / "another").is_file()
                    if should_pass:
                        assert result.returncode == 0 and unpacked, where
                        assert f"Verified and unpacked: {ARCHIVE}" in result.stdout, where
                    else:
                        assert result.returncode != 0, where
                        assert not unpacked, f"failed check must not unpack: {where}"
                        assert "FAILED" in result.stdout, where
                    if os.environ.get("CHECKSUM_RECIPE_VERBOSE"):
                        print(f"[{hasher} {shell}] {label}: exit {result.returncode}, unpacked={unpacked}")
                    runs += 1
    print(f"PASS: checksum snippet, {runs} fixture runs; hashers {', '.join(hashers)}; shells {', '.join(shells)}.")


main()
