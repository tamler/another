# Launch readiness checklist

State on 2026-11-05: **not ready to announce.** The documented install and commands work, but nothing has been released, the site's newer pages aren't deployed, and `main` lacks the reviewed hardening stack. Nothing here was merged, tagged, deployed or posted.

The commands below ran against the binary that the documented install produces today: `main` at 189c0ec, whose `crates/` are identical to this branch's. Each run used a temporary `HOME` on macOS arm64.

## Install

| Check | Result |
|---|---|
| `cargo install --locked --git …` on stable 1.96 | Pass |
| Same command on Rust 1.88 | Pass |
| Repeat with `--force` | Pass; replaces the binary |
| `git clone`, then `cargo build --release --locked` | Pass |
| `another --version`, `another init`, `another status` | Pass |
| `cargo fmt`, `clippy -D warnings`, `cargo test`, `cargo +1.88 check` | Pass; 80 tests, 0 failed |
| Linux install from source | Not run |

## Documented commands

| Area | Result |
|---|---|
| README quickstart, CLI table, `$ANOTHER_AGENT` default | Pass |
| Hooks: dry run writes nothing, idempotent install, backup, `0600` kept, symlink kept, existing hooks kept, invalid JSON refused with nothing written, uninstall | Pass |
| Hook events for Claude, Grok (stand-down and own pack) and Cursor output formats; `ANOTHER_AGENT` override | Pass |
| Mail, threads, read-only ack, requests through every state, `another mail` absent as documented | Pass |
| Holds: conflict exits 3, a batch with a conflict grants nothing | Pass |
| All four guide command blocks | Pass |
| Resources: register, borrow, heartbeat, release, `--stopped`, orphans with and without scan | Pass |
| Reap: plan only, grace block, `--confirm` sends SIGTERM (a `sleep` started by the test) | Pass |
| Reap `--force` SIGKILL | Not run |
| MCP over stdio: all 18 tools called, with the guide's arguments where it gives them; `fetch_inbox` repeat leaves mail unread; no bodies in the feed | Pass |
| Loopback HTTP MCP, on a test port because 7466 was in use | Pass; default port read from `serve --help` |
| Remote: non-loopback bind refused, tokens, 401 without a token, `--allowed-host`, audit log | Pass, with the `Host` correction below |
| Federation: pair, sync, cross-node mail and reply, requests refused across nodes, merged feed, revoke | Pass, over loopback and a local `Host`-rewriting proxy |
| Wake: stored targets, `--dry-run`, `--all`, `--force --message`, `--via herdr`; tmux and herdr through stand-ins, cmux dry run only | Pass |
| Herdr helper script: `summary`, `wake` | Pass |
| `herdr plugin install` and `link` | Not run; `link` could not start under the test's temporary paths |
| Real tmux, cmux and Herdr sessions; Tailscale, Cloudflare and `ssh -R` tunnels | Not run |
| Claude Code, Codex, Grok CLI and Cursor loading the hooks and MCP config | Not run; needs a human with those clients |

## Claims

| Claim | Result |
|---|---|
| Loopback only without `--auth` and `--allow-remote-bind` | Holds |
| No telemetry; network only where configured | Holds in source: outbound HTTP is only in federation transport, plus a TCP reachability check on registered endpoints. No traffic capture was run |
| "Another rejects unknown hosts" | **Was overstated.** True for `/mcp` only. Fixed in the docs |
| A hub kept on loopback and exposed through a tunnel | **Documented setup was unsafe.** See below |
| `another peers` shows last sync | Spokes only; hub shows `never`. Fixed in the docs |
| Site fixture output (register, send, ack, activity, wake dry run, token, serve) | Formats match the binary, read side by side rather than compared by a script |
| Site build and checker | Pass: 21 public files, 11 HTML pages |
| Repository links from the site, README and docs resolve on `main` | Pass, including the `#safety-defaults` anchor |
| Issue-template link | Resolves, but `main` has no templates until the site branch merges |
| External links | 39 of 42 return 200. crates.io and a Cursor canvas link in `SPEC.md` return 403 to curl, so neither was verified. The last is a template URL in a doc |

The production site was not requested in this round. [SEO4.md](SEO4.md) and the round-four audit hold the most recent production observations, taken the same day.

### The tunnelled hub

`FEDERATION.md` told hub operators to run `another serve --federation` on loopback and expose it through a tunnel. That server answers `GET /activity` with status 200, no token and any `Host`. Anyone who can reach the tunnel can read agent names, mail subjects and project paths. `/mcp` does return 403 for a foreign `Host`, and `/fed/*` still needs a node signature.

`another serve --auth --federation` closes this. Through a proxy rewriting `Host`, unauthenticated `/activity` and `/mcp` returned 401, while pairing, sync and cross-node mail still worked, with no `--allowed-host`. The docs now use that command. The README and site were changed to match.

PR #53's source adds a loopback `Host` check for servers without `--auth`. It was read, not run.

## Release artifacts and checksums

