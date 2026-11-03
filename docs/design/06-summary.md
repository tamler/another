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
| `decisions` | Open (with age and deadline); answered and expired in the window |
| `requests` | Open, claimed and stale counts; completed in the window |
| `holds` | Live holds; hold conflicts in the window (cross-lane ones counted separately) |
| `spend` | Per account: spent in the window, plus the period's used % and status from `spend_report` |
| `agents` | Agents active in the window |
| `counts` | Events per category |
| `markdown` | The phone version: a heading, the attention list, then one line per section, about 15 lines at most |

`since` is a duration (default `24h`), an RFC 3339 time, or an event-id cursor, as elsewhere.

## Surfaces

- **CLI:** `another summary [--since 24h] [--json]`. It prints the markdown by default.
- **MCP:** `daily_summary {since?}` returns the JSON with `markdown` inside it.
- **HTTP:** `GET /summary?since=24h` returns JSON, or `text/markdown` with `&format=markdown` or `Accept: text/markdown`. It sits behind `--auth`, is scoped and is audited.
- `activity_summarize` is unchanged: it is the hour-level digest that the hooks use.

## Scope

Remote callers get each section exactly as the underlying scoped read returns it:
- lanes, decisions, holds and spend from their own scope rules (notes 01–05);
- events from the scoped feed (note 02);
- webhooks: only the token's own.

So the summary can't show what any one of those reads would hide.

## Migration

None.

## Out of scope

- Scheduling. The manager or a cron job calls the endpoint. A scheduled summary that posts itself would need the push from note 02, plus a place to send it, and that belongs to the manager bot.
