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
  - for ephemeral agents, how long it may go without being seen (`--idle-hours`; without it, the node setting `remote_idle_hours`, default 4, read when checked so changing it applies to existing agents).
- **No takeovers.** An invite can't be made for a name an agent on the hub already has, and a redemption fails (same `invalid or expired invite`) if someone registered the name after the invite was made. The exceptions: later uses of the same invite refresh its own agent, and a remote agent that has gone (no live token) can be invited again.
- **Single use and non-widening.** Redeeming takes only the code, plus optional client info (hostname, client version) that is recorded but never trusted. The resulting token's globs and lane are copied from the invite; a redeemer can't ask for anything else.
- **Each use mints a separate token.** An invite with `--uses 3` admits three sessions, each with its own token. The agent is registered once: later redemptions refresh it.
- **Status:** `list` shows uses, expiry and `active` / `used` / `expired` / `revoked`. `revoke` stops future redemptions; tokens already issued are kicked separately (`agent kick`).

**Redeeming:** `POST /join` with `{ "code": "anj_…", "client": { "hostname": "…", "version": "…" } }`, or the CLI `another join <hub-url> <code>`.

- `POST /join` is the only unauthenticated route besides `/health`: the code itself is the credential. It is only served when the hub runs with `--auth`, since a token is useless without it. It is **rate-limited**: 10 attempts a minute per client address, then `429 rate_limited` (audited as denied, `rate limited`).
- Codes and token digests are compared in constant time.
- **On success** the hub, in one transaction:
  1. registers the agent;
  2. creates a token (`ano_…`, label `invite <id>`; expiry = the invite's `--token-ttl` if given, otherwise none, because ephemeral agents are cleaned up by presence);
  3. records the membership (`remote_agents`);
  4. emits `agent.joined`.
- **The response** contains:
  - the agent's address, the token (shown once), the scope (globs, and the lane as an unambiguous `name@project`) and the hub URL;
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

1. **Get the binary.** Uses `another` if it is on `PATH`. Otherwise it downloads the release archive of **the hub's own version** (`v<version>`, pinned in the snippet; never "latest") for the platform, and installs it to `~/.local/bin` only if it matches the release's published `.sha256`. A checksum mismatch stops the script. With no archive for the platform, it builds that same tag: `cargo install --locked --git … --tag v<version> another`.
2. **Join:** `another join "$HUB" "$CODE" --write-mcp auto --hello`.
   - The token is saved to `$ANOTHER_HOME/remote.json` (`0600`), and the env exports go to `$ANOTHER_HOME/remote.env` (`0600`).
   - The MCP config is written: `claude mcp add --scope user …` when `claude` is on `PATH`, and the Codex table when `~/.codex` exists. For Codex, `ANOTHER_TOKEN` must be in its environment: source `remote.env`.
   - `--hello` sends a heartbeat (session start) and, with a lane, a status report "joined from <hostname>".

The snippet contains only the hub URL and the single-use code: no token, no hub keys.

## 3. Lifecycle and presence

The `remote_agents` table records, per invite redemption: the agent, invite and token, whether it is ephemeral, the idle limit, the join time, and the state (`active` / `gone`) with a reason.

**Presence** is computed from the agent's `last_seen_at`, which any authenticated call (reads included), heartbeat or hook updates:

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

It goes away after `heartbeat --end` / `POST /agent/leave` ends its session, or after its idle limit without being seen. Each lane it was in (the invite's lane, and any open lane it leads) gets **`lane.agent_left {lane_id, project_id, name, agent_id, reason}`**. That is a lane status event, so it shows in the lane feed and is delivered by the `lane_status` webhook filter.

**Only the token's projects.** A heartbeat or leave through the agent's token (HTTP or MCP), and its departure, renew or release only holds and leases in the token's projects. A persistent agent's leave also releases its resource leases and closes its spend leases there, like a session end. Anything the same agent holds elsewhere on the hub is left alone.

**When the lead leaves:**
- The lane member active longest takes over as lead, and the new lead and the lane's creator are mailed by the system agent `another`. Members are remote agents joined to the lane and agents holding files for it.
- With nobody left, the lane stays open with `leaderless_since` set. `lane.leaderless` is emitted, the creator is mailed, and the daily summary lists it.
- It is **never** blocked automatically. `lane update --lead` clears the flag.

**Kicking:**
- `another agent kick NAME [--reason]` (local) does the same at once, for **any** agent, ephemeral or not. It revokes **all** of the agent's tokens, so its next request gets 401, and emits `agent.kicked` plus `lane.agent_left`.
- Over MCP, `agent_kick {agent, target, reason?}` is for **manager agents** (program `grok_bot`) only. A remote manager acts as its token's agent and can only kick agents in its projects; others are `not_found`. Its kick revokes all of the target's tokens but releases holds and leases, and tells lanes, only in the manager's projects. People and the system agent can't be kicked.
- The reason is kept on the membership row, never in an event.

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
- **Writes on a mirror:** `lane_update` and `lane_close` are refused with `conflict` ("lane api is owned by node hub: …"), and so is asking a decision with a mirrored lane (the owner node is checked first). Concurrent changes therefore never race: the owner serialises them in its log, and mirrors converge by replaying the owner's events in order.
- **Reports on a mirror are proposals.** `lane_report` on a mirror (for example on a spoke that is offline) returns the report with `proposed_to: <node>`.
  - The summary travels as mail to an agent of the owner node (the lead, else the creator).
  - `lane.report_proposed` carries ids, status and progress only, and is routed to the owner node alone.
  - When it arrives, the owner applies it as a status report from its author, under the usual lane rules, and marks the carrier mail read. A proposal it can't apply emits `lane.proposal_ignored`.
  - Done reports can't be proposed.
- **Same name on two nodes:** two nodes may each have an open lane with the same name in the same project. On each node, a bare name or `name@project` means the **local** lane first; the other one is reachable by id.
- **Holds:** a mirrored `hold.acquired` keeps its `lane_id` when that lane exists locally (as a mirror or a local lane). Cross-node overlaps stay warnings (D26), now flagged `same_lane` when both holds are for the same lane.

**Decisions are answered where they were asked; answers flow back to the asker's node.**
- **Mirror:** a `decision.created` from another node is stored in `remote_decisions`, with the project, asker, person, deadline and status. `decision_list` / `decision_show` include these mirrors, with `node` set. Lists merge local and mirrored decisions newest first before the limit, and the lane and person filters apply to mirrors too.
  - The event carries no question text or labels (note 03), only the option **values**: single-word identifiers, enough to validate an answer.
  - The mirror's question is the question mail when that reached this node (the person lives here). Otherwise it points at the decision's node.
- **Answering a mirror** (`decision_answer`, CLI or HTTP):
  1. The answer is validated against the mirrored options.
  2. The mirror is marked `answer_sent`.
  3. The node emits **`decision.answer_relayed`** `{decision_id, decision_node, value, has_text, text_message_id?, answered_by, relayed_by}`, routed like mail **only to the decision's node**.
  - **No free text in events:** any answer text or note goes as mail to the asking agent, and the event only names that message. At the origin, the text is read from that mail, and the carrier is marked read: the answer mail is what the agent reads.
  - Text length and the note rule (note 03) are checked before the mirror is marked `answer_sent`.
  - So is the relayer rule (a confirmed manager or person, not the asker), so a refused relayer can't leave the mirror stuck in `answer_sent`.
  - **A relayer is required.** Use `--relayed-by`, or `--as` a person registered on this node. The origin applies a relayed answer only if `relayed_by_id` names an agent **of the node that signed the event**, whose role that node confirmed. A missing, unknown or foreign relayer is dropped (`decision.answer_ignored`, status `relayer_unknown` / `relayer_not_of_sender`).
- **The origin** applies the first relayed answer through the normal answer path: mail to the agent, wake, lane release, spend approvals. The self-approval rule still applies: the relaying agent must be a manager or a person, and not the asker. Later or refused answers are dropped, and the origin emits `decision.answer_ignored`. Its own `decision.answered` / `expired` / `cancelled` then syncs back and settles the mirror.
- **Conflict rule:** the first answer applied at the origin wins. Concurrent answers from two nodes can't both apply.

Spend stays node-local: accounts are local, and so are their entries.

## Migration

Migration 11, additive: the `agent_invites`, `remote_agents` and `remote_decisions` tables (with a nullable `idle_hours`: null follows the node setting), and a nullable `lanes.leaderless_at`. Lane mirrors reuse `lanes`, with `node_id` set to the origin.

## Security

- **Invite codes:** single-use by default, expiring, stored hashed, never widened, failures indistinguishable, every attempt audited.
- **Tokens:** tied to the invite's globs, so everything from phases 1–6 is scoped as before: lanes, events, stream/poll, decisions, holds, spend, summary and webhooks.
- **Kick and gone:** revocation is immediate, because every request verifies the token; open SSE streams close within 30 s (note 02).
- **Snippet:** contains no secret except the single-use code. `remote.json` and `remote.env` are `0600`, and the hub's `0700` home is never touched.
- **Federation:** decision answer text and proposed report summaries travel only as mail to the owner node; the events carry ids and status. Lane mirrors carry no content.
- **Snippet:** pinned to the hub's version and the release's checksum.

## Out of scope

- Multi-hub agents; per-invite rate limits (the `/join` limit is per client address).
- Shipping or configuring Tailscale.
