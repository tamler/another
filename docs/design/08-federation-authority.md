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
mistake their own pairing grants for the borrower's grants. Exclusive, released
or lapsed foreign resources take no new borrower lease. Liveness is evaluated at
the receiver's clock. Borrowers never clear a resource's lapse clock or resurrect
an expired foreign borrower lease by renewal.

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
