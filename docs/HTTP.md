# HTTP endpoints

Every route `another serve` serves. `scripts/gen-docs.py --check` fails CI when a route in the routers (`crates/another/src/serve.rs`, `fed.rs`) is missing here. The command and tool references are [CLI.md](CLI.md) and [MCP.md](MCP.md).

**Auth:**
- **Without `--auth`** (the default), the server binds loopback only and every route is open to local callers.
- **With `--auth`**, every route except `GET /health`, `POST /join` and `/fed/*` needs `Authorization: Bearer <token>`. Token callers see only their token's projects (D38). Hidden objects return the same `not_found` as missing ones. `/mcp` also refuses a `Host` that `--allowed-host` doesn't name. See [REMOTE.md](REMOTE.md).
- **`/fed/*`** is served only with `--federation`, and is authenticated by node signatures instead of tokens ([FEDERATION.md](FEDERATION.md)).

Errors are JSON: `{"error": {"code": "not_found" | "forbidden" | "invalid" | "conflict" | "rate_limited" | …, "message": "…"}}`.

## Core

| Endpoint | Auth | What it does |
|---|---|---|
| `GET /health` | open | `{"ok": true, "node_id": …, "auth": bool}`. Used by `another doctor`. |
| `POST /mcp` | token with `--auth` | MCP over streamable HTTP (also `GET` and `DELETE` for the session stream). The tools are in [MCP.md](MCP.md). |
| `GET /activity` | token with `--auth` | The activity feed, the same as MCP `activity_feed`. Query: `since` (a duration such as `1h`, an RFC 3339 time, an event id, or `@<seq>`), `limit`, `kinds` (comma-separated categories such as `mail,request`). |

## Push ([PUSH.md](PUSH.md))

| Endpoint | Auth | What it does |
|---|---|---|
| `GET /events/stream` | token with `--auth` | Server-Sent Events. Resumable with `Last-Event-ID`. Query: `since`, `kinds`. |
| `GET /events/poll` | token with `--auth` | Long-poll JSON. Waits up to `timeout` seconds (at most 60) for events after `since`. Query: `since`, `kinds`, `timeout`. |

## Decisions ([HUMAN-DECISIONS.md](HUMAN-DECISIONS.md))

| Endpoint | Auth | What it does |
|---|---|---|
| `GET /decisions` | token with `--auth` | Decisions, open ones by default. Query: `status`, `all`, `project`, `lane`, `limit`. |
| `GET /decisions/{id}` | token with `--auth` | One decision, with its options and answer. |
| `POST /decisions/{id}/answer` | token with `--auth` | Answer it. Body: `{"value": "ship", "text": "…", "answered_by": "pat"}` (`text` only when free text is allowed; `answered_by` defaults to the person asked). A token's agent is recorded as the relayer and must be a confirmed `grok_bot` or `human`. The asking agent can never answer. |

## Spend and summary ([SPEND.md](SPEND.md), [MANAGER-BOT.md](MANAGER-BOT.md))

| Endpoint | Auth | What it does |
|---|---|---|
| `GET /spend` | token with `--auth` | Spend report per account, with totals by lane and agent and a markdown burn-down. Query: `period` (`current` or `previous`), `account`, `lane`. |
| `GET /summary` | token with `--auth` | Daily summary. Query: `since` (default `24h`), `format=markdown` for the phone-sized text instead of JSON. |

## Remote agents ([REMOTE-AGENTS.md](REMOTE-AGENTS.md))

| Endpoint | Auth | What it does |
|---|---|---|
| `POST /join` | open (the invite code is the credential), served only with `--auth` | Redeem an invite. Body: `{"code": "anj_…", "client": {…}}`. Returns the agent, a scoped token, and MCP config for Claude Code and Codex. Rate-limited per client address. Every attempt is audited. |
| `POST /agent/heartbeat` | token | The token agent's session heartbeat. Body: `{"phase": "start" \| "beat" \| "end"}` (default `beat`). |
| `POST /agent/leave` | token | Leave the hub. An ephemeral agent's token is revoked, and its holds and leases in the token's projects are released. |
| `GET /lanes` | token | Lanes in the token's projects. |
| `POST /lanes/{lane}/report` | token | Post a report to a lane. Body: `{"summary": "…", "status": "working", "progress": 40}`. A report into a mirrored lane becomes a proposal for its owner node. |

## Federation ([FEDERATION.md](FEDERATION.md)), with `--federation`

| Endpoint | Auth | What it does |
|---|---|---|
| `POST /fed/pair` | the single-use pairing code in the body | A spoke redeems a hub's pairing invite (`another pair join`). |
| `POST /fed/push` | node signature (a paired spoke) | A spoke pushes the events it authored. Idempotent by event id. |
| `GET /fed/pull` | node signature (a paired spoke) | A spoke pulls everyone else's events, filtered for it. Query: `after` (sequence), `limit` (1–2000). |
