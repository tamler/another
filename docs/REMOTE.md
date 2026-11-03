# Remote peers (Grok Bot and other remote agents)

A remote agent, such as Grok Bot running off your machine, joins as a first-class peer through **authenticated streamable HTTP MCP**. Each bearer token acts as exactly one agent, may only touch projects matching its globs, and every call it makes is recorded in an audit log. Reaping is never available remotely.

## 1. Register the remote agent and create a token

```bash
another agent register grok-bot --program grok_bot
```

```bash
another token create --agent grok-bot --project '/path/to/work/*' --label "grok bot" --expires 30d
```

The token (`ano_…`) is printed **once**; Another stores only its SHA-256 hash. Other commands:

```bash
another token list
```

```bash
another token revoke <token-id>
```

Project globs are matched against a project's absolute path and its slug (`*` stays within one path segment, `**` crosses segments). With no `--project`, the token can still use its own mailbox, heartbeat and the activity feed, but every project-scoped call is denied.

## 2. Run an authenticated server on loopback

Keep your local agents on the plain loopback server (or stdio), and run a second, authenticated server for the tunnel:

```bash
another serve --auth --bind 127.0.0.1:7467
```

With `--auth`, `/mcp`, `GET /activity` the push endpoints `GET /events/stream` and `GET /events/poll` ([PUSH.md](PUSH.md)), and `GET /decisions`, `GET /decisions/{id}` and `POST /decisions/{id}/answer` ([HUMAN-DECISIONS.md](HUMAN-DECISIONS.md)) require `Authorization: Bearer <token>`; `GET /health` stays open. Because tunnels deliver traffic from loopback, the authenticated server checks the token on **every** request, local or not.

Tunnels usually forward the public hostname in the `Host` header. Another rejects unknown hosts (DNS-rebinding protection), so name each tunnel hostname with `--allowed-host`.

## 3. Expose it through a tunnel (pick one)

### Tailscale Serve (private to your tailnet; recommended)

```bash
tailscale serve --bg 7467
```

```bash
another serve --auth --bind 127.0.0.1:7467 --allowed-host my-laptop.my-tailnet.ts.net
```

The peer connects to `https://my-laptop.my-tailnet.ts.net/mcp`. Use Serve (tailnet-only), not Funnel (public), unless you specifically need public reachability.

### Cloudflare Tunnel

```bash
cloudflared tunnel --url http://127.0.0.1:7467
```

```bash
another serve --auth --bind 127.0.0.1:7467 --allowed-host another.example.com
```

For a named tunnel, route your hostname to `http://127.0.0.1:7467` in its ingress rules and pass that hostname to `--allowed-host`. Put Cloudflare Access in front of it for a second layer if you like.

### ssh -R (through a host you control)

```bash
ssh -N -R 7467:127.0.0.1:7467 me@relay.example.com
```

On the relay host the server is reachable at `http://127.0.0.1:7467/mcp` (relay loopback only), so no `--allowed-host` is needed.

### Binding directly (discouraged)

A non-loopback bind is refused unless **both** `--auth` and `--allow-remote-bind` are given:

```bash
another serve --auth --allow-remote-bind --bind 0.0.0.0:7467
```

This serves plain HTTP, so put TLS in front of it or keep it on a private network.

## 4. Point the remote agent at it

Configure the remote agent's MCP client with the server URL (`https://…/mcp`) and an `Authorization: Bearer ano_…` header. A client without MCP can poll the feed instead:

```bash
curl -H "Authorization: Bearer $ANOTHER_TOKEN" "https://my-laptop.my-tailnet.ts.net/activity?since=1h&kinds=mail,request"
```

`GET /activity` takes `since` (a duration like `30m`, an RFC 3339 time, or an event-id cursor), `limit` and `kinds`, and returns the same JSON as the `activity_feed` tool. It never includes message bodies, and it only shows events from the token's projects (see [PUSH.md](PUSH.md#what-a-token-can-see)).

## What a token may do

| Tools | Rule |
|---|---|
| `fetch_inbox`, `acknowledge`, `claim_request`, `heartbeat`, `send_message` (`from`), `resource_borrow` / `release` / `heartbeat` | Must act as the token's agent; the agent is filled in when omitted |
| `hold_paths` | Token's agent, and a `project` matching the token's globs (required) |
| `ensure_project`, `resource_register` (with a project) | Project must match the token's globs |
| `register_agent` | Only the token's own identity |
| `threads` | Only threads the token's agent sent or received in |
| `activity_feed`, `activity_summarize`, `events_poll` | Only events from the token's projects (or, for project-less events, its own agent's); no message bodies |
| `resource_find` | Node-wide, read-only awareness |
| `webhook_subscribe` | Project globs default to the token's and may only be narrowed; `all_projects` is refused |
| `webhook_list`, `webhook_remove` | Only webhooks this token created |
| `orphans_list` | Lapsed registrations only; the process scan is never run for remote peers |
| `holds_by_lane` | Only holds in the token's projects |
| `lane_list`, `lane_show` | Only lanes whose project matches the token's globs (others are omitted, or `forbidden`) |
| `lane_create` | Token's agent, and a `project` matching the globs (required) |
| `lane_update`, `lane_report`, `lane_close` | Token's agent, on lanes in the token's projects |
| `decision_list`, `decision_show` | Only decisions whose project matches the token's globs |
| `decision_ask`, `decision_cancel` | Token's agent; the decision's project must match (cancel: only the asking agent) |
| `decision_answer` | Decision's project must match; `relayed_by` is always the token's agent |
| `resource_reap` | **Never** (remote reaping is disabled) |

## Audit log

Every authenticated or rejected remote request is recorded with time, token, agent, operation, outcome (`ok`, `error`, `denied`), redacted arguments (message bodies and notes are replaced by their size) and the forwarding header or peer address:

```bash
another audit --limit 50
```
