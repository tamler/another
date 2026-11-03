# Changelog

All notable changes to Another. Unreleased work is listed under the next version.

## Unreleased

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
  - New events: `hold.conflict`, emitted when an acquire is refused, and `resource.lease_expiring`, emitted once when a lease nears its end.
- **Human decisions** ([design](docs/design/03-decisions.md), [guide](docs/HUMAN-DECISIONS.md)):
  - Structured questions from agents to a person: 2–6 options, optional free text, context links, a deadline and what happens at it (`keep_open`, `choose_default`, `cancel`), and an optional lane, which shows `needs_decision` until answered.
  - The person (default `$ANOTHER_DECIDER`, else `jacob`) is an agent with the new program `human`.
  - Answers are mailed to the asking agent in the same thread, and wake it if it has a wake target. Who answered and who relayed it are both recorded.
  - Surfaces:
    - MCP: `decision_ask`, `decision_list`, `decision_show`, `decision_answer`, `decision_cancel`.
    - HTTP: `GET /decisions`, `GET /decisions/{id}`, `POST /decisions/{id}/answer`.
    - CLI: `another decision ask|list|show|answer|cancel`.
  - Events: `decision.created|answered|expired|overdue|cancelled`.
  - Requests gain a `kind` field (`task` or `decision`).
- CI also runs on pull requests that target non-`main` branches, so stacked PRs are checked.

### Changed
- **Remote event reads are now filtered by token scope** (D38, supersedes D16). `activity_feed`, `activity_summarize` and `GET /activity` used to show every project's events to any token. They now show only events from the token's projects (and project-less events concerning its own agent). Token, peer and node events are hidden from tokens. Local callers are unaffected.
- When reading forward, the feed's `next_since` now advances past events filtered out by `kinds` or scope.

### Database
- Migration 6 adds the `lanes`, `lane_links` and `lane_reports` tables. It is additive; existing databases upgrade in place.
- Migration 8 adds the `decisions` table and `requests.kind` (default `task`). It is additive.
- Migration 7 adds the `webhooks` and `webhook_deliveries` tables and a nullable `resource_leases.expiry_warned_at` column. It is additive.
