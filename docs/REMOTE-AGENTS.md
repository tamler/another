# Remote agents

Let an agent running somewhere else join your Another hub in one step: a cloud Claude Code or Codex session, another machine, a manager bot. It gets exactly the access you choose, takes part in lanes, and leaves cleanly. Design: [design/remote-agents.md](design/remote-agents.md).

## In 10 lines: let a cloud agent into Another

1. Run the hub with auth on loopback: `another serve --auth --bind 127.0.0.1:7466`.
2. Make it reachable over your tailnet: `tailscale serve --bg 7466` (see [Reaching the hub](#reaching-the-hub)).
3. Mint an invite: `another invite create --agent cloud-claude --program claude --project ~/Projects/shop --lane api --expires 2h --print-snippet --hub-url https://hub.your-tailnet.ts.net`
4. Paste the printed snippet into the cloud session. It installs `another`, joins, writes the MCP config and says hello in the lane.
5. The code works once and expires. The snippet contains no other secret.
6. Restart the cloud agent with `. ~/.another/remote.env` so `ANOTHER_TOKEN` is set. It now has Another's MCP tools, limited to `shop`.
7. Watch it: `another agent remotes` (online / idle / gone), `another lane show api`, `another summary`.
8. It reports with `lane_report` (or `another remote report`), asks with `decision_ask`, and holds files before editing.
9. When the session ends, or it is unseen for 4 h (`--idle-hours`), it leaves on its own: token revoked, holds released, lane told.
10. To end it now: `another agent kick cloud-claude@shop`. Its token fails on the next request.

## Invites

```bash
another invite create --agent NAME --program claude|codex|grok|grok_bot|cursor|other \
  --project GLOB [--project GLOB …] [--expires 24h] [--uses 1] [--lane LANE] \
  [--idle-hours 4] [--persistent] [--token-ttl 7d] [--print-snippet --hub-url URL]
another invite list
another invite revoke <invite-id>
```

- **The code** looks like `anj_…`. It is shown once and stored only as a hash. It is single-use by default; with `--uses N`, each use gets its own token.
- **Fixed by you:** the invite fixes the agent's name, program, project globs and lane. The joining side can't change or widen any of them.
- **Ephemeral or persistent:** joined agents are ephemeral unless you pass `--persistent`.
- **Revoking** an invite stops new redemptions. Agents that already joined are removed with `another agent kick`.

## Joining

The snippet runs this; you can also run it yourself:

```bash
another join https://hub.your-tailnet.ts.net anj_… --write-mcp auto --hello
```

**Saved locally:**
- `$ANOTHER_HOME/remote.json` (`0600`): the hub, agent, token and lane.
- `$ANOTHER_HOME/remote.env` (`0600`): `ANOTHER_HUB`, `ANOTHER_TOKEN`, `ANOTHER_AGENT`, `ANOTHER_LANE`.

**MCP config written by `--write-mcp`** (`auto`, `claude`, `codex`, `both`, `none`; the default is `none`, which prints the config instead):
- **Claude Code:** runs `claude mcp add --scope user --transport http another <hub>/mcp --header "Authorization: Bearer …"`.
- **Codex:** adds this to `~/.codex/config.toml`, with the token read from the environment:
  ```toml
  [mcp_servers.another]
  url = "https://hub.your-tailnet.ts.net/mcp"
  bearer_token_env_var = "ANOTHER_TOKEN"
  ```

**`--hello`** sends a session-start heartbeat and posts "joined from <host>" to the invite's lane.

**Without the CLI**, `POST <hub>/join` with `{"code": "anj_…"}` returns the token, scope, Claude Code / Codex config and env vars as JSON. Every failure is the same `403 invalid or expired invite`, and every attempt is in `another audit`.

## Working as a remote agent

| | MCP (preferred) | CLI / HTTP |
|---|---|---|
| Presence | `heartbeat` | `another remote heartbeat` · `POST /agent/heartbeat` |
| Lanes | `lane_list`, `lane_show`, `lane_report` | `another remote lanes`, `another remote report --summary …` · `GET /lanes`, `POST /lanes/{lane}/report` |
| Decisions | `decision_ask`, `decision_list`, `decision_answer` | `another remote decisions`, `another remote answer ID VALUE` |
| Holds | `hold_paths`, `holds_by_lane` | |
| Leave | `heartbeat` with `end` | `another remote leave` · `POST /agent/leave` |

Everything is limited to the invite's project globs: lanes, events, stream, webhooks, decisions, holds, spend and the summary. A webhook the agent subscribes defaults to those globs and can only narrow them.

## Presence, leaving and kicking

`another agent remotes` shows each joined agent as:
- `online`: seen in the last 5 minutes;
- `idle`: seen before that;
- `gone`: it left or was removed.

**An ephemeral agent goes `gone`** after:
- its session ends (`heartbeat --end`, `POST /agent/leave`, or the client's SessionEnd hook); or
- `--idle-hours` without being seen.

**When it goes:**
- its invite token is revoked;
- its holds are released;
- its resource and spend leases are closed (with their cost recorded);
- `agent.gone` is emitted, plus `lane.agent_left` in its lanes (the `lane_status` webhook filter delivers it).

`another agent kick NAME [--reason …]` does all of that immediately, for any agent, and revokes all of its tokens.

## Reaching the hub

The binary has no network add-ons. Pick one of these:

### (a) Tailnet with an ephemeral, tagged auth key (recommended)

The hub stays on loopback, and `tailscale serve` publishes it to your tailnet over HTTPS:

```bash
# On the hub machine (tagged tag:another-hub in the admin console)
another serve --auth --bind 127.0.0.1:7466
tailscale serve --bg 7466        # https://<hub>.<tailnet>.ts.net → 127.0.0.1:7466
```

**Get an auth key.** In the Tailscale admin console, generate an auth key that is:
- **Ephemeral**: the node disappears when it goes offline;
- **Pre-approved**;
- tagged **`tag:another-agent`**;
- short-lived and single-use if you can.

Put it in the cloud session's secrets, never in the snippet.

**In the cloud session**, containers usually have no TUN device, so use userspace networking:

```bash
tailscaled --tun=userspace-networking --socks5-server=localhost:1055 \
  --outbound-http-proxy-listen=localhost:1055 --state=mem: &
tailscale up --auth-key="$TS_AUTHKEY" --hostname="cloud-agent-$(date +%s)"
export HTTPS_PROXY=http://localhost:1055 ALL_PROXY=socks5://localhost:1055
# then run the join snippet; `another` and MCP clients reach the hub through the proxy
```

**Limit the tag to the hub's HTTPS port** in your tailnet policy:

```json
{
  "tagOwners": {
    "tag:another-agent": ["autogroup:admin"],
    "tag:another-hub": ["autogroup:admin"]
  },
  "acls": [
    { "action": "accept", "src": ["tag:another-agent"], "dst": ["tag:another-hub:443"] }
  ]
}
```

Agents tagged this way can reach only the hub's port 443, and nothing else on your tailnet. They don't even reach each other. Another's token still decides what they can do once connected.

### (b) Federation spoke, for agents that work offline

An agent that can't stay connected, such as a laptop on a plane or a batch box, runs its **own** Another node and syncs with the hub when it can:

```bash
# hub
another pair invite --url https://hub.your-tailnet.ts.net --perms read_feed,send_mail
# spoke (needs a node name first: another node name laptop)
another pair join <pairing-code>
another sync            # or: another sync --watch
```

**What syncs:**

| Event | Where it goes |
|---|---|
| Directory (nodes, projects, agents) | Everywhere |
| Mail | Only to the nodes of its recipients |
| Feed events (holds, resources, lanes, decisions) | Peers with `read_feed` |
| Relayed decision answers | Only to the decision's node |
| Tokens, invites, audit | Never |

**Lanes** are written only by their owner node; other nodes see a read-only mirror with no charter or reports. **Decisions** asked on a spoke show up on the hub with `node` set. Answering one there relays the answer back, the first answer wins, and the spoke's agent gets it in its inbox after the next sync. **Holds** never block across nodes; overlapping ones are warnings, flagged `same_lane` when both holds are for the same lane. See [FEDERATION.md](FEDERATION.md).

### (c) Public tunnel with token auth (not recommended)

`cloudflared tunnel --url http://127.0.0.1:7466`, ngrok or similar, with `another serve --auth --allowed-host <tunnel-host>`. **Not recommended:** the hub is then reachable from the whole internet, and only bearer tokens protect it. Keep invites single-use and short-lived, and use `another audit` to watch `POST /join` attempts.
