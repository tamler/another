# Push: event stream, long-poll and webhooks

A manager bot shouldn't have to poll files or the feed to learn that a lane is blocked or needs a decision. Another pushes events in three ways:

| Way | Best for | Endpoint / tool |
|---|---|---|
| Server-Sent Events | A long-running bot that keeps a connection open | `GET /events/stream` |
| Long-poll | Anything that can make HTTP requests; MCP-only clients | `GET /events/poll`, MCP `events_poll` |
| Webhooks | A bot that should be woken up (serverless, chat bots) | `another webhook add`, MCP `webhook_subscribe` |

All three only carry events the caller's token may see (below). Design: [design/02-push.md](design/02-push.md).

## What a token can see

With `another serve --auth`, every event read is filtered by the token's project globs. This covers `activity_feed`, `activity_summarize`, `GET /activity`, the stream, long-poll and webhooks.

- An event that belongs to projects is visible only if one of its projects matches the token's globs.
  - That includes lanes, holds, resources, requests and mail between agents with projects.
  - The token's agent having acted in another project does not make that event visible.
- An event with no project, such as mail between agents that have no project or their sessions, is visible only if it concerns the token's own agent. The match is by agent id, never by name, so another node's agent with the same name doesn't count. Webhooks a token created follow the same rule.
- Token, peer and node events are never visible to tokens.
- Requests in `activity_summarize` counts follow the same rule as their events: a request into another project stays hidden even when the token's own agent sent it.
- Local callers (CLI, stdio MCP, loopback HTTP without `--auth`) see everything.

Items are one-line summaries plus ids. Message bodies are never included.

**Cursors:** `next_since` is normally an event id. When the last event a read scanned is one the token can't see, `next_since` is an opaque position like `@1234` instead, so a hidden event's id is never handed out. Pass either form back as `since`. A hidden event's id is the same `not_found` as an unknown one.

## Stream (SSE)

```bash
curl -N -H "Authorization: Bearer $ANOTHER_TOKEN" \
  "https://my-laptop.my-tailnet.ts.net/events/stream?since=15m&kinds=lane,request,hold"
```

```
id: 01JD...
event: lane.status_changed
data: {"event_id":"01JD...","kind":"lane.status_changed","summary":"manager moved lane api: working -> needs_decision",...}
```

- **Parameters:**
  - `since` is an event id, a duration (`15m`, `2h`) or an RFC 3339 time. Without it, the stream starts from now.
  - `kinds` is a comma-separated list of categories: `mail`, `request`, `hold`, `resource`, `session`, `system`, `federation`, `lane`.
