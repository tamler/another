# Federation (v2)

Several machines, such as a laptop, a remote runner and a GPU box, each run their own Another **node** and stay aware of each other. Nodes exchange their append-only, signed event logs through a **hub**. The hub is just another free Another node that you host yourself; spokes dial out to it, so spokes need no inbound ports.

## Model

- **Per-node authority.** Each node is authoritative for its own agents, path holds and resources. Events from other nodes are projected into local read models but never rewrite another node's ownership. Only the owning node can stop or reap its resources.
- **Signed events.** Every node has an Ed25519 key, created on first boot, and signs every event it authors. Relayed events are checked against their origin node's key, so the hub cannot forge another node's events.
- **Hub and spoke.** Spokes **push** the events they authored and **pull** a filtered stream of everyone else's events. Spokes never talk to each other directly. A mesh may come later.
- **Addressing.** Agents on other nodes are `name@project@node`, or `name@@node` for agents without a project. Local addresses (`name`, `name@project`) are unchanged.

## What travels where

| Events | Sent to |
|---|---|
| Directory: node, project and agent registrations | every paired node |
| Mail (`message.sent`) | only nodes that host a recipient; mail between agents on one node never leaves it |
| Read and acknowledgement receipts | only the node that sent the message |
| Activity: sessions, requests, holds, resources | nodes with `read_feed`, plus the owner of a resource being borrowed |
| Tokens, audit records, pairing records | never |

## Permissions

The hub grants each spoke a set of permissions at pairing:

| Permission | Allows |
|---|---|
| `read_feed` | receiving other nodes' activity (merged activity feed, remote resource visibility, hold-clash warnings) |
| `send_mail` | mailing agents on other nodes |
| `borrow_resource` | borrowing resources owned by other nodes (only when reachable) |

**There is no remote reap permission.** Reaping and stopping happen only on the owning node, with human confirmation (see [RESOURCES.md](RESOURCES.md)).

The hub enforces permissions on what spokes push, and each node checks them again before sending. A spoke may only push events it authored itself.

## Set up

Name each node first; names must be unique across the federation:

```bash
another node name studio
```

On the hub (keep it on loopback and expose it through a tunnel; see [REMOTE.md](REMOTE.md)):

```bash
another serve --federation --bind 127.0.0.1:7468
```

```bash
another pair invite --url https://studio.my-tailnet.ts.net --perms read_feed,send_mail,borrow_resource --expires 1h
```

The invite code is single-use. It carries the hub's URL, node id and public key. Compare the printed key fingerprint with `another node show` on the hub if the code came through an untrusted channel.

On each spoke:

```bash
another node name laptop
```

```bash
another pair join another-invite:...
```

```bash
another sync
```

```bash
another sync --watch --every 30
```

`another peers` lists paired nodes with key fingerprints, permissions and last sync. `another pair revoke <node>` cuts a node off: the hub refuses its next request.

## Cross-node behaviour

- **Mail**: `another send --from bob --to carol@@runner …`. Replies, threads and acknowledgements work across nodes. Requests stay node-local: open them only with agents on the same node.
- **Merged activity feed**: `another activity` shows other nodes' events prefixed with their node name, e.g. `[hub] alice@shop holds shop:src/**`. Message bodies never appear in the feed.
- **Resources**: `another resources` lists other nodes' resources with their `node` and whether this machine can **reach** them (a TCP connect to the endpoint). They can be borrowed only when reachable and when `borrow_resource` was granted, and the lease is reported back to the owning node. Loopback endpoints are reachable only from the same machine.
- **Lanes**: a lane is written only by the node that owns it. Other nodes keep a **read-only mirror** (name, project, lead, status; no charter or reports, which never travel), and `lane show` / `lane_list` mark it with `node`. Updating, reporting into or closing a mirror is a `conflict`: change it on its node (for example through a remote-agent token, see [REMOTE-AGENTS.md](REMOTE-AGENTS.md)) or mail its lead. Two nodes may each have an open lane with the same name; on each node the name means its own lane, and the other is reached by id.
- **Decisions**: a decision asked on one node is listed on the others with `node` set. Answering it there relays the answer, including any free text, **only to the decision's node**, which applies the first answer it receives (mail to the asking agent, lane release, spend approval) and ignores later ones. Its `decision.answered` then settles the mirrors.
- **Node-local signals**: `hold.conflict` and `lane.hold_conflict` (refused acquires) and `resource.lease_expiring` never leave their node. Subscribe to them on the node where the holds and leases live.
- **Path holds**: holds stay node- and repo-local and are never distributed locks. A synced hold keeps its lane when the receiving node knows that lane, and a cross-node warning is flagged `same_lane` when both holds are for the same lane. When an agent acquires a hold that overlaps a live hold on another node, in a project with the **same name**, the result carries a **warning** (`warnings` in JSON, `warning:` lines in the CLI), never a conflict.

## Wire protocol (for implementers)

- `POST /fed/pair`: `{invite_id, secret, node: {node_id, display_name, public_key}}` returns `{hub, perms}`.
- `POST /fed/push`: `{events: [WireEvent]}` returns `{accepted, duplicates, rejected}`.
- `GET /fed/pull?after=<hub seq>&limit=N` returns `{events, cursor, more, nodes}`.
- Push and pull requests carry these headers:
  - `x-another-node: <node id>`
  - `x-another-time: <ms>`
  - `x-another-signature: <Ed25519 over "another-request-v1\n{method}\n{path?query}\n{time}\n{sha256(body)}">`

  The hub rejects requests from unpaired or revoked nodes, and requests whose clock is more than 5 minutes off.
- A `WireEvent` is `{seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig}`, where `payload` is the exact signed JSON text. The signature covers `"another-event-v1\n{event_id}\n{node_id}\n{ts}\n{actor}\n{kind}\n{sha256(payload)}"`.

## Limits (v2)

- Mail bodies are readable by the hub that relays them. Host the hub yourself; end-to-end encryption is future work.
- Hub-and-spoke only, with no mesh. A spoke syncs with one hub.
- Requests and path holds are node-local.
