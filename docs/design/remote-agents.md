# Design note 07: First-class remote agents

Status: implemented in phase 7. Builds on [01-lanes](01-lanes.md), [02-push](02-push.md), [03-decisions](03-decisions.md), [04-lane-holds](04-lane-holds.md), [05-spend](05-spend.md) and [06-summary](06-summary.md).

## Problem

Remote agents can already use a hub. But each one needs the person to do three things by hand:
- register the agent;
- mint a long-lived bearer token;
- hand-write an MCP config.

The token then lives forever unless someone remembers to revoke it. Examples of remote agents: a cloud Claude Code or Codex session, a second machine, a manager bot.

Nothing tracks whether a remote agent is still there. When a cloud session is thrown away, its holds and leases linger until their TTLs run out, and its lane never hears that it left.

The goal: a remote agent joins in **one step** with a short-lived, single-use code. It gets exactly the access the person chose, takes part in lanes, and leaves cleanly, or is removed cleanly.

## 1. Invites

```bash
another invite create --agent cloud-claude --program claude --project ~/Projects/shop \
  --expires 24h --uses 1 --lane api [--idle-hours 4] [--persistent] [--print-snippet --hub-url https://…]
another invite list
another invite revoke <invite-id>
```

**The invite:**
- **Code format:** `anj_<invite_id>_<random>` ("another join"), at least 192 random bits. Only a SHA-256 hash of it is stored, and it is printed once.
- **What it fixes, all chosen by the person:**
  - the agent name and program;
  - the project globs (required, one or more);
  - an optional lane, which must be open and in a project matching the globs;
  - the expiry (default 24 h, at most 30 days);
  - how many uses it allows (default 1);
  - whether the agent is ephemeral (the default) or persistent;
  - for ephemeral agents, how long it may go without a heartbeat (`--idle-hours`, default 4).
- **Single use and non-widening.** Redeeming takes only the code, plus optional client info (hostname, client version) that is recorded but never trusted. The resulting token's globs and lane are copied from the invite; a redeemer can't ask for anything else.
- **Each use mints a separate token.** An invite with `--uses 3` admits three sessions, each with its own token. The agent is registered once: later redemptions refresh it.
- **Status:** `list` shows uses, expiry and `active` / `used` / `expired` / `revoked`. `revoke` stops future redemptions; tokens already issued are kicked separately (`agent kick`).

**Redeeming:** `POST /join` with `{ "code": "anj_…", "client": { "hostname": "…", "version": "…" } }`, or the CLI `another join <hub-url> <code>`.

