# Federation origin authority

This implements the existing per-node authority contract in FEDERATION.md and
D20–D28/D44. An event signature establishes its origin; payload object IDs do not
establish that origin's right to change an object.

Before storing an incoming event, verify its signature and check whether it is an
already-stored duplicate. Validate new state-changing events against stored object
owners, using the signed origin for direct pushes and hub relays alike. Report
authority violations as individual rejections, leave them out of the relay log,
and continue the batch. Database failures remain errors, not permanent rejections.
Unknown event kinds keep their existing no-projection behavior.

Agents, projects, messages, holds, lanes and resource registrations cannot collide
with another node's IDs. Acting senders, session agents, receipt agents, lane
creators/leads and decision askers belong to their authoring node. Recipients and
resource borrowers can belong to other nodes. Proposals preserve the existing
permission to report status on another node's lane, but cannot impersonate its
lead. A carrier message can be read or marked read only when its origin and sender
match its proposer or answer relayer.

Resource ownership follows the resource row. Lease ownership follows the lease
row. Only the resource's node can stop it, adopt it or promote a borrower; a
promotion can legitimately select a remote borrower's lease. A spoke needs
borrow_resource for a foreign target, as determined from stored resource ownership,
and its routing hint must identify that owner. Receivers of hub relays do not
mistake their own pairing grants for the borrower's grants.

Whether a borrow or renewal was in time is decided once, by the fixed hub, at its
own clock, when the borrower pushes it. The hub refuses an exclusive, released or
leaseless target, and a renewal naming a lease already expired there, before the
event is stored, so it is never relayed. A renewal is refused whole, so borrowers
emit one event per lease on another node's resource. Everything the hub relays is
replayed without a clock check, including by a resource owner that was offline:
one history gives the same leases on every node, whatever the delivery delay.
Ownership checks still run on relays. Stop and reap stay with the resource's node.

A relayed borrow or renewal can reach the owner after it lapsed the resource,
because the owner hadn't heard of the lease when its own one ended:

- If a lease is then live at the owner's clock, the lapse no longer holds. The owner
  clears it and promotes as usual, with its own signed `resource.transferred`.
- If the delivered lease has already expired there, the row is recorded and nothing
  else changes: the resource stays lapsed and its lapse clock keeps running.
- A stopped or reaped resource stays that way. A later borrow takes no lease, and the
  stop ends the lease everywhere once it reaches the hub.

The owner decides promotion from what is live at its clock when it settles, so two
different delivery timings can promote different borrowers. Each outcome is one
signed event that every node then follows.

This trusts the hub for timing, as D20 already does for its node directory. A
compromised or older hub can make owners accept false leases until the owner stops
the resource. A borrower whose renewal is refused keeps its local lease until its
local TTL; the only signal is the rejection in the push report.

Signed rename events must obey the existing node-name syntax and uniqueness rules.
Historical node.created events do not rename a directory-known node. The paired
hub's directory retains its existing D20 trust and key-pinning behavior; a spoke
cannot supply that directory. No new distributed naming policy is introduced.

SQL guards supplement pre-log validation. No schema, public API, dependency or
permission is added. This is prospective enforcement: it does not repair rows or
erase malicious events accepted by an older version.

Verification uses the supplied store and real-loopback push reproductions changed
to assert rejection, plus positive multi-node mail, receipt, resource, lane and
decision flows. It includes mixed batches, duplicate retries after state changes,
ID collisions, relay exclusion, permitted borrowing, lapse clocks and cursor
advancement. Cross-family plan and final review accompany the workspace tests,
formatting, Clippy and Rust 1.88 checks. This does not establish cross-machine or
production acceptance.
