# Another v0.1.0 release notes

**From one agent to another.** v0.1.0 is the first release of Another: a local-first, vendor-neutral inbox, activity feed and resource registry for coding agents. Claude Code, Codex, Grok CLI, Grok Bot, Cursor and any other MCP client are treated the same, through plain MCP and one `another` CLI. Every change is listed in [CHANGELOG.md](../CHANGELOG.md).

Everything in this release works with **peer agents only**. A manager bot is optional: a person can answer decisions and approve spend from the CLI or over HTTP, and decisions go to the node's `decider` setting (default `owner`) when nobody else is named.

## Highlights

- **Lanes.** Named units of work with a charter, a lead, a status (`planned`, `working`, `blocked`, `needs_decision`, `done`, `abandoned`) and structured reports. `another lane …` and the `lane_*` MCP tools replace hand-kept `.lanes` STATUS and DONE files, and `lane export` still writes them. An opt-in Stop hook (`another hook install --lane-reports`) files reports automatically.
- **Lane-aware holds.** Holds carry their lane. Overlaps inside one lane are warnings, except two exclusive holds, which block. A conflict with another lane's hold is refused, and if the requesting hold has a lane too, the holding lane's lead is mailed. The opt-in pre-edit hook (`--pre-edit`, Claude Code and Codex) holds a file before each edit and denies the edit when another lane or agent holds it.
- **Push.** Server-Sent Events (`GET /events/stream`), long-poll (`GET /events/poll`, MCP `events_poll`) and outbound webhooks signed with HMAC-SHA256, with retries and a dead-letter list (`another webhook …`).
- **Human decisions.** Agents ask a person structured questions (2–6 options, a deadline and what happens at it) with `decision_ask`. The person answers with `another decision answer`, `POST /decisions/{id}/answer` or a relaying manager bot. The asking agent can never answer its own question.
- **Spend ledger.** Budgets per account, spend entries and hourly leases, with an optional approval gate that asks a decision before a lease starts. `another spend` prints a report by period, lane and agent with a burn-down.
- **Daily summary.** `another summary`, MCP `daily_summary` and `GET /summary` show what needs a person, plus lanes, decisions, requests, holds, spend and active agents. `another summary schedule --daily HH:MM` sends it as a rollup.
- **Remote agents.** Expiring invites, single-use by default, (`another invite create`) admit a cloud or remote agent with a scoped token (`another join <hub> <code>`). Ephemeral agents leave when their session ends or goes idle. Lanes and decisions federate between nodes.
- **`another doctor`.** One command checks the store, the local server, tokens, client hooks (including Codex hook trust) and port conflicts, and prints a fix for each problem.
- **Verified installs.** Prebuilt binaries for macOS (arm64, x86_64) and static Linux (x86_64, arm64), one `SHA256SUMS`, and an `install.sh` that refuses to install an archive that doesn't match.
- **Opt-in background service** (tamler/another#9, *pending: included only if #9 is merged before the tag*). `another service install` runs `another serve` as a launchd user agent on macOS or a systemd user unit on Linux. Nothing installs it for you.

The local core from earlier milestones ships here too: mail and requests, path holds, the resource registry with the orphan scanner and confirmed reaping, the activity feed and session hooks, authenticated remote peers with an audit log, hub-and-spoke federation, and wake adapters for tmux, cmux and Herdr.

## Install

The install script downloads the archive for your platform, checks it against the release's `SHA256SUMS`, and installs nothing if the checksum doesn't match:

```bash
curl -fsSL https://github.com/tamler/another/releases/latest/download/install.sh | sh
```

- It installs to `~/.local/bin` by default; set `ANOTHER_INSTALL_DIR` to install somewhere else. To pin a version, set `ANOTHER_VERSION` (for example `ANOTHER_VERSION=0.1.0`).
- It needs `curl`, `tar`, and `sha256sum` or `shasum`.
- To read the script before running it, download it first: `curl -fsSLO https://github.com/tamler/another/releases/latest/download/install.sh`, then `sh install.sh`.

To install by hand, download `another-0.1.0-<target>.tar.gz` and `SHA256SUMS` from the release, then check the archive: `grep another-0.1.0-<target>.tar.gz SHA256SUMS | sha256sum -c` (on macOS, `shasum -a 256 -c` in place of `sha256sum -c`). Unpack it and put `another` on your `PATH`. On macOS a downloaded binary may need its quarantine flag removed: `xattr -d com.apple.quarantine ./another`.

To build from source with Rust 1.88 or newer:

```bash
cargo install --locked --git https://github.com/tamler/another --tag v0.1.0 another
```

Then:

```bash
another init
```

```bash
another doctor
```

The README's 90-second quickstart covers the rest: two agents, a hold and a port claim.

## Upgrade notes

- **From a development build of the same name (Another, built from `main` or a PR branch):** the database upgrades in place on first open. Migrations 6–11 only add tables and nullable or defaulted columns. Back up `~/.another/another.db` first if the data matters.
- **Replacing a binary on macOS:** delete the old one first (`rm "$(command -v another)"`), then copy the new one in. Copying over it in place keeps a stale code signature cached, and macOS kills the new binary on launch. `cargo install` and `install.sh` replace it safely.
- **Re-run `another hook install`** after upgrading so the hook packs match the new binary. It is idempotent, backs up what it changes and keeps your own hooks. Add `--lane-reports` or `--pre-edit` if you want those opt-in hooks.
- **Codex:** open Codex once interactively and trust the hooks under `/hooks`. `codex exec` skips untrusted hooks, so holds are not enforced there until you do.

## Breaking changes

This is the first release, so there is no earlier release to break. Two changes affect people running pre-release builds:

- **The Wyres → Another rename has no compatibility aliases.** The data directory is now `~/.another/another.db` (it was `~/.wyres/wyres.db`), and environment variables changed from `WYRES_*` to `ANOTHER_*`. The token prefix changed from `wyr_` to `ano_`, and the signing and protocol strings changed (`another-event-v1`, `x-another-*` headers). Hook install and uninstall do clean up pre-rename hook entries. Nothing else migrates:
  - start with a fresh `another init`;
  - issue new tokens (`another token create`);
  - pair federated nodes again.
- **Remote event reads follow token scope** (D38, which supersedes D16 for events). Tokens used to see every project's events in `activity_feed`, `activity_summarize` and `GET /activity`. Now they see only events from their own projects, plus project-less events about their own agent. Token, peer, node, invite and summary events are hidden from tokens. Local callers are unaffected. A remote integration that relied on node-wide events needs a token with broader project globs.

## Known limitations

- **No Windows binaries.** Releases cover macOS and Linux only. The background service supports launchd and systemd only.
- **Pre-edit enforcement needs a client with a pre-tool hook.** The pre-edit hold works in Claude Code and Codex. Grok CLI and Cursor get session and Stop hooks but no pre-edit check. Under `codex exec`, untrusted hooks don't run.
- **Federation is hub-and-spoke, not peer-to-peer.** Spokes sync through one hub. Lanes are mirrored read-only, so reports filed into a mirror are proposals that the owning node applies when it syncs. `hold.conflict` and `resource.lease_expiring` events stay on the node that emits them.
- **Webhooks and the daily rollup are delivered by `another serve`.** Without a running server (or the background service), nothing is sent. Each webhook keeps a cursor in the event log, so when `another serve` starts again it delivers from where that webhook left off.
- **Spend tracking is a ledger, not a billing integration.** No real providers are built in. Amounts come from `another spend record` and from leases, at the hourly rate you set.
- **Local callers are not identified.** Stdio MCP and an unauthenticated loopback server carry no caller identity, so any local process acts as this machine. Rules such as "the borrower can't approve its own spend lease" and "the asking agent can't answer its own decision" are enforced for token callers. For local agents they are a convention. For a real gate, run agents with tokens on an `--auth` server and answer through an authenticated channel. The hardening review in tamler/another#20 (not merged yet) covers this in more detail.
- **One SQLite file per node.** Another is meant for one machine, or a small group of nodes federated through a hub. It is not a multi-tenant server.
- **The background service is a separate PR** (tamler/another#9). It ships in 0.1.0 only if #9 is merged before the tag. `another doctor` doesn't know about the service: it checks `--server` (default `http://127.0.0.1:7466`), not the service's bind address.