- `POST /join` is the only unauthenticated route besides `/health`: the code itself is the credential. It is only served when the hub runs with `--auth`, since a token is useless without it.
- **On success** the hub, in one transaction:
  1. registers the agent;
  2. creates a token (`ano_…`, label `invite <id>`; expiry = the invite's `--token-ttl` if given, otherwise none, because ephemeral agents are cleaned up by presence);
  3. records the membership (`remote_agents`);
  4. emits `agent.joined`.
- **The response** contains:
  - the agent's address, the token (shown once), the scope (globs, lane) and the hub URL;
  - `mcp.claude_code`: a `claude mcp add` command line and the JSON for `.mcp.json` / `~/.claude.json`;
  - `mcp.codex`: a `~/.codex/config.toml` table with `url` and `bearer_token_env_var = "ANOTHER_TOKEN"`;
  - `env`: `ANOTHER_HUB`, `ANOTHER_TOKEN`, `ANOTHER_AGENT` and `ANOTHER_LANE`.
- **Failures** all return `403 {"error":{"code":"forbidden","message":"invalid or expired invite"}}`: unknown, wrong secret, expired, used up, revoked, or a lane closed since the invite was made. The code doesn't reveal which; the audit log does.
- **Audit:** every attempt is audited (`POST /join`, outcome ok, denied or error), with the invite id when the code parses and the client info.

**The agent's project:**
- With a lane, it is the lane's project.
- Otherwise, if the globs match exactly one project on the hub, it is that project.
- Otherwise the agent has no project, so its address is its bare name.

## 2. Join snippet

`invite create --print-snippet --hub-url URL` prints a POSIX `sh` block for a cloud session:

1. **Get the binary.** Uses `another` if it is on `PATH`. Otherwise it downloads the latest GitHub release archive for the platform (`another-<version>-<target>.tar.gz`), verifies its `.sha256`, and installs it to `~/.local/bin`. If that fails (no release, or an unknown platform), it runs `cargo install --locked --git https://github.com/tamler/another another`.
2. **Join:** `another join "$HUB" "$CODE" --write-mcp auto --hello`.
   - The token is saved to `$ANOTHER_HOME/remote.json` (`0600`), and the env exports go to `$ANOTHER_HOME/remote.env` (`0600`).
   - The MCP config is written: `claude mcp add --scope user …` when `claude` is on `PATH`, and the Codex table when `~/.codex` exists. For Codex, `ANOTHER_TOKEN` must be in its environment: source `remote.env`.
   - `--hello` sends a heartbeat (session start) and, with a lane, a status report "joined from <hostname>".

The snippet contains only the hub URL and the single-use code: no token, no hub keys.

## 3. Lifecycle and presence

The `remote_agents` table records, per invite redemption: the agent, invite and token, whether it is ephemeral, the idle limit, the join time, and the state (`active` / `gone`) with a reason.

**Presence** is computed from the agent's `last_seen_at`, which any call, heartbeat or hook updates:

| Presence | When |
|---|---|
| `online` | seen within the last 5 minutes |
| `idle` | seen less than the idle limit ago (persistent agents: never gone through idleness) |
| `gone` | it left, was kicked, or (ephemeral only) its session ended or it went past the idle limit |

**When an ephemeral agent goes away**, the sweeper (`serve` runs it every 30 s) does this once:
1. revokes its invite tokens;
2. releases its holds (reason `gone`);
3. ends its open spend leases, recording their cost, and withdraws pending ones;
4. marks it offline;
5. emits `agent.gone {agent_id, reason}`.

It goes away after `heartbeat --end` / `POST /agent/leave` ends its session, or after `idle_hours` without being seen. Each lane it was in (the invite's lane, and any open lane it leads) gets **`lane.agent_left {lane_id, project_id, name, agent_id, reason}`**. That is a lane status event, so it shows in the lane feed and is delivered by the `lane_status` webhook filter.

**Kicking:** `another agent kick NAME [--reason]` (local only) does the same at once, for **any** agent, ephemeral or not. It revokes **all** of the agent's tokens, so its next request gets 401, and emits `agent.kicked` plus `lane.agent_left`.

**Views:**
- `another agent remotes` lists joined agents with their presence.
- `daily_summary` lists agents that left in its window.

**Remote-agent HTTP routes** (bearer token, scoped, audited), for clients without MCP, such as the CLI and shell scripts:

| Route | What it does |
|---|---|
| `POST /agent/heartbeat {"phase":"start"\|"beat"\|"end"}` | Session heartbeat: renews holds and leases; `end` releases them |
| `POST /agent/leave` | Leave: an ephemeral agent goes `gone` now, a persistent one ends its session |
| `GET /lanes` | Lanes in scope |
| `POST /lanes/{lane}/report {"summary", "status"?, "progress"?}` | Lane report as the token's agent |
| `GET /decisions`, `POST /decisions/{id}/answer` | As in note 03 |

The client CLI uses `remote.json`: `another remote status|heartbeat|lanes|report|decisions|answer|leave`.

## 4. Reachability

Covered in docs only, in [REMOTE-AGENTS.md](../REMOTE-AGENTS.md#reaching-the-hub):

1. **Tailnet (recommended).** The cloud session joins the person's tailnet with an **ephemeral, tagged, pre-approved auth key** in **userspace networking** mode (`tailscaled --tun=userspace-networking`). An ACL limits `tag:another-agent` to the hub's `:443`, where `tailscale serve` fronts `another serve --auth` on loopback.
2. **Federation spoke** for agents that work offline: `another pair join` / `another sync`. The doc lists which events sync (mail to recipient nodes, feed events, lane mirrors, decision answers).
3. **Public tunnel** (Cloudflare Tunnel, ngrok) with token auth. **Not recommended**: the hub is exposed to the internet, and only the bearer token protects it.

The binary has no Tailscale dependency.

## 5. Federation tie-in

Today, lane events federate as feed events but aren't projected, and decisions don't cross nodes at all. With remote agents, a spoke's agents need to see the hub's lanes, and a spoke's decisions need to be answerable from the hub.

**Lanes have one writer: their owner node.**
- **Mirrors:** other nodes keep a read-only mirror, projected from `lane.created`, `lane.updated`, `lane.status_changed` and `lane.closed`. It holds the name, project (the origin's), lead, creator and status. There is no charter or reports, since events never carry them.
- **Owner:** `Lane.node` names the owner node (null when local).
- **Writes on a mirror:** `lane_update`, `lane_report` and `lane_close` are refused with `conflict` ("lane api is owned by node hub: use the hub's MCP, or mail its lead"). Concurrent changes therefore never race: the owner serialises them in its log, and mirrors converge by replaying the owner's events in order.
- **Same name on two nodes:** two nodes may each have an open lane with the same name in the same project. On each node, a bare name or `name@project` means the **local** lane first; the other one is reachable by id.
- **Holds:** a mirrored `hold.acquired` keeps its `lane_id` when that lane exists locally (as a mirror or a local lane). Cross-node overlaps stay warnings (D26), now flagged `same_lane` when both holds are for the same lane.

**Decisions are answered where they were asked; answers flow back to the asker's node.**
- **Mirror:** a `decision.created` from another node is stored in `remote_decisions`, with the question, options, project, asker, deadline and status. Its question and options already travel in the event, because the person must see them. `decision_list` / `decision_show` include these mirrors, with `node` set.
- **Answering a mirror** (`decision_answer`, CLI or HTTP):
  1. The answer is validated against the mirrored options.
  2. The mirror is marked `answer_sent`.
  3. The node emits **`decision.answer_relayed`** `{decision_id, decision_node, value, text?, answered_by, relayed_by}`. This is routed like mail, **only to the decision's node**, so free text never reaches other peers.
- **The origin** applies the first relayed answer through the normal answer path: mail to the agent, wake, lane release, spend approvals. Later answers are ignored, and the origin emits `decision.answer_ignored`. Its own `decision.answered` / `expired` / `cancelled` then syncs back and settles the mirror.
- **Conflict rule:** the first answer applied at the origin wins. Concurrent answers from two nodes can't both apply.

Spend stays node-local: accounts are local, and so are their entries.

## Migration

Migration 11, additive: the `agent_invites`, `remote_agents` and `remote_decisions` tables. Lane mirrors reuse `lanes`, with `node_id` set to the origin.

## Security

- **Invite codes:** single-use by default, expiring, stored hashed, never widened, failures indistinguishable, every attempt audited.
- **Tokens:** tied to the invite's globs, so everything from phases 1–6 is scoped as before: lanes, events, stream/poll, decisions, holds, spend, summary and webhooks.
- **Kick and gone:** revocation is immediate, because every request verifies the token; open SSE streams close within 30 s (note 02).
- **Snippet:** contains no secret except the single-use code. `remote.json` and `remote.env` are `0600`, and the hub's `0700` home is never touched.
- **Federation:** decision answer text goes only to the decision's node. Lane mirrors carry no content.

## Out of scope

- Proposals from mirrors: offline lane reports applied by the owner later.
- Multi-hub agents; per-invite rate limits.
- Shipping or configuring Tailscale.
