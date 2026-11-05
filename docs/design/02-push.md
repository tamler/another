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

An event with projects is visible to a scope only if one of its projects matches the globs. **Project scope wins over agent identity**: a token acting as `manager` does not see `manager`'s actions in a project outside its globs.

An event with no project (mail between agents without projects, their sessions) is visible only if it concerns the token's own agent, **matched by agent id, never by name**: the agent's id appears in the payload or among the message's parties, or the event's actor address resolves, on the event's own origin node, to the token's agent. Names are not unique across nodes: another node's project-less `manager` is a different agent. Local callers (CLI, stdio, loopback without `--auth`) see everything, as before. The activity summary's agent, request and hold counts are filtered the same way.

(An earlier draft of this note also showed events that concerned the token's agent in other projects. The phase 2 integration test caught that this leaks another project's lane to a manager token scoped to one project, so the rule above replaced it.)

Reading forward, `next_since` advances past events the scope can't see, so a cursor never re-scans them. The cursor is an event id, which reveals only that some event happened at that time.

## 2. Stream and long-poll (pull, without polling files)

Both endpoints sit behind `--auth` when it is on, are filtered by scope, and are audited once per request.

- `GET /events/stream?since=<cursor|duration>&kinds=<categories>` is **Server-Sent Events**.
  - It replays everything after `since`, then sends new events as they are written: the server checks the log every second, so writes from other processes (CLI, other agents) are seen too.
  - Each SSE message has `id: <event_id>`, `event: <kind>`, and `data: <ActivityItem JSON>`.
  - Browsers and SSE clients resume with `Last-Event-ID`.
  - A keep-alive comment is sent every 15 seconds.
- Without `since`, both endpoints start from now.
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
| `needs_decision` | a lane moving to `needs_decision` (status change, or a report with that status); `decision.created`, `decision.overdue` (phase 3) |
| `request_created` | `request.created`, `decision.created` |
| `request_answered` | `request.completed`, `request.cancelled`, `decision.answered`, `decision.expired` |
| `hold_conflict` | `hold.conflict` (new event when an acquire is refused; node-local, never federated) |
| `lease_expiring` | `resource.lease_expiring` (new event, emitted once by the sweeper when a lease has under 5 minutes left, or under half its TTL for short leases; renewing re-arms it; node-local, never federated) |
| `all` | everything the scope can see, except mail and delivery events |

**Scope.**
- Every webhook has **project globs**.
- From the CLI they are required (`--project`, or the explicit `--all-projects`).
- Through remote MCP they default to the token's globs and can only be narrowed to a subset of them, compared as strings. Remote callers can only list or remove webhooks their own token created.
- Delivery uses the same visibility rule as streams, so a webhook can never carry an event from a project outside its globs. A webhook created by a token also gets that token agent's **own-agent rule** for project-less events, exactly as its stream would (maintainer's decision). Webhooks created locally carry project events only, unless they use `--all-projects`.
- **A webhook lives no longer than its token.** Before every queueing pass, webhooks whose creating token was revoked or has expired are **disabled**: `disabled_at` and `disabled_reason` are set, they never queue again, their pending deliveries become dead (`webhook disabled: token revoked`), and `webhook test` refuses them. `list` shows the reason. Local webhooks have no token and are unaffected.

**Delivery.**
- `another serve` (HTTP or stdio) runs a delivery worker every 2 seconds:
  - It turns new events into `webhook_deliveries` rows; a cursor per webhook means each event is considered once.
  - It POSTs due deliveries with `ureq` (10 s timeout, no redirects).
  - A due delivery is claimed for a minute before it is sent, so two `serve` processes never send it twice.
  - When no webhooks exist it only reads, so the many stdio servers agents run take no write lock.
- **Body:** `{ "delivery_id", "webhook_id", "filter", "event": ActivityItem, "data": <allowlisted event fields> }`. `data` keeps ids, timestamps, numbers and a short list of named strings, and drops notes, subjects and reasons; mail events can't match any filter.
- **Headers:**
  - `X-Another-Event: <kind>`
  - `X-Another-Delivery: <id>`
  - `X-Another-Timestamp: <unix ms>`
  - `X-Another-Signature: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`
- **Retries:** non-2xx responses and network errors back off after 10 s, 30 s, 2 min, 10 min, 1 h and 6 h. After 7 attempts the delivery is **dead**: `another webhook dead` lists it with its last error, and `another webhook retry <id>` re-queues it.
- **URLs** must be `https://`, except loopback (`http://127.0.0.1`, `localhost`, `[::1]`) for local relays and tests.
- **Secrets** are stored in the node database (0700 directory), because they are needed for signing. `list` never shows them.

`another webhook test` sends a synthetic `webhook.test` delivery synchronously; it is not an event in the log.

## Federation

`hold.conflict` and `resource.lease_expiring` are **node-local signals**: they are classified as never shared, so they don't leave the node (maintainer's decision). Other nodes already see the holds and leases themselves (as cross-node warnings, D26), and a refusal or an expiry warning is only actionable on the node where it happened. Each node's own webhooks deliver its own signals.

## Remote `resource_find` stays node-wide

Token scoping covers **events**. `resource_find` keeps D16's node-wide, read-only awareness (maintainer's decision): borrowing a shared model server across projects is a core use case, and resource rows carry no bodies.

## Migration

Migration 7, additive:
- the `webhooks` table (including `disabled_at`, `disabled_reason`) and the `webhook_deliveries` table;
- a `resource_leases.expiry_warned_at` column (nullable, defaults to NULL).

## Out of scope

- Push to other nodes (federation already syncs logs).
- Websocket transport.
- Per-webhook rate limits; batching. Deliveries are sent one per event.
