# Changelog

All notable changes to Another are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - Unreleased

The first release. It contains the local core built in milestones 1–6 and the coordination series (Phases 1–7: lanes, push, decisions, lane holds, spend, summary, remote agents), plus the opt-in background service, `another doctor` and verified release installs. The date is set when `v0.1.0` is tagged. Release notes: [docs/release-notes-v0.1.0.md](docs/release-notes-v0.1.0.md).

### Fixed
- Cancelling an overdue decision commits its timeout result, request closure and automatic mail before returning a conflict, even without a running server or when the deadline passes as cancellation begins.

### Added
- **Local core** (milestones 1–6):
  - One SQLite store per node (`~/.another`, or `$ANOTHER_HOME`; `another init`), an append-only event log, and agents and projects (`another agent`, `another project`).
  - Mail with threads, read and ack receipts (`another send|reply|inbox|ack|threads|thread`), and requests with owners (`another requests`, `another request`).
  - Expiring path holds (`another hold`, `another holds`, `another heartbeat`, `another sweep`).
  - A resource registry with leases (`another resources|resource|borrow|release`), an orphan scanner (`another orphans`) and human-confirmed reaping (`another reap … --confirm`).
  - An activity feed (`another activity`, `another events`) and hook packs for Claude Code, Codex, Grok CLI and Cursor that inject a short digest at session start (`another hook install`).
  - MCP over stdio (`another serve --stdio`) or loopback streamable HTTP (`another serve`).
  - Authenticated remote peers: bearer tokens with project globs (`another token`), an audit log of remote calls (`another audit`) and `GET /activity`.
  - Federation: signed event logs, hub-and-spoke sync and cross-node mail (`another node`, `another pair`, `another peers`, `another sync`).
  - Wake adapters for tmux, cmux and Herdr (`another wake`).
- **Lanes** ([design](docs/design/01-lanes.md), [guide](docs/LANES.md)):
  - Named units of work with a charter, lead agent, project, status (`planned`, `working`, `blocked`, `needs_decision`, `done`, `abandoned`), linked threads, requests, holds and messages, and structured reports (summary, progress, links, artifacts) plus a DONE report with outcome and PR link.
  - `another lane create|list|show|update|report|done|close|export` and MCP tools `lane_create`, `lane_list`, `lane_show`, `lane_update`, `lane_report`, `lane_close`.
  - Reports are mailed to the lane's counterpart in the lane thread.
  - `lane export` writes the old `.lanes`-style STATUS and DONE markdown.
  - Opt-in Stop hook (`another hook install --lane-reports`) for automatic reports from Claude Code, Codex, Grok and Cursor.
  - Remote callers only see and change lanes in their token's project globs. Lanes outside them look like unknown lanes (`not_found`), and their events are left out of the remote activity feed.
  - Open lane names are unique per project (a closed lane's name can be reused); address lanes as `name` or `name@project`.
  - Lane events carry ids, names, project and status only (no charter, report text, progress or links). Links must belong to the lane's project.
  - The Stop hook redacts likely secrets (`ano_`, `sk-`, `ghp_`, long hex/base64, …) from the agent's last message (Claude Code and Codex; Grok and Cursor send none) before storing it. Summaries are cut to 300 characters, at most one report per 15 minutes.
- **Push** ([design](docs/design/02-push.md), [guide](docs/PUSH.md)):
  - `GET /events/stream` (Server-Sent Events, resumable with `Last-Event-ID`), `GET /events/poll` (long-poll JSON) and the MCP tool `events_poll`.
  - Outbound webhooks with HMAC-SHA256 signatures, retries with backoff and a dead-letter list.
    - Filters: `lane_status`, `lane_report`, `needs_decision`, `request_created`, `request_answered`, `hold_conflict`, `lease_expiring`, `all` (every event except mail and delivery receipts and token, peer, node and summary events). Phases 5 and 6 add `budget` and `daily_summary`.
    - CLI: `another webhook add|list|remove|test|dead|deliveries|retry`.
    - MCP tools: `webhook_subscribe`, `webhook_list`, `webhook_remove`.
    - `another serve` delivers them.
  - New events: `hold.conflict`, emitted when an acquire is refused, and `resource.lease_expiring`, emitted once per lease term within 5 minutes of expiry (or half the TTL); renewing re-arms it. Both, and `lane.hold_conflict`, are node-local and are not federated.
  - A webhook is disabled once the token that created it is revoked or expires. Webhooks a token created follow its own-agent rule for project-less events, like the stream.
- **Human decisions** ([design](docs/design/03-decisions.md), [guide](docs/HUMAN-DECISIONS.md)):
  - Structured questions from agents to a person: 2–6 options, optional free text, context links, a deadline and what happens at it (`keep_open`, `choose_default`, `cancel`), and an optional lane, which shows `needs_decision` until answered.
  - The person (default: the `decider` node setting, `another config decider NAME`, else `owner`) is an agent with the new program `human`.
  - Only a manager bot or a person can relay an answer, and never the asking agent. A short note may accompany a chosen option.
  - Timeout and overdue mail comes from the reserved system agent `another`.
  - `another config [KEY [VALUE]]` manages node settings.
  - Answers are mailed to the asking agent in the same thread, and wake it if it has a wake target. Who answered and who relayed it are both recorded.
  - Surfaces:
    - MCP: `decision_ask`, `decision_list`, `decision_show`, `decision_answer`, `decision_cancel`.
    - HTTP: `GET /decisions`, `GET /decisions/{id}`, `POST /decisions/{id}/answer`.
    - CLI: `another decision ask|list|show|answer|cancel`.
  - Events: `decision.created|answered|expired|overdue|cancelled`.
  - Requests gain a `kind` field (`task` or `decision`).
- **Lane-aware holds** ([design](docs/design/04-lane-holds.md)):
  - Holds carry a lane (explicit `lane`, else `$ANOTHER_LANE`, else the single open lane the agent leads in that project), looked up in the hold's project only. Overlaps inside one lane are warnings, except two exclusive holds, which still block.
  - When the requesting hold has a lane, a cross-lane conflict also mails the holding lane's lead (from the system agent, at most once per lane pair every 10 minutes).
  - Under `codex exec`, Codex skips hooks that haven't been trusted under `/hooks`; HOOKS.md and the installer say so (hold enforcement there isn't guaranteed without trust).
  - The pre-edit hook compares symlink-resolved paths, and warns (but still holds) when `$ANOTHER_LANE` is closed, unknown or in another project.
  - Cross-lane conflicts emit `lane.hold_conflict`, which the `hold_conflict` webhook filter delivers.
  - `another holds --by-lane`, MCP `holds_by_lane`, and live holds in `lane show`.
  - Opt-in pre-edit hook for Claude Code and Codex: `another hook install --pre-edit`. It holds files before each edit and denies the edit when another lane or agent holds them.
