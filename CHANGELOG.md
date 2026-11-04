# Changelog

All notable changes to Another. Unreleased work is listed under the next version.

## Unreleased

### Fixed
- Cancelling an overdue decision commits its timeout result, request closure and automatic mail before returning a conflict, even without a running server or when the deadline passes as cancellation begins.

### Added
- **Lanes** ([design](docs/design/01-lanes.md), [guide](docs/LANES.md)):
  - Named units of work with a charter, lead agent, project, status (`planned`, `working`, `blocked`, `needs_decision`, `done`, `abandoned`), linked threads, requests, holds and messages, and structured reports (summary, progress, links, artifacts) plus a DONE report with outcome and PR link.
  - `another lane create|list|show|update|report|done|close|export` and MCP tools `lane_create`, `lane_list`, `lane_show`, `lane_update`, `lane_report`, `lane_close`.
  - Reports are mailed to the lane's counterpart in the lane thread.
  - `lane export` writes the old `.lanes`-style STATUS and DONE markdown.
  - Opt-in Stop hook (`another hook install --lane-reports`) for automatic reports from Claude Code, Codex, Grok and Cursor.
  - Remote callers only see and change lanes in their token's project globs. Lanes outside them look like unknown lanes (`not_found`), and their events are left out of the remote activity feed.
  - Lane names are unique per project; address lanes as `name` or `name@project`.
  - Lane events carry ids, names, project and status only (no charter, report text, progress or links). Links must belong to the lane's project.
  - The Stop hook redacts likely secrets (`ano_`, `sk-`, `ghp_`, long hex/base64, …) from the agent's last message before storing it.
- **Push** ([design](docs/design/02-push.md), [guide](docs/PUSH.md)):
  - `GET /events/stream` (Server-Sent Events, resumable with `Last-Event-ID`), `GET /events/poll` (long-poll JSON) and the MCP tool `events_poll`.
  - Outbound webhooks with HMAC-SHA256 signatures, retries with backoff and a dead-letter list.
    - Filters: `lane_status`, `lane_report`, `needs_decision`, `request_created`, `request_answered`, `hold_conflict`, `lease_expiring`, `all`.
    - CLI: `another webhook add|list|remove|test|dead|deliveries|retry`.
    - MCP tools: `webhook_subscribe`, `webhook_list`, `webhook_remove`.
    - `another serve` delivers them.
  - New events: `hold.conflict`, emitted when an acquire is refused, and `resource.lease_expiring`, emitted once when a lease nears its end. Both are node-local and are not federated.
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
  - Holds carry a lane (explicit `lane`, else `$ANOTHER_LANE`, else the lane the agent leads), looked up in the hold's project only. Overlaps inside one lane are warnings, except two exclusive holds, which still block.
  - A cross-lane conflict also mails the holding lane's lead (from the system agent, debounced).
  - Under `codex exec`, Codex skips hooks that haven't been trusted under `/hooks`; HOOKS.md and the installer say so (hold enforcement there isn't guaranteed without trust).
  - The pre-edit hook compares symlink-resolved paths, and warns (but still holds) when `$ANOTHER_LANE` is closed or belongs to another project.
  - Cross-lane conflicts emit `lane.hold_conflict`, which the `hold_conflict` webhook filter delivers.
  - `another holds --by-lane`, MCP `holds_by_lane`, and live holds in `lane show`.
  - Opt-in pre-edit hook for Claude Code and Codex: `another hook install --pre-edit`. It holds files before each edit and denies the edit when another lane or agent holds them.
- **Spend ledger** ([design](docs/design/05-spend.md), [guide](docs/SPEND.md)):
  - Accounts: `gpu_pod`, `api_credits`, `plan_reset`, `server` and `domain`, each with a unit, budget, and monthly period with a reset day (or lifetime).
  - Spend entries; hourly leases with an owner and a cost per hour, renewed by heartbeat.
  - Approval gate: `requires_approval` makes borrowing ask a decision first, of the account's approver or the node's decider. The borrower can't relay its own approval. Withdrawing a pending lease withdraws its decision (`spend.lease_withdrawn`).
  - Ending a session closes the agent's spend leases.
  - `another spend link ACCOUNT RESOURCE`: borrowing a linked resource goes through the account's approval gate and accrues cost while the resource lease lives.
  - Scoped tokens see the remaining budget of shared accounts (read-only), but only their own projects' spend.
  - `another spend`: a report by period, lane and agent with a phone-sized burn-down. Also MCP `spend_report` and `GET /spend`.
  - `spend.*` events, a new `spend` activity category and a `budget` webhook filter.
  - No real accounts are built in; the examples live in [docs/SPEND.md](docs/SPEND.md).
- CI also runs on pull requests that target non-`main` branches, so stacked PRs are checked.

### Changed
- Spend reports keep active leases in the count before cost accrues, including at monthly resets, while retaining their overlap with historical periods.
- **Remote event reads are now filtered by token scope** (D38, supersedes D16). `activity_feed`, `activity_summarize` and `GET /activity` used to show every project's events to any token. They now show only events from the token's projects (and project-less events concerning its own agent). Token, peer and node events are hidden from tokens. Local callers are unaffected.
- When reading forward, the feed's `next_since` now advances past events filtered out by `kinds` or scope.

### Database
- Migration 6 adds the `lanes`, `lane_links` and `lane_reports` tables. It is additive; existing databases upgrade in place.
- Migration 10 adds the `spend_accounts`, `spend_entries` and `spend_leases` tables, and a nullable `resources.spend_account_id`. It is additive.
- Migration 9 adds `holds.lane_id` (nullable). It is additive.
- Migration 8 adds the `decisions` table and `requests.kind` (default `task`). It is additive.
- Migration 7 adds the `webhooks` and `webhook_deliveries` tables and a nullable `resource_leases.expiry_warned_at` column. It is additive.
