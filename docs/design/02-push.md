# Design note 02: Push (event stream, long-poll, webhooks)

Status: implemented in phase 2. Builds on [01-lanes](01-lanes.md).

## Problem

The manager bot learns what happened by polling `activity_feed` or files. It needs to hear about lane status changes, needs-decision moments, requests, hold conflicts and expiring leases as they happen. It must hear only about the projects its token covers.

## 1. Event visibility follows token scope (supersedes D16)

Until now, remote tokens could read the node-wide activity feed (D16). Push makes that a standing subscription, so **every** remote read of events becomes scope-filtered: `activity_feed`, `activity_summarize`, `GET /activity`, the new stream and poll endpoints, and webhooks.

An event's projects are derived from its payload and the tables it points at:

| Event | Project comes from |
|---|---|
| `lane.*`, `project.ensured`, `hold.acquired/updated/conflict`, `resource.registered`, `resource.lease_expiring` | the payload's `project_id` |
| `hold.renewed/released/expired` | the held hold rows |
| `resource.*` | the resource rows (via `resource_id` or `lease_ids`) |
| `message.sent`, `delivery.*` | the projects of the sender and recipients |
| `request.*` | the request's message, as for `message.sent` |
| `session.*`, `agent.registered` | the agent's project |
| `token.*`, `peer.*`, `node.*` | none: never visible to scoped callers |

An event is visible to a scope if any of its projects matches the globs, **or** it concerns the token's own agent: the agent is the actor, or the payload's `agent_id` is the token's agent. Anything that can't be attributed is hidden. Local callers (CLI, stdio, loopback without `--auth`) see everything, as before. The activity summary's agent, request and hold counts are filtered the same way.

## 2. Stream and long-poll (pull, without polling files)

Both endpoints sit behind `--auth` when it is on, are filtered by scope, and are audited once per request.

- `GET /events/stream?since=<cursor|duration>&kinds=<categories>` is **Server-Sent Events**.
  - It replays everything after `since`, then sends new events as they are written: the server checks the log every second, so writes from other processes (CLI, other agents) are seen too.
  - Each SSE message has `id: <event_id>`, `event: <kind>`, and `data: <ActivityItem JSON>`.
  - Browsers and SSE clients resume with `Last-Event-ID`.
  - A keep-alive comment is sent every 15 seconds.
- `GET /events/poll?since=&kinds=&timeout=30` is **long-poll JSON**. It returns `{items, next_since}` as soon as at least one event is available, or when `timeout` (≤ 60 s) runs out.
- MCP `events_poll` does the same long-poll for MCP-only clients, capped at 30 s.

Items are `ActivityItem`s: one-line summaries plus ids. Message bodies are never included (D1).

## 3. Outbound webhooks

**Subscriptions.** `another webhook add --url URL --events lane_status,needs_decision --project GLOB...` creates a subscription and prints a signing **secret** once. `list`, `remove`, `test`, `dead` and `retry` complete the set. The MCP tools are `webhook_subscribe`, `webhook_list` and `webhook_remove`.

**Filters** (named groups of event kinds):

| Filter | Fires on |
|---|---|
| `lane_status` | `lane.created`, `lane.status_changed`, `lane.closed` |
| `lane_report` | `lane.reported` |
| `needs_decision` | a lane moving to `needs_decision` (status change, or a report with that status); `decision.created` (phase 3) |
| `request_created` | `request.created`, `decision.created` |
| `request_answered` | `request.completed`, `request.cancelled`, `decision.answered`, `decision.expired` |
| `hold_conflict` | `hold.conflict` (new event when an acquire is refused) |
| `lease_expiring` | `resource.lease_expiring` (new event, emitted once when a lease has under 5 minutes left; renewing re-arms it) |
| `all` | everything the scope can see, except mail and delivery events |

**Scope.**
- Every webhook has **project globs**.
- From the CLI they are required (`--project`, or the explicit `--all-projects`).
- Through remote MCP they default to the token's globs and can only be narrowed to a subset of them, compared as strings. Remote callers can only list or remove webhooks their own token created.
- Delivery uses the same visibility rule as streams, so a webhook can never carry an event from a project outside its globs.

**Delivery.**
- `another serve` (HTTP or stdio) runs a delivery worker every 2 seconds:
  - It turns new events into `webhook_deliveries` rows; a cursor per webhook means each event is considered once.
  - It POSTs due deliveries with `ureq`.
- **Body:** `{ "delivery_id", "webhook_id", "filter", "event": ActivityItem, "data": <event payload> }`. `data` is included because these event kinds carry no message bodies; mail events can't match any filter.
- **Headers:**
  - `X-Another-Event: <kind>`
  - `X-Another-Delivery: <id>`
  - `X-Another-Timestamp: <unix ms>`
  - `X-Another-Signature: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`
- **Retries:** non-2xx responses and network errors back off after 10 s, 30 s, 2 min, 10 min, 1 h and 6 h. After 7 attempts the delivery is **dead**: `another webhook dead` lists it with its last error, and `another webhook retry <id>` re-queues it.
- **URLs** must be `https://`, except loopback (`http://127.0.0.1`, `localhost`, `[::1]`) for local relays and tests.
- **Secrets** are stored in the node database (0700 directory), because they are needed for signing. `list` never shows them.

## Migration

Migration 7, additive:
- the `webhooks` and `webhook_deliveries` tables;
- a `resource_leases.expiry_warned_at` column (nullable, defaults to NULL).

## Out of scope

- Push to other nodes (federation already syncs logs).
- Websocket transport.
- Per-webhook rate limits; batching. Deliveries are sent one per event.