- **Spend ledger** ([design](docs/design/05-spend.md), [guide](docs/SPEND.md)):
  - Accounts: `gpu_pod`, `api_credits`, `plan_reset`, `server` and `domain`, each with a unit, budget, and a `monthly` period with a reset day (1–28), or `none` (lifetime). `another spend account add|list|update|archive`.
  - Spend entries (`another spend record`); hourly leases with an owner and a cost per hour, renewed by heartbeat (`another spend borrow|release|leases`).
  - Approval gate: `requires_approval` makes borrowing ask a decision first, of the account's approver or the node's decider. The borrower can't relay its own approval. Withdrawing a pending lease withdraws its decision (`spend.lease_withdrawn`).
  - Ending a session closes the agent's spend leases.
  - `another spend link ACCOUNT RESOURCE`: borrowing a linked resource goes through the account's approval gate and accrues cost while the resource lease lives.
  - Scoped tokens see the remaining budget of shared accounts (read-only), but only their own projects' spend.
  - `another spend`: a report by period, lane and agent with a phone-sized burn-down (`--period current|previous|YYYY-MM`, `--account`, `--lane`, `--agent`). Also MCP `spend_report` and `GET /spend`.
  - MCP tools `spend_record`, `spend_borrow`, `spend_release`, `spend_leases`, `spend_accounts`, and `spend_account_upsert` (local callers only).
  - `spend.budget_threshold` fires at 80% and 100% of a budget.
  - `spend.*` events, a new `spend` activity category and a `budget` webhook filter.
  - No real accounts are built in; the examples live in [docs/SPEND.md](docs/SPEND.md).
- **Daily summary** ([design](docs/design/06-summary.md)):
  - `another summary [--since 24h]`, MCP `daily_summary`, and `GET /summary` (JSON, or markdown with `format=markdown`).
  - Contents: what needs a person, lanes moved, stuck and closed, decisions, requests, holds and conflicts, spend, active agents, and active people (listed separately). Scoped for remote callers, including an event-id `since`. `truncated` is set when any section hit its cap (20,000 events, or 500 open decisions).
  - Opt-in daily rollup: `another summary schedule --daily HH:MM --to webhook|mail:NAME`, sent by `another serve` (HTTP or `--stdio`) from the `another` system agent; `--off` stops it. It covers the time since the last rollup, at most 48 hours. A new `daily_summary` webhook filter (local all-project webhooks only).
