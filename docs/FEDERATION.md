# Federation (v2)

Several machines, such as a laptop, a remote runner and a GPU box, each run their own Another **node** and stay aware of each other. Nodes exchange their append-only, signed event logs through a **hub**. The hub is just another free Another node that you host yourself; spokes dial out to it, so spokes need no inbound ports.

## Model

- **Per-node authority.** Each node is authoritative for its own agents, path holds and resources. Events from other nodes are projected into local read models but never rewrite another node's ownership. Only the owning node can stop or reap its resources. A signature proves only who sent an event, so before storing one a node also checks that the agents, projects, messages, holds and resources it would create or change belong to the event's origin node. An event that names another node's object (for example, a registration reusing another node's agent id, mail claiming another node's sender, or a read, ack, transfer or stop for another node's delivery or resource) is rejected, listed in the sync report's rejected events, and is not relayed. Mail to another node's agents and borrowing another node's resource are still allowed, within the permissions below. Whether a borrow needs `borrow_resource` follows the resource the hub knows, not any routing field in the event. A node may not rename itself to a name another known node has, and an address whose node name matches more than one known node is refused as ambiguous (use the node id). The origin chooses an event's time and TTLs, so a receiver takes the time no later than its own clock and clamps lease and hold TTLs to the local limits (24 hours for leases, 4 hours for holds): a skewed clock or a huge TTL cannot keep a foreign lease or hold alive. Each node bounds a lease by its own clock, and a node that syncs later sees the same borrow later. So the node that owns a resource also expires other nodes' leases on it when they run out, and publishes that expiry. Every node that replays the history then ends the lease at the same point, including the lease holder's node, which is always sent the expiry. A hub that ran an earlier build with untrusted spokes may still hold rows that forged events changed before this check existed; those rows are not repaired automatically.
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

`another peers` lists paired nodes with key fingerprints, permissions and last sync. `another pair revoke <node>` cuts a node off: the hub refuses its next request, and ends that node's leases on the hub's own resources (reason `peer_revoked`), so a revoked node is never promoted to owner. Its agents, holds and leases on other nodes' resources stay until they expire. A revoked node can pair again only with a fresh, unused invite.

## Cross-node behaviour

- **Mail**: `another send --from bob --to carol@@runner …`. Replies, threads and acknowledgements work across nodes. Requests stay node-local: open them only with agents on the same node.
- **Merged activity feed**: `another activity` shows other nodes' events prefixed with their node name, e.g. `[hub] alice@shop holds shop:src/**`. Message bodies never appear in the feed.
- **Resources**: `another resources` lists other nodes' resources with their `node` and whether this machine can **reach** them (a TCP connect to the endpoint). They can be borrowed only when reachable and when `borrow_resource` was granted, and the lease is reported back to the owning node. Loopback endpoints are reachable only from the same machine.
- **Path holds**: holds stay node- and repo-local and are never distributed locks. When an agent acquires a hold that overlaps a live hold on another node, in a project with the **same name**, the result carries a **warning** (`warnings` in JSON, `warning:` lines in the CLI), never a conflict.

## Restarts, restores and offline spokes

- **Hub restart.** Everything is in the hub's database, so a restarted hub carries on where it stopped. Spokes resume from their cursors.
- **Spoke offline for days.** On its next `another sync`, the spoke pushes its backlog and pulls everyone else's in pages of 500 until caught up. Days-old holds arrive already expired, so other nodes see only the spoke's current holds as live.
- **Replays are harmless.** Every event has a unique id, and both sides skip events they already have. Resending from zero changes nothing.
- **Hub restored from an older backup.** A spoke keeps two cursors: how far it has pushed its own events (its own sequence numbers), and how far it has pulled (the hub's sequence numbers). A restore takes the hub's sequence back. The pull cursor then points past the hub's log, so new hub events would be skipped. The push cursor says the spoke's events reached the hub, but the restore lost some of them, so they would never be sent again.

  The spoke detects this from the hub's `reset` answer, rewinds both cursors to zero and syncs again in the same `another sync`. `another sync --json` reports `"hub_reset": true`, and a local `peer.resynced` event records it, so the next sync does not reset again.

  The anchor the spoke sends is the last event the hub actually sent it, even when that event was not stored (a duplicate, or one this node rejected); a local `peer.anchored` event records it in that case. Each pulled page commits its cursor and its anchor in one transaction, so a failed or interrupted sync never leaves one ahead of the other. Then a second restore is noticed too. After a resync:
  - every spoke's own events reach the hub again;
  - events only the hub wrote after the backup are gone, as with any restore.
- **Limit.** Detection relies on the hub missing the last event the spoke received from it, or on the spoke's cursor being beyond the hub's log. A restore can go unnoticed when both of these hold:
  - it lost only events this spoke never receives (for example, mail between other nodes for a spoke without `read-feed`);
  - the hub then writes past the spoke's old cursor before the spoke syncs.

  In that case the hub's new events between the backup's end and that cursor are skipped. To rule it out after restoring a hub, run on each spoke:

  ```sh
  sqlite3 "$ANOTHER_HOME/another.db" "UPDATE peers SET push_cursor = 0, pull_cursor = 0"
  ```

  Then run `another sync`. A resend from zero is always safe.

## Wire protocol (for implementers)

- `POST /fed/pair`: `{invite_id, secret, node: {node_id, display_name, public_key}}` returns `{hub, perms}`.
- `POST /fed/push`: `{events: [WireEvent]}` returns `{accepted, duplicates, rejected}`.
- `GET /fed/pull?after=<hub seq>&limit=N[&after_id=<event id>]` returns `{events, cursor, more, nodes, reset}`. `after_id` is the last event the spoke received from this hub. `reset: true`, with no events, means the hub's log no longer reaches `after` or no longer holds `after_id`; see "Restarts, restores and offline spokes" below. Hubs without this field omit it, and spokes without it ignore it.
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
