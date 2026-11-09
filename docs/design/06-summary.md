# Design note 06: Daily summary

Status: implemented in phase 6. Builds on notes 01–05.

## Problem

The person running the agents wants one message a day, or on demand, that answers four questions:
- What moved?
- What is stuck?
- What needs me?
- What did it cost?

The message has to read well on a phone. The manager bot wants the same information as JSON, to post or act on. Today that means stitching together `activity_summarize`, `lane_list`, `decision_list`, `spend_report` and the holds, each with its own scoping.

## Shape

`Store::daily_summary(since, scope) -> DailySummary` builds everything from the existing scoped reads, so it never shows more than those reads would:

| Section | Contents |
|---|---|
| `attention` | One line per item that needs a person: open decisions (oldest first, with their ids), blocked or `needs_decision` lanes, accounts over budget or on pace to exceed it, pending spend approvals, stale requests, dead webhook deliveries |
| `lanes` | Open lanes by status; lanes that changed status, reported or closed in the window, from lane events; closed lanes with their outcome and PR link |
| `decisions` | Open (with age and deadline): every visible open decision is counted (`open_total`), and the list keeps the oldest 500. Answered and expired in the window |
| `requests` | Open, claimed and stale counts; completed in the window |
| `holds` | Live holds; hold conflicts in the window (cross-lane ones counted separately) |
| `spend` | Per account: spent in the window, plus the period's used % and status from `spend_report` |
| `agents_active` | Agents active in the window (not the `another` system agent) |
| `people_active` | People (program `human`) active in the window, listed apart from agents |
| `truncated` | True when an event list hit the pager's cap (20,000 per section) or there are more than 500 open decisions; the markdown says so |
| `counts` | Events per category |
| `markdown` | The phone version: a heading, the attention list, then one line per section, about 15 lines at most |

`since` is a duration (default `24h`), an RFC 3339 time, or an event-id cursor, as elsewhere. An event id is resolved through the shared scoped cursor read (`activity::resolve_cursor`, which the feed and `activity_summarize` use too): an event the caller can't see gets the same `not_found` as an unknown id.

**Budgets:** an account over budget whose percentage the caller can't see (no budget figure, or a zero budget) is listed as "over budget" with no percentage.

## Surfaces

- **CLI:** `another summary [--since 24h] [--json]`. It prints the markdown by default.
- **MCP:** `daily_summary {since?}` returns the JSON with `markdown` inside it.
- **HTTP:** `GET /summary?since=24h` returns JSON, or `text/markdown` with `&format=markdown` or `Accept: text/markdown`. It sits behind `--auth`, is scoped and is audited.
- `activity_summarize` stays the hour-level digest that the hooks use. One change: its request counts now count task requests only (decisions are reported on their own), and the summary reuses them.

## Scope

Remote callers get each section exactly as the underlying scoped read returns it:
- lanes, decisions, holds and spend from their own scope rules (notes 01–05);
- events from the scoped feed (note 02);
- webhooks: only the token's own.

So the summary can't show what any one of those reads would hide.

## Scheduled rollup (opt-in)

`another summary schedule --daily HH:MM --to webhook|mail:NAME` (local only; `--off` turns it off, no flags shows it). Stored as the node settings `summary_daily` and `summary_to`.
- **Runs in the server:** `another serve` checks on every sweep. Once a day, after `HH:MM` local time, it sends the local (unscoped) summary covering the time since the last one (24 h the first time, at most 48 h).
- **Once, even with several servers:** the send rechecks the "sent today" marker inside its write transaction (writes are serialized), so several sweepers sharing one home (an HTTP server plus stdio MCP servers) send it once.
- **`mail:NAME`:** mail from the reserved system agent `another`. An unknown name is refused when the setting is written. If the send later fails for a reason that won't pass by itself (the name became ambiguous), the failure is logged once and retried the next day, or as soon as the schedule is saved again. Database and file errors retry on the next sweep.
- **`webhook`:** emits `summary.daily` (from `another`), delivered through the new `daily_summary` webhook filter. The event is node-local (never federated) and never visible to tokens. So only local webhooks without project globs receive it; token-created webhooks and `all` never do. Its payload is the summary itself: it is the delivery.
- Pulling with `another summary`, MCP `daily_summary` or `GET /summary` is unchanged.

## Migration

None (settings live in `node_settings`, from migration 8).