- [Manager bot guide](docs/MANAGER-BOT.md): token, webhooks to subscribe, and which MCP tools to call.
- **Remote agents** ([design](docs/design/07-remote-agents.md), [guide](docs/REMOTE-AGENTS.md)):
  - Invites:
    - `another invite create|list|revoke`: expiring codes (24h by default, at most 30 days), single-use by default (`--uses` up to 100), that fix the agent's name, program, project globs and lane.
    - Redeem with `another join <hub> <code>` or `POST /join`. That registers the agent, issues a scoped token, and returns Claude Code / Codex MCP config and env vars.
    - Invited agents are ephemeral unless the invite is created with `--persistent`.
    - `--print-snippet --hub-url URL` prints a shell block for cloud sessions. It installs the inviting binary's version (a checksum-verified release archive, else `cargo install` of the same tag) unless `another` is already on `PATH`, then joins.
    - Invites never take over a registered agent's name; `POST /join` (unauthenticated, served only with `--auth`) is limited to 10 attempts a minute per client address.
  - Lifecycle:
    - Presence (`another agent remotes`).
    - Ephemeral agents leave when their session ends or when idle (`--idle-hours`, else the `remote_idle_hours` node setting, default 4): token revoked, holds and leases in the token's projects released, `lane.agent_left` emitted. Remote heartbeats and leave touch only the token's projects too.
    - When a lane's lead leaves, the longest-active member takes over; with nobody left, the lane is flagged leaderless (never blocked automatically).
    - `another agent kick` removes any agent at once; a local manager agent (`grok_bot`) can kick over MCP (`agent_kick`). Tokens are refused `agent_kick` for now.
  - Remote-agent HTTP routes (`/agent/heartbeat`, `/agent/leave`, `/lanes`, `/lanes/{lane}/report`) and a client CLI (`another remote …`).
  - Federation:
    - Lanes are mirrored read-only on other nodes; only the owner writes. Reports into a mirror are proposals, which the owner applies when they sync.
    - Decisions asked on another node can be answered here, and the answer is relayed only to that node. Free text travels as mail, never in events.
    - Synced holds keep their lane when the receiving node knows it (its own lane or a mirror).
- CI also runs on pull requests that target non-`main` branches, so stacked PRs are checked.
- **Background service** (opt-in).
  - `another service install|status|logs|stop|start|uninstall`. It is never installed unless you run `another service install`.
  - macOS: a launchd user agent (`one.toanother.another`, `RunAtLoad`, `KeepAlive`, logs in `$ANOTHER_HOME/logs/`). Linux: a systemd user unit (`another.service`, `Restart=on-failure`, journal logs).
  - `install` takes `serve`'s flags (`--bind`, `--auth`, `--allowed-host`, `--trusted-proxy-header`, `--allow-remote-bind`, `--federation`) and the global `--home`. Re-running it updates the service in place and replaces every flag, so repeat the ones you want to keep; it warns when the update drops `--auth`, an allowed host, the trusted proxy header or `--federation`, or changes the proxy header, bind address or home, and restores the previous service if the update fails. It refuses when another process already listens on the port.
  - `stop` leaves the service installed: it starts again at the next login (launchd) or the next time the user manager starts (systemd). `uninstall` removes it; if it can't stop the service, it fails and keeps the definition so you can retry.
- **`another doctor`**: checks the store (writable, schema), the local server (reachable, Another, same home), tokens (expired, expiring, matching no project), client hooks for Claude Code, Codex and Grok (and whether Codex has recorded trust for them), and port and resource conflicts. PASS / WARN / FAIL / SKIP with a fix per problem; machine-readable with the global `--json`; exit 1 on any FAIL. A hook config it can't read is a FAIL. Without an explicit `--server` or `ANOTHER_SERVER`, it checks the background service's address when the service uses this home, and reports a stopped service, a missing binary, a service on another home or an unreadable definition.
- **Release installs:** tagged releases also publish one `SHA256SUMS` and `install.sh`. One-line install: `curl -fsSL https://github.com/tamler/another/releases/latest/download/install.sh | sh`. It verifies the archive before installing (`ANOTHER_VERSION`, `ANOTHER_INSTALL_DIR` to pin or relocate).
- README quickstart rewritten as a 90-second demo script (install, start, two agents, a hold, a port claim, doctor).

### Changed
- Doctor rejects wrapper mentions outside executable hook commands and preserves recognition of installer paths containing spaces or apostrophes.
- Spend reports keep active leases in the count before cost accrues, including at monthly resets, while retaining their overlap with historical periods.
- **Remote event reads are now filtered by token scope** (D38, supersedes D16). `activity_feed`, `activity_summarize` and `GET /activity` used to show every project's events to any token. They now show only events from the token's projects (and project-less events concerning its own agent). Token, peer, node, invite and summary events are hidden from tokens. Local callers are unaffected.
- `activity_summarize` open, claimed and stale request counts exclude decision requests (`kind = 'decision'`).
- When reading forward, the feed's `next_since` now advances past events filtered out by `kinds` or scope. For a scoped caller whose last scanned event is hidden, the cursor is an opaque `@<seq>` value instead of an event id; pass it back unchanged.
- **Database schema:** migrations 6–11, all additive; existing databases upgrade in place on first open.
  - Migration 6 adds the `lanes`, `lane_links` and `lane_reports` tables.
  - Migration 7 adds the `webhooks` and `webhook_deliveries` tables and a nullable `resource_leases.expiry_warned_at` column.
  - Migration 8 adds the `decisions` and `node_settings` tables and `requests.kind` (default `task`).
  - Migration 9 adds `holds.lane_id` (nullable).
  - Migration 10 adds the `spend_accounts`, `spend_entries` and `spend_leases` tables, and a nullable `resources.spend_account_id`.
  - Migration 11 adds the `agent_invites`, `remote_agents` and `remote_decisions` tables, and a nullable `lanes.leaderless_at`.

[Unreleased]: https://github.com/tamler/another/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/tamler/another/releases/tag/v0.1.0
