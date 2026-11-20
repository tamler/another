# Federation (v2)

Several machines, such as a laptop, a remote runner and a GPU box, each run their own Another **node** and stay aware of each other. Nodes exchange their append-only, signed event logs through a **hub**. The hub is just another free Another node that you host yourself; spokes dial out to it, so spokes need no inbound ports.

## Model

- **Per-node authority.** Each node is authoritative for its own agents, path holds and resources. Events from other nodes are projected into local read models but never rewrite another node's ownership. Only the owning node can stop or reap its resources. A signature proves only who sent an event, so before storing one a node also checks that the agents, projects, messages, holds and resources it would create or change belong to the event's origin node. An event that names another node's object (for example, a registration reusing another node's agent id, mail claiming another node's sender, or a read, ack, transfer or stop for another node's delivery or resource) is rejected, listed in the sync report's rejected events, and is not relayed. Mail to another node's agents and borrowing another node's resource are still allowed, within the permissions below. Whether a borrow needs `borrow_resource` follows the resource the hub knows, not any routing field in the event. A node may not rename itself to a name another known node has, and an address whose node name matches more than one known node is refused as ambiguous (use the node id). The origin chooses an event's time and TTLs, so a receiver takes the time no later than its own clock and clamps lease and hold TTLs to the local limits (24 hours for leases, 4 hours for holds): a skewed clock or a huge TTL cannot keep a foreign lease or hold alive. Each node bounds a lease by its own clock, and a node that syncs later sees the same borrow later. So the node that owns a resource also expires other nodes' leases on it when they run out, and publishes that expiry. Every node that replays the history then ends the lease at the same point, including the lease holder's node, which is always sent the expiry. A hub that ran an earlier build with untrusted spokes may still hold rows that forged events changed before this check existed; those rows are not repaired automatically.
- **Local rules.** A signature also does not prove that the sender's build applied the rules a local command applies, so the receiver applies them again (D98). An event is rejected, listed in the sync report's rejected events and not relayed when it carries: an agent name that is not 1 to 64 ASCII letters, digits, `-`, `_` or `.`; a resource name that is blank, over 200 bytes or holds a control or text-direction character; a resource endpoint over 2 KiB, purpose over 1 KiB, or metadata that is not a JSON object of at most 64 KiB; or a message body that is not text of at most 256 KiB. A rejected event is not sent again and does not hold back later events. A message is never rejected for its subject: an over-long subject is cut to 300 characters and stored as one safe line (D75). Other free text from a peer (a hold's reason, an agent's program, model and description, a resource's endpoint and purpose) is stored as one safe line (D97). An agent whose registration is rejected is not known on the receiving node, so it cannot be addressed there and its mail is not delivered there.
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
- **Spoke offline for days.** On its next `another sync`, the spoke pushes its backlog and pulls everyone else's until caught up, in batches and pages of at most 500 events and a few MiB each (see "Size limits" below). Days-old holds arrive already expired, so other nodes see only the spoke's current holds as live.
- **Replays are harmless.** Every event has a unique id, and both sides skip events they already have. Resending from zero changes nothing.
- **Hub restored from an older backup.** A spoke keeps two cursors: how far it has pushed its own events (its own sequence numbers), and how far it has pulled (the hub's sequence numbers). A restore takes the hub's sequence back. The pull cursor then points past the hub's log, so new hub events would be skipped. The push cursor says the spoke's events reached the hub, but the restore lost some of them, so they would never be sent again.

  The spoke detects this from the hub's `reset` answer, rewinds both cursors to zero and syncs again in the same `another sync`. `another sync --json` reports `"hub_reset": true`, and a local `peer.resynced` event records it, so the next sync does not reset again.

  The spoke sends two anchors with its first pull:
  - **Received:** the last event the hub actually sent it, with the hub sequence number it came at, even when that event was not stored (a duplicate, or one this node rejected). A local `peer.anchored` event records both. The hub answers `reset` when it no longer holds that event, or holds it at another sequence number: a restored hub can get it back when its author resyncs, but not at the same place.
  - **Pushed:** the last of the spoke's own events the hub accepted, recorded by a local `peer.pushed` event. Only this spoke pushes its own events, so no other spoke can put that one back. The spoke reads it before pushing anything new.

  Each pulled page commits its cursor and received anchor in one transaction, and each pushed batch its cursor and pushed anchor, so a failed or interrupted sync never leaves one ahead of the other. Then a second restore is noticed too. After a resync:
  - every spoke's own events reach the hub again;
  - events only the hub wrote after the backup are gone, as with any restore.
- **Limit.** A restore can still go unnoticed when all of these hold:
  - it lost none of this spoke's own events, and not the last event the hub sent this spoke (or that event is back at exactly the same sequence number);
  - the hub then writes past the spoke's old pull cursor before the spoke syncs.

  For example, it lost only mail between other nodes, for a spoke without `read-feed`. In that case the hub's new events between the backup's end and that cursor are skipped. A spoke upgraded from a version without these anchors also lacks them until its next pull and push. To rule it out after restoring a hub, run on each spoke:

  ```sh
  sqlite3 "$ANOTHER_HOME/another.db" "UPDATE peers SET push_cursor = 0, pull_cursor = 0"
  ```

  Then run `another sync`. A resend from zero is always safe.

## Size limits

A sync between honest nodes does not stop because of how much is waiting (D99). Three things bound it: the size of one event this build writes, the size of one request or answer, and the size of the batch or page that fills it.

**One event this build writes** is at most 2 MiB (2,097,152 bytes) as it travels: its wire JSON, escaping included, with the sequence number counted as one digit. The largest mail an honest node writes to one recipient is 1,837,191 bytes: a 256 KiB body made only of control characters, which cost seven bytes each on the wire, a 300-character subject and the longest names. This is a rule for local writes only. An event already in a log, or one a peer sends, is not judged by it: it travels if it fits the transport (below).

- A local command or tool that would write a larger event is refused as `invalid`, with the kind of event and both sizes, and nothing is written. Mail has no rule on how many agents one message names, so this is what stops a message to thousands of recipients: with the costliest body and the longest names it is refused at roughly 1,500 recipients, with a plain 256 KiB body at roughly 10,000.
- The fields that had no size rule each have one, so the caller is told which field is too long: an agent's description 4 KiB and model 200 bytes, a hold's reason 1 KiB and each pattern 4,096 bytes, a request's completion or cancellation note 16 KiB. The CLI prints, for example, `another: agent description is 4097 bytes; the limit is 4096` and exits non-zero; the MCP tools answer `invalid` with the same text. These rules apply to what a local caller writes, including a remote caller of `another serve`. The edit hook's holds and a session start are not held to them and write nothing near them.
- A heartbeat or a session end over more than 10,000 holds, or 10,000 leases, writes several `hold.renewed`, `hold.released`, `resource.renewed` or `resource.released` events of at most 10,000 ids each in its one transaction, so neither can meet the 2 MiB rule. Up to 10,000 it is one event, as before, and a reader takes the ids event by event.
- A peer's event is not refused for its size. What bounds it is the transport: it reaches a hub only in a push, so it is at most 8 MiB, and only the signed push of a paired, unrevoked node is read and stored. The other rules for a peer's event (see "Model") are unchanged.

**One request or answer.** Every route that reads a body states its limit:

| Route | Who can reach it | Largest body read |
|---|---|---|
| `POST /fed/push` | a caller whose node header names a paired, unrevoked spoke; any other is answered `401` before any of its body is read. The signature and the time are checked after the body | 8 MiB |
| `POST /fed/pair` | anyone who can reach the hub's address (an invite is checked after the body is read) | 2 MiB, as before |
| `GET /fed/pull` | paired nodes | no body |
| OAuth and dashboard `POST` routes | as before | 2 MiB, as before |
| `/mcp` | as before | read by the MCP library, unchanged |

A spoke reads a pull answer of at most 32 MiB, and a pairing or push answer of at most 10 MiB, as before. A body over a limit is answered `413` and is not read further. A caller with no pairing makes a hub buffer nothing on `/fed/push`. A caller that knows the id of a paired node, which every node of the federation does, can make it buffer 8 MiB per request before the signature fails, where it was 2 MiB; there is no limit on how many requests run at once, as before.

**One batch or page.** A push batch is closed before its request body would pass 4 MiB, and a pull page before its answer, the node directory included, would pass 8 MiB. Both also stop at 500 events, and both always hold at least one event, so an event larger than the budget travels alone. One event at the 2 MiB rule fits either, each budget is under the limit of the side that reads it, and the build fails if a change breaks that. Cursors, anchors and ordering are unchanged: the same events arrive in the same order however they are cut. Ordinary traffic costs the same number of requests as before (2,000 small messages: 5 pushes, 5 pulls).

**What can travel.** The limits of the transport decide, not the 2 MiB rule:

- A spoke's own event is pushed, alone if need be, when the push that carries it is at most 8 MiB (the event itself up to 8,388,595 bytes).
- Whatever a hub took in a push it can relay: the event is at most 8 MiB, a pull answer may be 32 MiB, and the rest of an answer is the node directory, which is about 150 bytes a node. The build fails if the pull limit is ever less than twice the push limit.
- A hub's own event is pulled, alone if need be, when the answer that carries it is at most 32 MiB.
- A build before these limits blocked at 2 MiB on a push and 10 MiB on a pull, so nothing it delivered is blocked now, and events between those sizes and the new ones now travel.

**With a node on an earlier build.**

- *New spoke, older hub.* The older hub reads at most 2 MiB per push and answers `413` to more, without reading the body; the spoke sees that answer or only a failed connection (a broken pipe on Linux, and on macOS sometimes "invalid argument"). On a `413`, or on any I/O error while a push is being sent other than a refused connection, it sends half as many events, down to one, for the rest of that sync. From 500 events that is at most eight cuts, so a network that is simply failing costs at most nine failed requests before its error is reported. The older hub bounds a page by count only; if an answer is over what the spoke reads, the spoke asks again with half the `limit`, down to one event.
- *Older spoke, new hub.* Nothing changes on the spoke. It gains from the hub's 8 MiB push limit and from pages closed at 8 MiB, under the 10 MiB it reads.
- When one event alone is over what the other side takes, halving ends and the sync fails with a message that names the cause and says that retrying will not help, for example `the hub at <url> refused event <id> (message.sent) as too large: ...`.

## If a sync reports an event that cannot be sent

Only an event that does not fit the transport even alone is left behind: in a spoke's log, one over the 8 MiB a hub reads; in a hub's log, one over the 32 MiB a spoke reads. No build from D99 on writes one (it writes at most 2 MiB), but an earlier build could, for example by registering an agent with a description of many megabytes through an MCP tool. The node sends everything before it, then `another sync` fails every time, exits non-zero, and prints

```text
another: event <id> (<kind>, <size> bytes on the wire) in this node's log is larger than a hub reads in one push (8388608 bytes), so it cannot be sent and the events after it wait behind it; retrying will not help; the owner must decide whether it is skipped or keeps blocking (docs/FEDERATION.md, "If a sync reports an event that cannot be sent")
```

A hub that holds such an event answers a spoke's pull with `pull failed (409 Conflict): event <id> ... in the hub's log is larger than a spoke reads in one pull answer (33554432 bytes) ...`, after the page before it. An earlier build stopped in the same place, at 2 MiB and 10 MiB, with a message that named nothing.

Whether Another should step past such an event by itself is not decided; it is the owner's decision, and the review of this change recommends skipping with a loud report. Until then the event is neither skipped nor deleted, and Another has no command that removes or steps past an event. What the owner can do today, on the node the message names (for the hub's message, on the hub):

1. Stop whatever runs Another there (`another serve`, the daemon) and copy the home directory as a backup.
2. Remove the one event from the log by hand:

   ```sh
   sqlite3 "$ANOTHER_HOME/another.db" "DELETE FROM events WHERE event_id = '<id>'"
   ```

3. Start Another again and run `another sync`.

What that costs: no other node ever receives that event. For an agent's registration, the agent stays unknown on the other nodes, so nobody there can address it and mail it sends is not delivered there, until it registers again with a description inside the limits. The node's own state is unchanged, since the event had already been applied there. The events after it are then sent. This removal on a spoke is run in the tests; it is a hand edit of the database, not a supported command.

## Wire protocol (for implementers)

- `POST /fed/pair`: `{invite_id, secret, node: {node_id, display_name, public_key}}` returns `{hub, perms}`.
- `POST /fed/push`: `{events: [WireEvent]}` returns `{accepted, duplicates, rejected}`. A body over the hub's limit is answered `413`; send fewer events.
- `GET /fed/pull?after=<hub seq>&limit=N[&after_id=<event id>[&after_id_seq=<hub seq>]][&pushed_id=<event id>]` returns `{events, cursor, more, nodes, reset}`. A page holds at most `limit` events (1 to 2000, default 500) and may hold fewer, with `more: true`, when the hub closes it by size. `after_id` is the last event the spoke received from this hub and `after_id_seq` the hub sequence number it came at; `pushed_id` is the last of the spoke's own events this hub accepted. `reset: true`, with no events, means the hub's log no longer reaches `after`, no longer holds `after_id` (at `after_id_seq`, when given), or no longer holds `pushed_id`; see "Restarts, restores and offline spokes" above. Hubs without `reset` omit it and ignore these parameters, and spokes without it ignore it.
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
