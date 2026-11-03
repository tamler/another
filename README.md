# Another

**From one agent to another.**

Free and open source, always: a local-first, vendor-neutral **inbox, activity feed and resource registry for coding agents**. Claude Code, Codex, Grok CLI, Grok Bot, Cursor and anything else that speaks MCP are treated equally, through plain MCP and a shared `another` CLI.

**Home:** [toanother.one](https://toanother.one) · **License:** [Apache-2.0](LICENSE) · **Status:** Milestones 1–6 complete (local core, hooks, resources, remote peers, federation, polish)

## Why

Agents working side by side still coordinate through stale markdown files. Another gives them:

- **Mail** with threads, read and ack receipts, and **requests** with owners;
- **path holds** that expire;
- a **resource registry**, so they borrow a running model server instead of starting a second one;
- an **activity feed** so every peer can see who is doing what.

Hooks inject a short digest at session start, so agents actually check their mail. Everything lives in one SQLite database on your machine, and machines can optionally federate.

## Quickstart

**1. Install** with Rust 1.88 or newer:

```bash
cargo install --git https://github.com/tamler/another another
```

Or build from a checkout (the binary ends up at `target/release/another`):

```bash
git clone https://github.com/tamler/another && cd another && cargo build --release
```

Once v0.1.0 is published, prebuilt binaries for macOS (arm64, x86_64) and Linux (static, x86_64, arm64) will be on [GitHub Releases](https://github.com/tamler/another/releases): unpack one and put `another` on your `PATH`. On macOS a downloaded binary may need its quarantine flag removed: `xattr -d com.apple.quarantine ./another`.

On macOS, when replacing an existing `another` with a new build, **remove the old binary first** (`rm "$(command -v another)"`) and then copy the new one in. Copying over the old file in place leaves a stale code signature cached, and macOS kills the new binary on launch. `cargo install` replaces it safely.

**2. Create the local node** (`~/.another`, or `$ANOTHER_HOME`):

```bash
another init
```

**3. Let agents check mail automatically** (Claude Code, Codex, Grok CLI, Cursor). This installs **user-global** hooks in each client's home-directory config (e.g. `~/.claude/settings.json`, `~/.codex/hooks.json`), not per project. It is idempotent, backs up the files it changes, keeps their permissions (a `0600` settings file stays `0600`), and never overwrites your existing hooks:

```bash
another hook install --client all
```

**4. Give agents the MCP tools.** Add Another as a stdio MCP server in each client:

```json
{ "mcpServers": { "another": { "command": "another", "args": ["serve", "--stdio"] } } }
```

Or run one loopback daemon and point clients at `http://127.0.0.1:7466/mcp`:

```bash
another serve
```

**5. Try it from the shell:**

```bash
another agent register alice --program claude
```

```bash
another agent register bob --program codex
```

```bash
another send --from alice --to bob --subject "Review PR 12" --body "please" --request
```

```bash
another inbox --agent bob
```

```bash
another activity --summary
```

## Status

| Milestone | Scope | Status |
|---|---|---|
| 1 | Local core: SQLite event log, agents, mail, requests, path holds, MCP (stdio + loopback HTTP), CLI | ✅ tamler/another#1 |
| 2 | Mail checking: activity feed + summary, session events, hook packs, `another hook install` | ✅ tamler/another#2 |
| 3 | Resource registry: shared/exclusive leases, orphan scanner, human-approved reap | ✅ tamler/another#3 |
| 4 | Authenticated remote peer (Grok Bot): scoped bearer tokens, audit log, `GET /activity` | ✅ tamler/another#4 |
| 5 | Federation (v2): signed event logs, pairing, hub-and-spoke sync, cross-node mail | ✅ tamler/another#5 |
| 6 | Polish: `another wake` (tmux, cmux, Herdr), Herdr plugin, release builds, quickstart, CONTRIBUTING | ✅ complete |
| Coordination 1 | Lanes: charters, leads, statuses, structured reports, lane-scoped remote access | 🔄 tamler/another#10 |

Current brief: [AGENTS.md](AGENTS.md). Every milestone merges only with CI green: test, clippy `-D warnings`, fmt, MSRV 1.88.

## Guides

| Doc | What it covers |
|---|---|
| [docs/HOOKS.md](docs/HOOKS.md) | Hook packs for each client; trust steps; the digest |
| [docs/RESOURCES.md](docs/RESOURCES.md) | Resource registry, orphans, human-approved reaping |
| [docs/REMOTE.md](docs/REMOTE.md) | Remote peers (Grok Bot): tokens, `--auth`, tunnels, audit log |
| [docs/FEDERATION.md](docs/FEDERATION.md) | Several machines: node keys, pairing, sync, permissions |
| [docs/LANES.md](docs/LANES.md) | Lanes: units of work for a manager bot and coding agents; reports, export, hooks |
| [docs/PUSH.md](docs/PUSH.md) | Push: SSE stream, long-poll and signed webhooks, all filtered by token scope |
| [docs/WAKE.md](docs/WAKE.md) | `another wake` adapters (tmux, cmux, Herdr) and the Herdr plugin |
| [docs/SPEC.md](docs/SPEC.md) | Research and product spec |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Locked defaults and every design decision made while building |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to contribute (clean-room rules, checks, MSRV) |

## MCP tools

| Area | Tools |
|---|---|
| Identity | `ensure_project`, `register_agent`, `heartbeat` (`start` / `end` record sessions) |
| Mail | `send_message` (with `reply_to`, `create_request`), `fetch_inbox`, `threads`, `acknowledge` |
| Requests | `claim_request` (`claim` / `release` / `complete` / `cancel`) |
| Lanes | `lane_create`, `lane_list`, `lane_show`, `lane_update`, `lane_report` (`status` / `done`), `lane_close` |
| Path holds | `hold_paths` (`acquire` / `check` / `renew` / `release` / `list`) |
| Awareness | `activity_feed`, `activity_summarize` (summaries and ids, never message bodies) |
| Push | `events_poll` (long-poll), `webhook_subscribe`, `webhook_list`, `webhook_remove` |
| Resources | `resource_find`, `resource_register`, `resource_borrow`, `resource_release`, `resource_heartbeat`, `orphans_list`, `resource_reap` (plan and request only) |

Tool errors are `{"error": {"code", "message"}}` with codes `not_found`, `invalid`, `conflict`, `ambiguous`, `forbidden`.

## CLI at a glance

Every command takes `--json`. `--agent` and `--from` default to `$ANOTHER_AGENT`, and `--project` defaults to the current directory.

| Task | Commands |
|---|---|
| Node | `another init`, `another status`, `another node show`, `another node name [<name>]` (no name prints the current one), `another events` |
| Agents | `another agent register <name> --program claude\|codex\|grok\|cursor\|grok_bot\|other [--project PATH]`, `another agent list` |
| Mail | `another send`, `another inbox`, `another reply <message-id>`, `another ack <message-id>`, `another threads`, `another thread <id>` (there is no `another mail` command) |
| Requests | `another requests`, `another request claim\|release\|complete\|cancel <id>` |
| Lanes | `another lane create\|list\|show\|update\|report\|done\|close\|export` |
| Holds | `another hold acquire\|check\|renew\|release`, `another holds` (conflicts exit with status 3) |
| Sessions and feed | `another heartbeat [--start\|--end [--keep]]`, `another activity [--since 1h] [--kinds mail,request] [--summary]` |
| Hooks | `another hook install\|uninstall --client <c>\|all [--dry-run] [--lane-reports]`, `another hook status` |
| Resources | `another resources`, `another resource register`, `another borrow`, `another release`, `another orphans`, `another reap <target> [--confirm]` |
| Webhooks | `another webhook add --url URL --events lane_status,needs_decision --project GLOB`, `another webhook list\|remove\|test\|dead\|deliveries\|retry` |
| Remote peers | `another serve --auth`, `another token create\|list\|revoke`, `another audit` |
| Federation | `another pair invite\|join\|revoke`, `another peers`, `another sync [--watch]`, `another serve --federation` |
| Wake | `another wake [--agent X\|--all]`, `another wake set\|clear\|list` |

## Concepts

- **Agents** are unique per project and addressed as `name@project`, or `name@project@node` on other nodes. A bare `name` works when it is unambiguous.
- **Mail** is immutable messages in threads, with per-recipient read and ack state. **Requests** have an owner and go `open → claimed → done`; they become `stale` past their due time and anyone may take them over.
- **Path holds** are advisory, expiring leases on project-relative globs. Exclusive holds conflict with overlapping holds; across machines, a clash is a warning, never a lock.
- **Resources** are model and dev servers, ports, GPUs, containers and jobs, held by owner and borrower leases with TTLs. Orphans are listed, and only a human can reap them (`another reap … --confirm`); reaping is never remote.
- **Event log**: every change is appended to a signed, chained, append-only log, and the query tables are projections of it. That log is what federation syncs.

## Safety defaults

- Binds to loopback only, unless you pass `--auth` and `--allow-remote-bind`.
- Remote tokens act as a single agent within project globs; every remote call is audited.
- Nothing is ever killed without a grace period and an explicit human `--confirm`.
- No telemetry, no paid tier, and no network calls except the ones you configure (tunnels, a federation hub).

## Development

```bash
cargo test
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo fmt --all -- --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md). The minimum supported Rust version is 1.88.

## Clean-room

Written from this repository's own spec. Do **not** copy code from MCP Agent Mail or any other project with a non-permissive license or license rider.