| Check | Result |
|---|---|
| Published releases and tags | None |
| Release workflow runs | 22, all pull-request builds; the publish job has never run |
| Latest PR build's four archives | All four `.sha256` files verify; each holds `another`, `LICENSE`, `README.md`; both Linux binaries are static |
| macOS arm64 archive runs | Pass, `another 0.1.0` |
| x86_64 macOS archive | Built, never run: the workflow's smoke test is off for it, and this machine has no Intel Mac or Rosetta |
| Linux archives | CI smoke test only; not run here |
| `shasum -a 256 -c` and `sha256sum -c` on a locally packaged archive | Pass; a tampered archive fails with exit 1 |
| Signatures | macOS binary is ad hoc linker-signed with no team; matches the site's "unsigned" statement |
| `xattr -d com.apple.quarantine` when the attribute is absent | Exits 1 with "No such xattr", as the install page warns |

Those archives came from the release stack's branch, not `main`. Its publish job also adds one `SHA256SUMS` and `install.sh`, so neither has ever been generated by a run; `main`'s workflow has per-archive files only. The install page recipe handles either file, so it does not depend on which workflow ships; see the polish round below.

Neither workflow compares the tag with the Cargo version, so a mismatched tag would publish misnamed archives.

## Privacy and names

| Check | Result |
|---|---|
| Personal names and email in the tree, built site and sampled image metadata | None found |
| An operator handle in `PLAN.md` | Removed |
| Commit identities on `main` | Noreply only, 9 commits |
| Commit identities on open PR branches | **A personal email appears on 15 of 31 open PR head branches.** Public already. Needs the owner |
| `LICENSE` | Holder is "The Another Authors". No personal name |

## Fixed in this round

- `FEDERATION.md`, README and site: hub command uses `--auth`, with the reason.
- `REMOTE.md`: the `Host` check applies to `/mcp`; never tunnel a server without `--auth`.
- `FEDERATION.md`: registrations carry absolute project paths to every paired node; hub-side `peers` shows `never`.
- `RESOURCES.md`: removed a pointer to an `AGENTS.md` snippet that doesn't exist.
- README: `PR #17` (closed) now points to its current replacement, `PR #50`; added `another sweep`, `project` and `agent show` to the CLI table.
- `PLAN.md`: operator handle removed.

## Polish round

Same day, on `launch/polish`. Nothing was deployed or posted.

- Install page: a copy-paste checksum recipe for per-archive `.sha256` files and `SHA256SUMS`. `site/check_checksum.py` runs the published snippet on every site build against local fixture archives, under each available shell and hasher: a match, tampering, an unlisted archive, a file naming another archive, a malformed line, wrong digests in a leading-space or tab layout, conflicting lines, and no checksum file. Only a match unpacks. The snippet compares digests itself and doesn't use `-c`: this Mac's `/sbin/sha256sum -c` exits 0 when it finds no line it can read, which let two earlier versions unpack unverified archives. The test caught the first; cross-family review caught the second. It has not run against a real release, because none exists.
- Proofread the site and README against `another --help` from this branch's `crates/`. Documented subcommands and flags all exist, and the common command forms ran with the documented exit status on a temporary node. `serve`, `reap --confirm`, `pair`, `sync` and `borrow` were checked against help only here; the runs above cover them.
- All 10 public pages have a unique description and the right canonical. The 404 page is deliberately `noindex` with no canonical.
- External links: the crates.io link in `SPEC.md` is fine (`rmcp` returns 200 from the crates.io API). The Cursor canvas link still returns 403. Placeholder tunnel hostnames are examples.
- Production: the five older pages return 200. `/guides/` and all four guides still return 404, because they aren't deployed.
- `x-launch.md`: a personal account handle removed.
- [COPY-REFRESH.md](COPY-REFRESH.md): refreshed channel copy, drafts only.

## Needs the owner

1. **What ships.** The documented install gives `main`, milestones 1–6, without the PR stack. Decide the stack's merge before any announcement. The docs fixes here reach readers only after merge and deploy, because the site links to `main`.
2. **Hardening before federation is promoted.** `main` still has the `/activity` behaviour until PR #53 lands. Consider making the code safe, not only the docs.
3. **First release.** The tag, and a publish path that has never run. Consider a tag-versus-Cargo check, and an Intel Mac check or a stated limit for that archive.
4. **Checksum format.** The stack publishes per-archive files and `SHA256SUMS`; `main` publishes per-archive files only. The install page recipe now handles both, so this no longer blocks the docs. Decide whether to close issue 28 once the recipe reaches `main`.
5. **`LICENSE` detection.** GitHub reports `NOASSERTION` / "Other". The text is the canonical Apache-2.0 with a 16-line notice added above it, which is the likely cause. Moving that notice to `NOTICE` should be checked against GitHub's detection. It is left alone because the copyright line is an open owner decision. Directories may read the detected license.
6. **Email in branch history.** Rewrite, squash on merge, or accept. New commits from a checkout with the default identity will add more.
7. **Release-notes reconciliation.** `RELEASE-NOTES-v0.1.0-DRAFT.md` now points to PR #51 instead of closed PR #18. Reconciling the two documents is still reserved for #51's owner.
8. **Carried from [SEO4.md](SEO4.md), unchanged:** deploying the guides, the `www` redirect and the crawler policy.
9. **Human acceptance.** A real handoff between two clients, and installs on Linux and an Intel Mac.
