# Federation (v2)

Several machines, such as a laptop, a remote runner and a GPU box, each run their own Another **node** and stay aware of each other. Nodes exchange their append-only, signed event logs through a **hub**. The hub is just another free Another node that you host yourself; spokes dial out to it, so spokes need no inbound ports.

**Status:** Hub and spoke between the owner's Mac and VPS is in scope for v0.1.0 (M9) and required for the tag, including a tested hub backup restore. Fixes to this path are admitted case by case. Bearer-token remote peers and hosted OAuth connectors remain frozen and experimental; federation does not depend on them.

## Model

- **Per-node authority.** Each node is authoritative for its own agents, path holds and resources. Events from other nodes are projected into local read models but never rewrite another node's ownership. Only the owning node can stop or reap its resources. A signature proves only who sent an event, so before storing one a node also checks that the agents, projects, messages, holds and resources it would create or change belong to the event's origin node. An event that names another node's object (for example, a registration reusing another node's agent id, mail claiming another node's sender, or a read, ack, transfer or stop for another node's delivery or resource) is rejected, listed in the sync report's rejected events, and is not relayed. Mail to another node's agents and borrowing another node's resource are still allowed, within the permissions below. Whether a borrow needs `borrow_resource` follows the resource the hub knows, not any routing field in the event. A node may not rename itself to a name another known node has, and an address whose node name matches more than one known node is refused as ambiguous (use the node id). The origin chooses an event's time and TTLs, so a receiver takes the time no later than its own clock and clamps lease and hold TTLs to the local limits (24 hours for leases, 4 hours for holds): a skewed clock or a huge TTL cannot keep a foreign lease or hold alive. Each node bounds a lease by its own clock, and a node that syncs later sees the same borrow later. So the node that owns a resource also expires other nodes' leases on it when they run out, and publishes that expiry. Every node that replays the history then ends the lease at the same point, including the lease holder's node, which is always sent the expiry. A hub that ran an earlier build with untrusted spokes may still hold rows that forged events changed before this check existed; those rows are not repaired automatically.
- **Local rules.** A signature also does not prove that the sender's build applied the rules a local command applies, so the receiver applies them again (D98). An event is rejected, listed in the sync report's rejected events and not relayed when it carries: an agent name that is not 1 to 64 ASCII letters, digits, `-`, `_` or `.`; a resource name that is blank, over 200 bytes or holds a control or text-direction character; a resource endpoint over 2 KiB, purpose over 1 KiB, or metadata that is not a JSON object of at most 64 KiB; or a message body that is not text of at most 256 KiB. A rejected event is not sent again and does not hold back later events. A message is never rejected for its subject: an over-long subject is cut to 300 characters and stored as one safe line (D75). Other free text from a peer (a hold's reason, an agent's program, model and description, a resource's endpoint and purpose) is stored as one safe line (D97). An agent whose registration is rejected is not known on the receiving node, so it cannot be addressed there and its mail is not delivered there.
- **A hold's pattern.** A peer's `hold.acquired` or `hold.updated` whose pattern is longer than 4,096 bytes (the limit of a local `another hold acquire`, [HOLDS.md](HOLDS.md#pattern-size-and-a-pattern-that-cannot-be-read)) is not applied: the hold does not exist on the receiving node. Only the length is judged when an event is stored, and nothing is compiled then; a pattern within the limit that turns out not to compile is found when it is compared and warns as a pattern that could not be read. Unlike the rejections above, the event is accepted, stored and relayed, so every node keeps the same log and sync is not held back. Each node that receives it writes an audit row (`another audit`: operation `federation hold pattern`, outcome `refused`, the origin as `node:<id>`, the event id and the reason), and `another sync` prints one line for each such event it pulled, for example `not applied 01M7W8RK0RSEZMKA4CV3YQJJ9D: hold.acquired from node vps (01M7TQ5Y2K8W3N6R4H9D1C7XBF) was stored and relayed but not applied as a hold: its pattern "src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…" (2097156 bytes) is longer than the limit of 4096 bytes`. A hub writes the audit row and a warning in the log of `another serve`; a hub has no `sync` to print the line. The pattern is shown as its first 40 characters and its length, never whole. A pattern with no glob syntax and within the limit is compared as text and is applied whatever it spells.
- **Signed events.** Every node has an Ed25519 key, created on first boot, and signs every event it authors. Relayed events are checked against their origin node's key, so the hub cannot forge another node's events.
- **Hub and spoke.** Spokes **push** the events they authored and **pull** a filtered stream of everyone else's events. Spokes never talk to each other directly. A mesh may come later.
- **Addressing.** Agents on other nodes are `name@project@node`, or `name@@node` for agents without a project. Local addresses (`name`, `name@project`) are unchanged.
- **Node names and ids.** A node name is 1 to 40 ASCII letters, digits, `-`, `_` or `.`. Names are unique whatever their case: with `alpha` known, no other node can pair as, rename itself to, or be listed as `Alpha`. Addresses are still matched exactly as written. `local` is reserved. It means "not named yet": a node named `local` cannot make an invite, join or pair, `another node name local` is refused, and a peer's `node.created` or `node.renamed` event with the name `local` is accepted but does not change that node's name here. A node id is the 26 character ULID made when the home was created; a pairing request, an invite or a hub's node list with anything else in the place of a node id is refused or, in a list, skipped. `ANOTHER_NODE_NAME` names a new home only when the name is valid; otherwise the node starts as `local` and a warning is logged.
- **Pairing.** An invite buys exactly one thing: a new node with the invite's permissions, or new permissions for the node whose key signed the request. It cannot touch any other node and cannot bring back a revoked one.
  - **The proof.** The joiner signs its pairing request with its node key. The signed bytes name the hub (id and key), the invite, the SHA-256 of the invite's secret, the joiner (id, key and name) and the time. One valid signature therefore shows that the sender holds the node's private key, knows the invite, and meant this hub and this invite. The secret itself is never sent, and neither is its hash, so someone who can read or block a pairing request cannot redeem the invite. A captured request cannot be used with another invite, on another hub, or more than 5 minutes later.
  - **The answer.** The hub signs its answer with its node key over the invite, the joiner, the hub's name and the permissions. The joiner checks it against the hub key in the invite and records nothing if it is missing or wrong, so a server at the hub's URL that does not hold the hub's key cannot complete a join. The hub's name in the answer is applied by the rule for a name in a hub's node list: used when it is valid, not `local` and not another node's, otherwise the hub is recorded under its id.
  - **A node the hub already knows.** A known node id that presents a different key is always refused; there is no override. A node row without a key is never given one by pairing. With the same key, a paired spoke may pair again: its permissions become the new invite's, and its name changes only if the new one passes the rename rule. Nothing else changes. A pairing request never clears a revoke on the hub, and never rewrites the row of the node's own hub.
  - **What the joiner checks against its own records.** `another pair join` writes one thing: the `hub` row of the node the invite names, and on a first meeting that node's entry. Before anything is sent, and again before anything is written, the node the invite names as the hub is held to what this node already stores. In each case below nothing is written and the invite is not used.
    - The invite names this node's own id: `this invite names this node itself as its hub; a node cannot join itself`. A node never has a pairing row for itself.
    - This node has a key stored for that id (its hub, or a node it knows from a hub's list) and the invite carries another key: `node <id> presented a different key than before; refusing`.
    - This node has a row for that id with no key: `node <id> is known here without a key; it cannot be joined`. A join never gives a keyless row a key, as pairing on the hub never does.
    - That node is paired here as a spoke, revoked or not: `node <name> is paired here as a spoke; it cannot also be this node's hub`. So a spoke cannot change its own permissions or undo its revoke by handing its hub's owner an invite of its own.
    - This node already syncs with a different hub: ``this node already syncs with another hub; revoke it first: `another pair revoke <hub>` ``. Because this is refused before the request is sent, it does not use up the other hub's invite.

    The one revoke a join clears is this node's own revoke of its hub: a spoke that ran `another pair revoke <hub>` and joins that hub again with a fresh invite syncs with it again, with the new invite's URL and permissions. The invite may give the hub another name, so `pair join` then prints a second line, `this join lifted this node's own revoke of hub <name it was stored under> (key <fingerprint>); it syncs with that hub again`, and the local `peer.paired` event records `revoke_lifted` and `previous_name`.
  - **The hub signs its invites and its answers.** Something that answers in front of the real hub, at an address edited into an invite or on the path of a plain `http://` link, can make a sync fail. It cannot make a spoke apply anything the hub did not sign.
    - **Invites.** The hub signs the whole invite with its node key: the format number, its id, key and name, the address, the invite id, the hash of the secret, the permissions and the expiry. `another pair join` checks that signature against the hub key in the invite before it sends anything. An invite with no signature is refused (`this invite is not signed by its hub: ...`), and so is one in which any field was changed after the hub made it (`this invite's signature does not match its contents: ...`). Nothing is sent and the invite is not used up. So the address a node syncs through is one the hub's owner gave to `another pair invite --url`. That command refuses an address that is not an `http://` or `https://` URL with a host, or that holds white space or a control character (`hub url must be an http:// or https:// URL with a host`); the joiner applies the same rule again before it dials.
    - **Answers to a push and a pull.** A spoke sends a fresh random nonce with every push and pull. The hub signs each `200` answer with its node key over its id, the spoke's id, the nonce, the request's own signature and the SHA-256 of the answer body. The spoke checks that against the hub key it pinned at pairing before it reads one field of the body. An answer without a signature, or with one that is not the hub's for this request and this body, is refused: nothing from it is applied, no cursor moves, a `denied` row goes into the audit log (`another audit`), and `another sync` exits 1 with the reason. An old answer played again, an answer to an earlier request, and an answer meant for another spoke all fail that check.
    - **No unsigned mode.** A spoke applies no unsigned answer. A hub refuses a push or pull that carries no nonce with `426` before anything is stored, with a text that tells the owner of that node to update it. So a hub and a spoke sync only when both run a build that signs (see "Upgrading" below).
    - **What something on the path can still do.** Drop requests and answers: `another sync` then fails and says so, the cursors stay, and the events are sent again next time. `last sync` in `another peers` is the last sync that ran to its end, so a sync that keeps failing shows as stale in `another doctor` after 24 hours on the spoke, and after 7 days on the hub. Read a plain `http://` link: signatures hide nothing. `another pair invite`, `another pair join` and `another doctor` warn when the hub's address is plain `http://` and its host is not `127.0.0.1`, `localhost` or `[::1]`. That is right for a private network and wrong on the open internet; use an `https://` address there. An error answer (anything that is not `200`) is not signed and is only ever a failure: its text is quoted as one line of at most 600 characters, and nothing in it changes state.
    - A second join to the hub this node already has replaces the stored address with the new invite's and the permissions with the ones the hub's signed answer grants. It keeps the cursors.
  - **What is not covered.** A stolen home directory holds the private key, so its holder is that node; revoke it on the hub. A leaked invite code lets its holder join as a new node under any free name; the invite is single use, so the intended join then fails. An invite holder who knows the id of a node that has not paired yet can pair first under that id with its own key; the real node is then refused ("presented a different key") and the hub's audit log records the refusal. No command undoes that: revoke the squatter and give the real machine a new home. Secrecy needs `https://` or a private link: on a plain `http://` link anyone on the path can read mail and the feed in transit, and can make a sync fail; keep the link behind TLS or a private network as described in [REMOTE.md](REMOTE.md). A hub the owner chose is trusted for the directory: spokes take the node list their hub signs. An invite whose hub key was replaced, and which was signed again with that key, is a valid invite to another hub; no signature can show that, only the fingerprint can (see "Set up" below). There is no key rotation.

## What travels where

| Events | Sent to |
|---|---|
| Directory: node, project and agent registrations | every paired node |
| Mail (`message.sent`) | only nodes that host a recipient; mail between agents on one node never leaves it |
| Read and acknowledgement receipts | only the node that sent the message |
| Activity: sessions, requests, holds, resources | nodes with `read_feed`, plus the owner of a resource being borrowed |
| `sync.skipped` and `sync.skip_acked` (see "An event that is too large to send") | the nodes the skipped event would have reached. Through a hub on an earlier build, every spoke with `read_feed` (see "With an earlier build" in that section) |
| Tokens, audit records, pairing records, push records, and transport wake and gone records (which name local terminals) | never |

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

Upgrade a hub and all its spokes together, and make invites after the hub's upgrade: a hub and a spoke sync only when both run a build that signs answers, a hub accepts only signed pairing requests, and a spoke joins only with an invite its hub signed (see "Upgrading" below). Nodes that are already paired stay paired; they do not pair again.

Name each node first; names must be unique across the federation, whatever their case:

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

The invite code is single-use. It carries the hub's URL, node id and public key, and a secret. Treat the code as a password until it is used or expires: whoever holds it can join as a new node. The code is signed by the hub key it names, so its address cannot be changed without that key. If the code came over a channel you do not trust, compare the fingerprint `pair join` prints with `another node show` on the hub before you run `another sync`: a join exchanges nothing but the pairing request, and `another pair revoke <hub>` undoes it.

Give `--url` an `https://` address unless the link to the hub is a private network. For a plain `http://` address that is not loopback, `pair invite` prints the code and then a warning on standard error (with `--json`, in `"warnings"`), and `pair join` does the same on the spoke.

On each spoke:

```bash
another node name laptop
```

```bash
another pair join another-invite:...
```

`pair join` signs the request with this node's key and sends it to the hub. The secret in the invite stays on this machine. The join is recorded only when the hub's answer is signed with the hub key in the invite. A refused request does not use up the invite.

```bash
another sync
```

```bash
another sync --watch --every 30
```

`another peers` lists paired nodes with key fingerprints, permissions and last sync. `another pair revoke <node>` cuts a node off: the hub refuses its next request, and ends that node's leases on the hub's own resources (reason `peer_revoked`), so a revoked node is never promoted to owner. Its agents, holds and leases on other nodes' resources stay until they expire.

A revoked node stays out. An invite does not bring it back, in either direction: its pairing request with an invite from this hub is refused, and `another pair join` on this hub with an invite the revoked node made is refused too. Neither uses up the invite.

- `another pair readmit <node>`, on the hub, lets a revoked spoke sync again with the permissions it had. It prints the node's key fingerprint; check it before relying on the node. Leases ended by the revoke are not restored. To change the node's permissions afterwards, pair it again with a fresh invite.
- `another pair release-name <node>` frees the name a revoked node holds. The node is then listed by its id, and its row, key, revoked state and agents stay. The name is free for any node at once: any paired spoke can take it with a rename, and any invite holder by pairing under it. So first revoke every node you do not trust and have no unused invite outstanding, then release the name, then at once rename or pair the intended node and sync it. On a node that is a hub to others, its spokes see the released node under its id after their next sync. A released node that is later re-admitted is listed by its id until `another node name <name>` is run on it and synced.
- A spoke that revoked its hub joins it again with a fresh invite. That join clears the spoke's own revoke of the hub and changes no other row.

Neither command has an HTTP route or an MCP tool: they are typed on the node itself.

`pair revoke`, `pair readmit` and `pair release-name` act on other nodes only. Given this node's own id or name, when a pairing row for it exists, they refuse: ``node <id> is this node itself; `another pair revoke`, `readmit` and `release-name` act on other nodes only``.

No command of this build makes a pairing row for the node itself. A build before the joiner checked the hub id could write one, from an invite that named the joining node's own id. Such a row is ignored everywhere: it is not this node's hub, `another sync` never dials its address, a join is not refused as a second hub because of it, and no request, push or pull is accepted in its name. `another peers` lists it marked `(this node itself; ignored)`, and `another doctor` warns `a pairing record for this node itself is ignored: an older build wrote it from an invite that named this node; no action is needed`. There is no command to remove it and none is needed.

### Pairing again

A paired, unrevoked spoke may redeem a fresh invite to get other permissions or to follow a hub whose URL changed. Its permissions become the invite's. Its name changes only if it differs and passes the rename rule (valid, not `local`, not another node's). Nothing else changes. `another activity` shows `paired again with node <name> (spoke)`, and the local `peer.paired` event records the invite, the key fingerprint and the previous name and permissions.

### Replacing a machine

| Case | What to do | Pairs again? |
|---|---|---|
| Reinstall the binary, same home | Nothing. | No |
| Move the home to another machine | Copy the home, run `another sync`. Same id and key. | No |
| Rename a node | `another node name <new>` on the node, then `another sync`. | No |
| A spoke's home is lost | Hub: `another pair revoke <old node>`. The new home is a new node: name it and join with a fresh invite. It takes a new name, or the hub runs `another pair release-name <old node>` and the new home takes the old name. | First pairing of a new node |
| The hub's home is lost or rebuilt under the same name | New hub: name it, serve, make an invite per spoke. Each spoke: `another pair revoke <old hub>`, `another pair release-name <old hub>`, `another pair join <code>`. | Every spoke is new to the new hub |

What a lost spoke home costs:

- Without `release-name`, the old name stays on the revoked node. Addresses `x@@oldname` keep resolving to the dead node's agents, and mail sent to them is accepted by the hub and never delivered. The new machine's agents have addresses under the new name.
- With `release-name`, the old node is listed by its id. `x@@oldname` resolves to the new node's agent `x` once that agent is registered and synced, and to nothing before. Mail already sent to the old node's agents stays undelivered; it is not moved to the new node.

Two things you will see after a release, both expected:

- **A rejected rename on the first sync.** The new home pulls the hub's history from zero, and that history holds the old home's own rename to the name. The new home now holds that name, so its first `another sync` prints a line like `rejected <event id>: node.renamed names "laptop", which another node already has`. That is the old node's event being refused, not a failure of the new one. It is not sent again.
- **The released node under an earlier name, until the next sync.** A node that pulls from zero applies the hub's node list first and the history after it. If the released node once signed a rename to a name that no listed node holds now, that old event is applied and the released node is shown under that earlier name, not its id, on that node. The node list of the next `another sync` puts it back under its id. A name that another node holds is never taken this way: the list is applied first, so the old rename to it is rejected. The released node is revoked, so mail to an address under its earlier name is not delivered to anyone.

For a rebuilt hub: if a spoke joins the new hub while the old hub's row still holds the name, the new hub is recorded under its id, `another peers` shows that id as the name, and its agents are addressed `x@@<new hub id>`. After `another pair release-name <old hub>` the next `another sync` applies the new hub's name from its list. A spoke that never knew the old hub sees the name at once.

### A pinned key that is wrong

A node pins another node's key the first time it sees it, and never replaces it. `another sync` can fail with exit code 3 and this text:

```
the hub's signed node list gives node <node id> the key <listed fingerprint>, but this node pinned the key <pinned fingerprint> for it earlier; the answer is refused and this node cannot sync. ...
```

It means this home and its hub hold two different keys for one node. A node's key never changes, so one of the two is wrong. The answer that carried the list is not applied, and the refusal is in the audit log (`another audit`) as `node list names <node id> with key <listed fingerprint>; pinned <pinned fingerprint>`.

- **Which one is wrong.** Run `another node show` on the node the text names and compare its fingerprint with the two in the text.
- **The pinned key is wrong.** This home synced, on a build before the hub signed its answers, through something that answered in the hub's place and listed its own key for that node. The home may also hold events that were accepted under that key and has skipped that node's real ones. There is no repair command: replace the home. Make a new home, name it and join with a fresh invite; on the hub, `another pair revoke <old node>` and, to give the new home the old name, `another pair release-name <old node>` (see "Replacing a machine" above).
- **The hub's key is wrong.** This can only be a hub that is itself a spoke of another hub and whose own home was damaged in the same way: it now signs the wrong key to its spokes. That hub's home is the one to replace.

What a replaced home loses: its node id, and everything that never leaves a node, which is the queue, tokens, OAuth clients and grants, the local pairing history, wake targets and the audit log. What it shared (its agents' registrations, mail and activity) stays on the hub and the other nodes under the old node id.

From this build on, a list reaches a spoke only in an answer its hub signed, so nothing on the path can pin a key. The one remaining way into this state is to join an invite whose hub key was replaced and to sync without comparing the fingerprint.

### Upgrading

**A hub and its spokes are upgraded together.** From this build on the hub signs its answers to a push and a pull, and a spoke applies nothing else. There is no working mix of builds for sync:

| Hub | Spoke | Join | Sync |
|---|---|---|---|
| this build | this build | signed invite required and verified | every answer verified |
| this build | older | works as before | refused by the hub: the spoke prints ``push failed (426 Upgrade Required): this hub signs its answers and serves only nodes that ask for that, and this request did not. Update this node to the hub's build, then run `another sync`. Nothing was stored, nothing is lost, and the node does not need to pair again`` (or `pull failed ...` when it had nothing to push), exit 1. The hub stores nothing and writes a `denied` audit row |
| older | this build | refused before anything is sent: `this invite is not signed by its hub: ...` | refused by the spoke: `the hub at <address> answered the push without a signature; refused, and nothing from that answer was applied. Either the hub runs an older build (update the hub and its spokes together), or ...`, exit 1. If that request was a push, the older hub has already stored the batch; it is sent again after the hub is updated and counts as already there |
| older | older | as before | as before, and open to everything this build closes |

- Upgrade a hub and all its spokes in one sitting. In a chain (a hub that is itself a spoke of another hub), go from the top down. Each link behaves by the table on its own: a node on this build between an older hub and its own spokes refuses the older hub's answers and keeps serving its spokes.
- The order within the sitting does not matter for safety. Whichever end is behind, sync on that link stops with one of the two texts above until it follows, and then resumes from the stored cursors.
- Nothing is lost in between. Events wait in each node's log, and cursors move only on an answer that was applied.
- No node pairs again. Neither refusal touches a pairing, so after both ends are updated `another sync` works with the hub, key and cursors each node already has.
- On each spoke, after upgrading: run `another sync`. For a hub that was joined on a build before invites were signed, `another doctor` then warns `hub <name> was joined before invites were signed, so its address <address> was never checked; ...`, `another sync` prints one `note:` line about it on standard error after each successful sync (with `--watch`, once), and `another peers` marks the hub's row `(joined before invites were signed; address not checked)`. Sync works all the same. The stored address came from an invite nothing had signed: compare it, as `another peers` shows it, with the address the hub's owner gives. To clear the warning, join again: on the hub `another pair invite --url <address> --perms <the spoke's current permissions>`, on the spoke `another pair join <code>`. A new join replaces the spoke's permissions with the invite's, on both sides, so give the invite the permissions `another peers` shows now. It keeps the cursors.

Pairing changed twice before that. First pairing requests were signed, then invites:

| Joiner | Hub | What happens |
|---|---|---|
| this build | this build | Works. |
| a build before signed pairing | this build | The hub answers 403 and the joiner prints `pairing failed (403 Forbidden): this hub accepts only signed pairing requests; upgrade the joining node; an invite made before the hub was upgraded must be made again`. The invite is not used. |
| this build | any, with an invite a build before signed invites made | The joiner refuses before sending anything: `this invite is not signed by its hub: an older build made it, or its signature was removed; not joined. Make a new invite on a hub that runs this build or a newer one`. An invite from before signed pairing does not decode and is refused as `invalid invite code`. |
| this build | a build before signed pairing | An older hub does not accept a signed request (it answers 422) and uses nothing. |

A node that an older build let be named `local` keeps that name wherever it is already recorded, but cannot pair or pair again until it is renamed. Two nodes whose names already differ only in case both keep working; nothing renames them.

### After upgrading: has a node been taken over?

Before this change, the holder of one valid invite could pair as a node the hub already knew: rename it, clear its revoke and replace its permissions, and then take its old name. Nothing proves who sent an old pairing request, but a second pairing and a name that moved are on record on the hub. These queries only read:

- A node paired more than once:

  ```sh
  sqlite3 "$ANOTHER_HOME/another.db" "SELECT json_extract(payload,'$.node_id'), count(*), group_concat(json_extract(payload,'$.display_name'), ' -> ') FROM events WHERE kind = 'peer.paired' AND json_extract(payload,'$.role') = 'spoke' GROUP BY 1 HAVING count(*) > 1"
  ```

  Deliberate second pairings show too. A changed name, or a second pairing after a revoke you did not undo, is the mark.
- A name that moved between nodes (the same name under two node ids):

  ```sh
  sqlite3 "$ANOTHER_HOME/another.db" "SELECT node_id, ts, json_extract(payload,'$.display_name') FROM events WHERE kind IN ('node.created','node.renamed') ORDER BY seq"
  ```
- Which node used each invite: `sqlite3 "$ANOTHER_HOME/another.db" "SELECT invite_id, used_at, used_by FROM invites"`. `another audit` lists every `POST /fed/pair`.
- `another peers`: a node that is not revoked though you revoked it, permissions you did not grant, or a name on the wrong key fingerprint.

To undo it, with commands only:

1. Hub: `another pair revoke <attacker node id>`. Use the id; the names are swapped. Revoke every other node you do not trust as well, and make sure no unused invite is outstanding: a released name is free to any paired node or invite holder at once.
2. Hub: `another pair release-name <attacker node id>`. Do step 3 straight after it, and hand out no invite until the victim holds the name again.
3. Victim: `another sync`, `another node name <its old name>`, `another sync`. If the victim should be revoked, as it was before, run `another pair revoke <victim id>` on the hub instead.
4. The victim's permissions were replaced by the attacker's invite: pair it again with an invite carrying the right ones.
5. Other spokes: `another sync`, twice if the first still shows the old names.
6. Mail sent to the taken name between the attacker's rename and its revoke went to the attacker's node and cannot be recalled. To list it, take `T1`, the `ts` of the attacker's `node.renamed` to that name, and `T2`, the `ts` of its `peer.revoked`, then:

   ```sh
   sqlite3 "$ANOTHER_HOME/another.db" "SELECT event_id, ts, actor FROM events WHERE kind = 'message.sent' AND ts BETWEEN <T1> AND <T2> AND payload LIKE '%<attacker node id>%'"
   ```

From now on every change to a peer's name, permissions or revoked state is on record with its cause: `peer.paired` (with `again`, `invite_id`, `fingerprint`, and on a second pairing `previous_name` and `previous_perms`), `peer.readmitted`, `peer.name_released`, `peer.revoked`, and an audit row for every refused pairing. The hub answers every failure of the invite check with one text, `invalid, used or expired invite, or the request is not signed for it`; the audit row's detail says which it was (`unknown invite`, `used invite (by <node id>)`, `expired invite` or `bad signature`), so an honest retry on a used invite can be told from a forged request.

## Cross-node behaviour

- **Mail**: `another send --from bob --to carol@@runner …`. Replies, threads and acknowledgements work across nodes. Requests stay node-local: open them only with agents on the same node.
- **Merged activity feed**: `another activity` shows other nodes' events prefixed with their node name, e.g. `[hub] alice@shop holds shop:src/**`. Message bodies never appear in the feed.
- **Resources**: `another resources` lists other nodes' resources with their `node` and whether this machine can **reach** them (a TCP connect to the endpoint). They can be borrowed only when reachable and when `borrow_resource` was granted, and the lease is reported back to the owning node. Loopback endpoints are reachable only from the same machine.
- **Path holds**: holds stay node- and repo-local and are never distributed locks. When an agent acquires a hold that overlaps a live hold on another node, in a project with the **same name**, the result carries a **warning** (`warnings` in JSON, `warning:` lines in the CLI), never a conflict. The comparison with other nodes' holds is best effort: it runs after the acquire has been committed, within one fixed budget of work for the command (about 40 ms), lists at most ten overlapping holds and counts the rest, and when the budget is spent says in one line how many holds of which nodes were not compared (`not_compared` in JSON). A peer decides how many holds it sends and how costly their patterns are to compare, so this is what keeps its holds from slowing a command. A hold from another node whose stored pattern cannot be read (a glob that does not compile, or a row over the limit that an earlier build stored) warns on every acquire in that project until it ends, and the warning names the node and the hold. Details and measurements: [HOLDS.md](HOLDS.md#pattern-size-and-a-pattern-that-cannot-be-read).

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

**One event this build writes** is at most 2 MiB (2,097,152 bytes) as it travels: its wire JSON, escaping included, with the sequence number counted as one digit. The largest mail an honest node writes to one recipient is 1,837,191 bytes: a 256 KiB body made only of control characters, which cost seven bytes each on the wire, a 300-character subject and the longest names. This is a rule for local writes only. An event already in a log, or one a peer sends, is not judged by it: it travels if it fits the transport (below). One that does not fit the transport even alone is skipped and reported, and the events after it travel (see "An event that is too large to send").

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
- An event over these sizes is not sent and does not block: the node that holds it steps past it, says so in a small signed event, and sends everything after it (see "An event that is too large to send").

**With a node on an earlier build.**

- *New spoke, older hub.* The older hub reads at most 2 MiB per push and answers `413` to more, without reading the body; the spoke sees that answer or only a failed connection (a broken pipe on Linux, and on macOS sometimes "invalid argument"). On a `413`, or on any I/O error while a push is being sent other than a refused connection, it sends half as many events, down to one, for the rest of that sync. From 500 events that is at most eight cuts, so a network that is simply failing costs at most nine failed requests before its error is reported. The older hub bounds a page by count only; if an answer is over what the spoke reads, the spoke asks again with half the `limit`, down to one event.
- *Older spoke, new hub.* Nothing changes on the spoke. It gains from the hub's 8 MiB push limit and from pages closed at 8 MiB, under the 10 MiB it reads.
- When one event alone is over what the other side takes, halving ends and the sync fails with a message that names the cause and says that retrying will not help, for example `the hub at <url> refused event <id> (message.sent) as too large: ...`. Such an event is not skipped: it fits this build's limits, and the message says to update the other side.

## An event that is too large to send

An event that does not fit the transport even alone is skipped, and the skip is reported until you acknowledge it (D-next). It never blocks the events after it and it is never silent. Nothing is deleted and there is no command that removes an event.

**What is skipped.** In a spoke's log, one of its own events that alone makes a push over the 8 MiB a hub reads (an event of more than 8,388,595 bytes). In a hub's log, an event that alone makes a pull answer, the node directory included, over the 32 MiB a spoke reads. No build from D99 on writes such an event (it writes at most 2 MiB), but an earlier build could, for example by registering an agent with a description of many megabytes through an MCP tool. The node whose log holds the event (the holder) decides, by this build's limits. Everything before and after the event travels in the same sync.

**The marker.** The first time the holder steps past the event it writes one small signed event of its own, `sync.skipped`: the skipped event's id, the node that wrote it, its kind, the agent that wrote it, its size, a short name of the thing (`agent <name>`, `hold <id>`, `<n> holds`, `<n> leases`, `message <id> to <n> recipients`, `request <id>`) and where the event would have gone. It never carries the event's large field. On a spoke it is written in the transaction that moves the push cursor past the event; on a hub, before the pull is answered. There is one marker per holder and event, however often the event is stepped past. The marker travels by the ordinary sync, in the same `another sync`, to the nodes the event would have reached: a marker for a message reaches only the nodes that host a recipient, a marker for a hold the nodes that would have seen the hold. A node may mark its own events; a hub may also mark an event it relays, and a spoke accepts that only from its own hub. Any other marker is refused on import, as is one whose fields are missing, mistyped or over their lengths.

**Where it is reported.** A node reports the unacknowledged skips of its own log and of its hub's log:

- `another sync` prints them after every sync, at most five in full and then `and <n> more skipped event(s): another needs`. The exit status stays 0. `--json` lists the same five at most under `skipped`, oldest first, and gives the number of all of them as `skipped_total`, so a node that writes many markers cannot make this report large. `another needs --json` counts them all and lists up to its row limit (`--limit`, at most 200), and `another activity --kinds sync` lists them, at most 500 per call; page with `--since <time or event id>`. When a sync fails, its error is printed as before and the skips return with the next sync that completes.
- `another needs` has a `skipped_event` row for each ([NEEDS.md](NEEDS.md)). A hub runs no sync, so this is where a hub reports its own. The row gives the same steps in that list's own wording: a step that is a complete command is printed bare (`another sync` on a spoke, the acknowledgement; a hub names no `another sync` step, because it has no hub to dial), and every other step starts with a word and has its commands in backticks.
- The holder writes an audit row (`sync skip`, outcome `skipped`), shown by `another audit`.
- `another activity --kinds sync` lists skips and acknowledgements a node knows, acknowledged or not, at most 500 per call; page with `--since <time or event id>`. Markers written by other spokes show there only; they are not repeated by `another sync` or `another needs`.

For an event in this node's own log, `another sync` prints:

```text
skipped <id> (<kind>, <bytes> bytes, written by <actor>): it is in this node's log and is larger than a hub reads in one push (8388608 bytes), so it was not sent; the events after it were sent
  about: <about>
  missing on other nodes: <what other nodes miss>
  to get it across:
    <step>
    <step>
  then: another sync ack <id> --owner
```

For an event in the hub's log, a spoke prints the same with `it is in node <hub>'s log and is larger than a spoke reads in one pull answer (33554432 bytes), so node <hub> did not send it`, `missing on this node:`, `to get it across, on <this node, or node <name>>:` and `then, on node <hub>: another sync ack <id> --owner`. The steps run on the node that wrote the event; the acknowledgement runs on the holder. Every value a peer wrote is shown as one bounded, safe line.

A value from a marker is put into a printed command only when it is what this software writes there: the agent's name when it is a valid agent name that does not start with `-`, a hold id and the event's id when they are ids this software makes. Anything else leaves the placeholder (`<name>`, `<hold id>`, `<event-id>`), and a `note:` line under `about:` says so, for example `note: the hold id in this report is not an id this software makes, so <hold id> is left for you to fill`. A hub chooses the text of its markers, and these lines are there to be copied.

**Getting the information across.** The steps are fixed per kind. No command re-sends the event itself.

*An agent's registration (`agent.registered`).* Missing: what this registration changed: the agent's description and model. If it was the agent's first registration, other nodes do not know the agent at all: nobody there can address it; mail it sent them is stored and not delivered; they do not see its holds, so an agent there is not warned before it edits the same files; they do not see its leases, so a resource it owns shows no owner there and cannot be borrowed from another node, and a resource it borrowed on another node is not held for it there. Steps:

1. `another agent register <name> --program <program> --model <model> --description "<4096 bytes or fewer>"` (add `--project <path>` if it has one; a model left out is cleared).
2. Each hold it has (`another holds --agent <name>` lists them): `another hold acquire --agent <name> -p <project> <the same paths>`, with `--shared`, `--repo-wide`, `--reason` and `--ttl` as the hold has them. The same hold is sent again; without `-p` from another directory it is a different hold, and without `--shared` a shared hold becomes exclusive or is refused.
3. Each resource of this node it holds a lease on: `another release <resource id or kind:name> --agent <name>`, then at once `another borrow <the same> --agent <name>`. A borrow without the release only renews, and a renewal brings nothing to a node that never had the lease. Between the two commands the resource has no lease unless another agent holds one: another agent on this node that borrows it in that moment becomes its owner, and if the resource is exclusive this agent's borrow is then refused. If another agent already holds a lease, that agent becomes the owner at the release and this agent comes back as a borrower.
4. Each resource of another node it borrowed: `another release <resource id> --agent <name>`, then `another borrow <resource id> --agent <name>`. If the borrow is refused, the release has already happened: it is refused when the hub does not grant borrowing, when the resource has no live owner, or when it cannot be reached from this machine.
5. Mail it sent to other nodes before step 1 is not delivered there and no command delivers it: send it again.
6. `another sync`.

*A hold (`hold.acquired`, `hold.updated`).* Missing: this hold: an agent on another node is not warned before it edits the same files, and renewals of it change nothing there. Steps: `another hold release --agent <agent> <hold id>`; `another hold acquire --agent <agent> -p <project> <paths>`, with `--shared`, `--repo-wide` and `--ttl` as the hold had them, each pattern 4096 bytes or fewer and `--reason` 1024 bytes or fewer; `another sync`.

*A renewal of holds (`hold.renewed`).* Missing: this renewal: other nodes may show these holds as expired, and not warn their agents, until the next one arrives. Steps: `another heartbeat --agent <agent>`, then `another sync`. The next heartbeat does the same by itself, in events of at most 10000 ids.

*A renewal of leases (`resource.renewed`).* Missing: this renewal. A lease on this node's own resource shows as expired on other nodes until the next renewal arrives. A lease borrowed on another node's resource is different: if that node saw no renewal before the lease ran out there, it ended the lease, on every node including this one, and a later renewal does not bring it back. Steps: `another sync`; for each resource of another node the agent borrowed, `another borrow <resource id> --agent <agent>`, which renews a lease that is still live and borrows again one that ended; `another sync`.

*A message (`message.sent`).* Missing: this message: its recipients on other nodes never received it. Steps: `another send` again to fewer recipients in each message (a body is at most 262144 bytes, so only the list of recipients makes a message this large); `another sync`.

*Any other kind.* No command re-sends the event. For most kinds nothing needs doing (a release runs out on other nodes by itself, a request only loses a feed line); if other nodes need what it recorded, do that action again.

**How a resource is named in these steps.** `kind:name` names only a resource of the node the command runs on. Another node's resource is named by its resource id, the value `another resources` prints in brackets at the end of each line; `another borrow port:<name>` for another node's resource answers `no registered resource "port:<name>"`.

**Acknowledging.** On the holder:

```bash
another sync ack <event-id> --owner
```

Acknowledging needs `--owner`, a terminal and a typed yes, as the queue's owner actions do. It records that the report was seen; it deletes nothing. One event per call. The id is looked up before anything is asked: an id no marker names is refused, an event in another node's log is refused with the node to run the command on, and an event already acknowledged is said to be so. It writes one `sync.skip_acked` event, which travels where the marker went, and one audit row (`sync ack`, outcome `ok`). `another sync` and `another needs` then stop reporting that skip, on the holder at once and on its spokes when the acknowledgement reaches them. The skipped event, the marker and the audit rows stay, and the event is still not sent. It needs no hub and works on a hub. It has no MCP tool and no remote route.

Past skips are listed by `another activity --kinds sync`, acknowledged or not, and by `another audit` on the node that holds the event. `another events` shows the two events themselves.

After a hub is restored from a copy older than its marker, it writes a new marker at the next pull, and if the copy also predates the acknowledgement it reports the skip again on the hub. Spokes that kept the old acknowledgement stay quiet.

### What other nodes miss

While an event is skipped, a node that would have received it treats later events as it treats any event about something it does not know: it stores them and applies nothing.

| Skipped event | On the other nodes | Later events affected |
|---|---|---|
| `agent.registered`, first registration | The agent is unknown | Its mail is stored and not delivered, its holds and leases are not shown, until it registers again. Mail sent before that stays undelivered |
| `agent.registered`, a later one | Description and model are older | None |
| `project.ensured` | The project is unknown | The node's agents there have no project in their address and their holds in it are not shown. No build from D99 on can write this event too large |
| `node.created`, `node.renamed` | The name is older until the next sync's node list | None |
| `message.sent` | The message is absent | A reply to it arrives without its parent |
| `delivery.read`, `delivery.acked` | The sender does not see the read or the acknowledgement | None |
| `hold.acquired`, `hold.updated` | The hold is not seen | Its renewals and its release change nothing there |
| `hold.renewed` | The holds look expired early | None; the next renewal corrects it |
| `hold.released` | The holds look held until they run out there, at most 4 hours after the last renewal seen | None |
| `resource.registered` | The resource is unknown | Adopting and borrowing it create no lease there. No build from D99 on can write this event too large |
| `resource.adopted`, `resource.borrowed`, `resource.transferred` | The lease or its role is absent | Its renewals and its release change nothing there |
| `resource.renewed` | The leases look expired early | For a borrow on another node's resource, that node ends the lease when it runs out there, on every node; borrow again. For a lease on the holder's own resource, the next renewal corrects it |
| `resource.released` | The leases look held until they run out there, at most 24 hours after the last renewal seen | None |
| `session.*` and the other `hold.*` and `resource.*` kinds | Presence is older, or a feed line is absent | None |
| `request.*` | A feed line is absent | None |

`push.*` and `transport.*` kinds are node-local operational evidence and are never synced. Other nodes never see them, so they do not participate in skip reporting.

### With an earlier build

The wire format, the cursors and the anchors are unchanged. A skip is visible to another node only as the two event kinds.

| Pairing | Holder | What happens |
|---|---|---|
| new spoke, new hub | spoke | The spoke skips, its marker is pushed and relayed where the event would have gone, and it is reported as above |
| new spoke, new hub | hub | The hub skips, the spoke pulls the marker and reports the hub's skip |
| new spoke, older hub | spoke | The spoke skips. The older hub stores the marker and relays it as an activity event to every spoke with `read_feed`, and reports nothing beyond its own feed line |
| new spoke, older hub | hub | The older hub does not skip, and what the spoke prints depends on the hub's build. Either way `another sync` exits non-zero on every run and nothing after that event arrives: update the hub, and it then steps past the event and reports it as above. A hub on a build with D99 and without this change names the event and answers 409; the spoke prints `pull failed (409 Conflict): event <id> (<kind>, <bytes> bytes on the wire) in the hub's log is larger than a spoke reads in one pull answer (33554432 bytes), so it cannot be sent and the events after it wait behind it; retrying will not help; the owner must decide whether it is skipped or keeps blocking (docs/FEDERATION.md, "If a sync reports an event that cannot be sent")`. That heading was this section's earlier name; the decision it speaks of is made, and no event needs removing by hand. A hub on a build before D99 sends an answer over what the spoke reads; the spoke halves down to one event and then fails with `... the hub runs an older build or holds an event over the size limit; a hub on this build or later steps past such an event and reports it, so update the hub` |
| older spoke, new hub | hub | The hub skips for the older spoke too. The older spoke stores the marker as an unknown kind and its feed shows `[hub] system: sync.skipped`. An event between 10 MiB and 32 MiB is not skipped and still blocks that older spoke, which reads 10 MiB: update the spoke |
| older spoke, new hub | spoke | The older spoke blocks on its own event as it always did. Update it, then it skips |
| new spoke, older hub, an event of 2 MiB to 8 MiB | spoke | Not skipped: the event fits this build's limits. The sync fails with the message that says to update the hub |

Because an older hub relays both kinds as activity events, a marker for a message then shows every spoke with `read_feed` this, and nothing of the content: that a message existed, its message id and the number of its recipients; the node ids that host its recipients; the sender and the sender's node; the event's id, its size and the time the sync skipped it. Update the hub to keep a marker for mail to the recipients' nodes only.

## Wire protocol (for implementers)

- An invite code is `another-invite:` followed by the unpadded URL-safe base64 of the JSON `{v, hub_url, hub: {node_id, display_name, public_key}, invite_id, secret, perms, expires_at, sig}`. `v` is `2`. `invite_id` is a ULID, like a node id; a joiner refuses a code with anything else there as `invalid invite code`, before it signs. The hub stores only the lowercase hex SHA-256 of the `secret` text.
  - `sig` is the hub's Ed25519 signature, checked with the strict rules against `hub.public_key`, over these UTF-8 fields joined by one `\n`, with no trailing newline:

    ```
    another-invite-v1
    {v as decimal}
    {hub node_id}
    {hub public_key}
    {hub display_name}
    {hub_url}
    {invite_id}
    {sha256_hex(secret)}
    {perms, joined by ","}
    {expires_at as decimal}
    ```

    The joiner checks it right after the code decodes and before any other field is judged, in this order: no `sig` is refused as not signed; a line break in the hub's id, key or name, the address or the invite id is `invalid invite code`; a signature that does not verify is refused as not matching the contents; then a `v` other than `2`, the expiry and the shape of each field. A field the joiner does not know is not signed and is ignored.
- `POST /fed/pair`: `{invite_id, node: {node_id, display_name, public_key}, time_ms, sig}` returns `{hub: {node_id, display_name, public_key}, perms, sig}`. The request holds no secret.
  - The request's `sig` is the joiner's Ed25519 signature, checked with the strict rules against `node.public_key`, over these UTF-8 fields joined by one `\n`, with no trailing newline:

    ```
    another-pair-v1
    {hub node_id}
    {hub public_key}
    {invite_id}
    {sha256_hex(secret)}
    {joiner node_id}
    {joiner public_key}
    {joiner display_name}
    {time_ms as decimal}
    ```

    The joiner takes the hub's id and key from the invite as text. The hub builds the same bytes from its own id and key and the hash it stored for the invite, never from the request.
  - The answer's `sig` is the hub's signature, checked the same way against the hub key in the invite, over:

    ```
    another-pair-accept-v1
    {invite_id}
    {sha256_hex(secret)}
    {joiner node_id}
    {joiner public_key}
    {hub display_name as sent in the answer}
    {perms as sent in the answer, joined by ","}
    ```
  - The hub checks, in this order and before it writes anything: the node id is a ULID; the name is valid and not `local`; the key decodes, is not a weak key and is in its canonical base64 form; `sig` is present (403 otherwise); `time_ms` is within 5 minutes of the hub's clock (403); the invite exists, is unused and unexpired and the signature verifies (403, one text for all four); then the rules for a node it already knows (409 or 403). A body that is not a pairing request is answered 400 `invalid pairing request`, and nothing from the body is logged or audited.
- `POST /fed/push`: `{events: [WireEvent]}` returns `{accepted, duplicates, rejected}`. A body over the hub's limit is answered `413`; send fewer events.
- `GET /fed/pull?after=<hub seq>&limit=N[&after_id=<event id>[&after_id_seq=<hub seq>]][&pushed_id=<event id>]` returns `{events, cursor, more, nodes, reset}`. A page holds at most `limit` events (1 to 2000, default 500) and may hold fewer, with `more: true`, when the hub closes it by size. `after_id` is the last event the spoke received from this hub and `after_id_seq` the hub sequence number it came at; `pushed_id` is the last of the spoke's own events this hub accepted. `reset: true`, with no events, means the hub's log no longer reaches `after`, no longer holds `after_id` (at `after_id_seq`, when given), or no longer holds `pushed_id`; see "Restarts, restores and offline spokes" above. Hubs without `reset` omit it and ignore these parameters, and spokes without it ignore it.
- Push and pull requests carry these headers:
  - `x-another-node: <node id>`
  - `x-another-time: <ms>`
  - `x-another-signature: <Ed25519 over "another-request-v1\n{method}\n{path?query}\n{time}\n{sha256(body)}">`

  - `x-another-nonce: <16 random bytes as 32 lowercase hex characters>`, new for every request, a retry included. It is not part of the request signature.

  The hub rejects requests from unpaired or revoked nodes, and requests whose clock is more than 5 minutes off (`401`). After that check, and before anything is stored or read from its log, it answers a request with no `x-another-nonce` with `426`, the header `Upgrade: another-signed-sync/1` and an `invalid` error, and one whose nonce is not 32 lowercase hex characters with `400`.
- Every `200` answer to a push or a pull carries `x-another-signature`: the hub's Ed25519 signature, checked by the spoke with the strict rules against the hub key it pinned at pairing, over these fields joined by one `\n`:

  ```
  another-answer-v1
  {hub node_id}
  {spoke node_id}
  {the request's x-another-nonce}
  {the request's x-another-signature, as text}
  {sha256_hex(answer body)}
  ```

  The body is the answer as the hub serialized it, before any content encoding, and the hub sends exactly the bytes it hashed. The spoke hashes the body after content decoding, so a proxy that compresses the answer with gzip still verifies, and one that rewrites the JSON does not. The spoke verifies before it parses the body. Error answers are not signed.
- A `WireEvent` is `{seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig}`, where `payload` is the exact signed JSON text. The signature covers `"another-event-v1\n{event_id}\n{node_id}\n{ts}\n{actor}\n{kind}\n{sha256(payload)}"`.
- Skipping an event that is too large to send changes nothing on the wire: no field, route, cursor or anchor rule. A skip is visible only as two ordinary event kinds. `sync.skipped` has the payload `{event_id, event_node, event_kind, event_actor, bytes, direction, about, route, route_nodes}` (`about` may be absent; `direction` is `push` or `pull`; `route` is `directory`, `feed` or `mail`; `route_nodes` holds at most 1,000 node ids), and `sync.skip_acked` has `{event_id, route, route_nodes}`. A receiver checks no order and no chain of events, and `prev_id` is not signed, so a stream with an event left out is accepted as before.

## Limits (v2)

- Mail bodies are readable by the hub that relays them. Host the hub yourself; end-to-end encryption is future work.
- Hub-and-spoke only, with no mesh. A spoke syncs with one hub.
- Requests and path holds are node-local.
- A spoke of a node that is itself a spoke refuses that node's hub's `sync.skipped` markers for other nodes' events: it takes a marker for another node's event only from its own hub. Each such marker is listed once in the sync's rejected events and is not stored.
- Mail a skipped agent sent to other nodes before it registered again is not delivered there (see "An event that is too large to send"). The query behind the skip report is not bounded against a flood of markers from a paired node.