- **Reconnecting:** standard SSE clients reconnect with `Last-Event-ID` and resume where they left off.
- **Timing:** the server checks the log every second, so events written by other processes (the CLI, other agents' MCP servers) arrive within about a second. A keep-alive comment is sent every 15 seconds.

## Long-poll

```bash
curl -H "Authorization: Bearer $ANOTHER_TOKEN" \
  "https://my-laptop.my-tailnet.ts.net/events/poll?since=$CURSOR&timeout=30"
# {"items": [...], "next_since": "01JD..."}
```

The request returns as soon as there is at least one event, or after `timeout` seconds (at most 60) with `items: []`. Pass `next_since` back as `since`; the cursor also skips past events your token can't see.

The MCP tool `events_poll` takes `since`, `kinds` and `timeout_seconds` (at most 30).

## Webhooks

```bash
another webhook add --url https://bot.example.com/another \
  --events lane_status,needs_decision,request_answered \
  --project ~/Projects/shop
# prints the webhook id and the signing secret (shown once)
another webhook list
another webhook test <webhook-id>     # send a signed test delivery now
another webhook dead                  # deliveries that gave up
another webhook retry <delivery-id>   # re-queue a dead delivery
another webhook deliveries --webhook <webhook-id>
another webhook remove <webhook-id>
```

**Rules:**
- `--project` takes a path or slug glob and can be repeated. It is required unless you pass the explicit `--all-projects`.
- URLs must be `https://`. The only exception is plain `http://` to `127.0.0.1`, `localhost` or `[::1]`, for local relays.
- Redirects are not followed.

**Filters:**

| Filter | Fires on |
|---|---|
| `lane_status` | `lane.created`, `lane.status_changed`, `lane.closed` |
| `lane_report` | `lane.reported` |
| `needs_decision` | a lane moving to `needs_decision` (by update or report); `decision.created`; `decision.overdue` ([human decisions](HUMAN-DECISIONS.md)) |
| `request_created` | `request.created`, `decision.created` |
| `request_answered` | `request.completed`, `request.cancelled`, `decision.answered`, `decision.expired` |
| `hold_conflict` | `hold.conflict`: an agent was refused a hold because another agent holds an overlapping path (node-local: it is not federated, so subscribe on the node where the holds are); `lane.hold_conflict`: the same, between two lanes (also node-local) |
| `lease_expiring` | `resource.lease_expiring`: a resource lease has under 5 minutes (or half its TTL) left. Sent once; renewing re-arms it. Node-local, not federated |
| `budget` | `spend.budget_threshold` (an account crossed 80% or 100% of its budget) and `spend.approval_requested` ([spend](SPEND.md)) |
| `all` | every visible event except mail and delivery events |

**Remote (MCP) subscriptions:**
- `webhook_subscribe` defaults to the token's project globs. It can narrow them to a subset (compared as strings) but never widen them.
- `all_projects` is refused for tokens.
- **A webhook stops when its token does.** If the token that created it is revoked or expires, the webhook is disabled: it never fires again, its pending deliveries become dead, and `another webhook list` shows `disabled: token revoked` (or `token expired`). Create a new one with a new token.
- A token can only list and remove the webhooks it created.

### Delivery

`another serve` (stdio or HTTP) delivers webhooks; nothing is sent unless a server is running. Every 2 seconds it queues new matching events and POSTs the ones that are due:

```
POST /another HTTP/1.1
Content-Type: application/json
X-Another-Event: lane.status_changed
X-Another-Delivery: 01JD...
X-Another-Timestamp: 1730600000000
X-Another-Signature: sha256=5f1c...

{"delivery_id":"01JD...","webhook_id":"01JC...","filter":"needs_decision",
 "event":{"event_id":"...","kind":"lane.status_changed","summary":"...","refs":{...}},
 "data":{"lane_id":"...","project_id":"...","name":"api","from":"working","to":"needs_decision"}}
```

- `event` is the feed item, but its `summary` is rebuilt from the event kind and the allowlisted data (for example `lane.status_changed (name=api, from=working, to=needs_decision)`), so request titles and other text the local feed shows never reach the receiver. `data` is the event's payload **reduced to ids, statuses, names, numbers and flags**: free text such as a request's completion note, a subject, a summary or a reason is dropped. Mail events never match a filter, so bodies are never sent either.
- **Success:** any 2xx response marks the delivery as delivered.
- **Retries:** anything else (another status, a network error, a 10-second timeout) is retried after 10 s, 30 s, 2 min, 10 min, 1 h and 6 h. After 7 attempts the delivery is **dead**: it shows in `another webhook dead` with its last error, and `another webhook retry` re-queues it.
- **Several servers:** if more than one `another serve` runs on the node, each delivery is claimed (for a minute) immediately before it is sent, so it goes out once even when receivers are slow.
- **Managing:** a token sees and removes only its own webhooks. Another token's webhook id gets the same `not_found` as an unknown one.

### Verifying signatures

Compute HMAC-SHA256 with the secret as the key over `"<X-Another-Timestamp>.<raw body>"`, hex-encode it, and compare it with the part of `X-Another-Signature` after `sha256=` using a constant-time comparison. Reject requests whose timestamp is more than a few minutes old. Each retry is signed again with a new timestamp but keeps its `X-Another-Delivery` id, so dedupe on that id.

```python
import hmac, hashlib, time

def verify(secret: str, headers, body: bytes) -> bool:
    ts = headers["X-Another-Timestamp"]
    if abs(time.time() * 1000 - int(ts)) > 5 * 60 * 1000:
        return False
    mac = hmac.new(secret.encode(), f"{ts}.".encode() + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest("sha256=" + mac, headers["X-Another-Signature"])
```

### Secrets

The signing secret is stored in the node database, because it is needed for signing, and the node home is `0700`. It is printed once and never listed. To rotate it, add a new webhook and remove the old one.
