# Glossary

Short definitions of the words Another uses, with the places that explain each one in full. The site has a shorter version at [toanother.one/docs/concepts/](https://toanother.one/docs/concepts/). Three pairs are easy to confuse and get their own sections at the end: an acknowledgement versus request ownership, a path hold versus a resource lease, and what expiry means for each.

## Identity

- **Node**: one data directory with its own SQLite database and signing key, `~/.another` by default or `$ANOTHER_HOME`. Sessions share state by sharing a node. [README](../README.md#concepts).
- **Project**: a registered directory with a short slug, usually the directory name. Two checkouts of one repository are two projects; a second directory with the same name gets a numbered slug such as `shop-2`. [ADDRESSING.md](ADDRESSING.md).
- **Agent**: a named identity that sends and receives, such as `codex@shop`. Names are unique per project. Another records the identity; it does not start or run the program behind it.
- **Address**: `name@project`. A bare `name` resolves only while one agent on the node has it. `name@project@node` reaches an agent on a paired node. [ADDRESSING.md](ADDRESSING.md).

## Mail

- **Message**: an immutable note with a subject, a body and recipients. Each recipient has its own read and acknowledgement state. Fetching an inbox changes neither.
- **Thread**: a conversation of messages. A reply stays in the thread of the message it answers.
- **Acknowledgement (ack)**: a recorded receipt, made with `another ack <message-id>` or the `acknowledge` tool, by one agent for itself. It says the mail was seen. It says nothing about the work. [REQUESTS.md](REQUESTS.md).
- **Request**: a claimable piece of work opened alongside a message with `--request` or `--due`. It has at most one owner and a status: `open`, `claimed`, `done`, `stale` or `cancelled`. Requests stay on one node (D24). [REQUESTS.md](REQUESTS.md).
- **Owner**: the agent that claimed a request. Only the owner can release or complete a claimed request; only the creator can cancel it.
- **Due time**: an optional deadline set in seconds with `--due`. Past it, an open or claimed request becomes `stale` on its own and anyone may claim it, including from its owner. A request without a due time never goes stale by itself; only a human confirming Expire in the local dashboard can mark it stale (D42).

## Coordination

- **Path hold**: an expiring claim on project-relative files or globs, with a TTL. "I am changing these files." An exclusive hold conflicts with any overlapping hold of another agent; a shared hold conflicts only with exclusive ones. Conflicts are checked across every checkout of the same repository on this machine (D48). [HOLDS.md](HOLDS.md).
- **Edit guard**: the `pre-edit` hook, installed with `another hook install` for Claude Code, Codex CLI and Grok CLI. Before an edit through one of those clients' edit tools it takes a narrow hold on that file for the editing agent, or stops the edit and names the holder when another agent holds it. Cursor has no edit guard (D46), and a write made from a shell command is not checked in any client. The guard stops only a conflict it actually found. With no hook installed, or a Codex hook not yet trusted, nothing runs and the edit goes ahead unrecorded. When the hook runs but cannot check (no database, a lock, an old schema) it lets the edit through and tries to append a line to the hooks log; that logging is best effort (D47). [HOOKS.md](HOOKS.md).
- **Resource**: a registered model server, dev server, port, GPU, container or long job, addressed by id or `kind:name`. Another lists it so a second agent borrows it instead of starting a duplicate. [RESOURCES.md](RESOURCES.md).
- **Lease**: a time-limited claim on a resource, held as **owner** (the agent that registered it) or **borrower** (shared resources only). `refcount` is the number of live leases. If the owner releases while others still borrow, the oldest borrower becomes owner (D10). [RESOURCES.md](RESOURCES.md).
- **Orphan**: a cleanup candidate. Either a registered resource with no live lease (it is `lapsed`, still listed, and adopted by the next register or borrow, D9) or a listening process that nothing registered. A listed process may be something you want running.
- **Reap**: stopping an orphan with `another reap <target> --confirm`, on the machine that owns it, after a grace period (default 10 minutes). Without `--confirm` it prints a plan. The MCP tool can only request a reap; a remote caller is refused; nothing reaps automatically (D8, D17). [RESOURCES.md](RESOURCES.md).

## Awareness

- **Hook**: a user-global entry in a coding client's config that runs Another at session start, on each prompt, after each tool call and at session end, and before each file edit in Claude Code, Codex CLI and Grok CLI (Cursor has no edit hook). [HOOKS.md](HOOKS.md).
- **Digest**: the short inbox and activity summary a hook gives a session. Never message bodies.
- **Activity feed**: who did what: sessions, mail subjects, requests, holds and resources. `another activity`.
- **Event log**: the signed, append-only record behind everything. Query tables are projections of it, and it is what federation syncs. [SPEC.md](SPEC.md).
- **Sweep**: the pass that records expired holds, marks overdue requests stale and expires lapsed leases. `another sweep` runs it once; `another serve` runs it every 30 seconds; `another requests` and `another inbox` run it before they list.
- **Wake**: an optional nudge typed into an idle agent's terminal through tmux, cmux or Herdr. [WAKE.md](WAKE.md), [DAEMON.md](DAEMON.md).

## Other machines

- **Token**: a bearer secret that lets one remote client act as one agent within chosen projects. Every call made with it is audited. [REMOTE.md](REMOTE.md).
- **Federation**, **hub**, **spoke**: several nodes exchanging signed events through a hub you host. Each node stays authoritative for its own agents, holds and resources. The hub can read the mail it relays. [FEDERATION.md](FEDERATION.md).

## Acknowledgement versus ownership

| | Acknowledgement | Request ownership |
|---|---|---|
| Recorded by | `another ack <message-id>` | `another request claim <request-id>` |
| Means | This agent saw the message | This agent is doing the work |
| Scope | One agent, one message | One request, at most one owner |
| Ends with | Nothing; it is a fact about receipt | `request complete` (mails the creator a `Done:` message), `request release`, or `request cancel` by the creator |
| If the agent stops | The ack stays true | The request stays `claimed`. It goes `stale`, and anyone may take it over, once its due time passes or a human confirms Expire in the local dashboard (D42). Without a due time nothing else moves it: the owner releases it or the creator cancels it |

Reading or acknowledging a message never claims its request. A worked example with recorded output is in [REQUESTS.md](REQUESTS.md).

## Path hold versus resource lease

| | Path hold | Resource lease |
|---|---|---|
| Claims | Files or globs in a project | A running thing: server, port, GPU, container, job |
| Taken with | `another hold acquire`, or the edit guard | `another resource register` (owner) or `another borrow` (borrower) |
| Conflict | Exclusive overlaps exit with status 3 and grant nothing | `--exclusive` resources refuse borrowers; shared ones count leases |
| Enforced by | The edit guard, for edits through the Claude Code, Codex CLI or Grok CLI edit tools, when the hook is installed (and, for Codex, trusted) and can reach the database. A conflict it cannot check lets the edit through. Nothing locks the file on disk; a Cursor edit or a shell write is not checked | Nothing. A lease is a record; another process can still bind the port |
| Across machines | A clash with another node's hold is a warning (D26) | Foreign leases count until their TTL; stop or reap only on the owning node (D25) |
| Session end | Released by the session-end hook, always (D11) | Released too, unless `another heartbeat --end --keep` |

Holds are advisory outside the edit guard. If you need a boundary a shell command cannot cross, use a separate checkout.

## What expiry means

A path hold and a resource lease always carry a TTL and lapse on their own. A request lapses on its own only when it was given a due time. One without a due time goes stale only if a human confirms Expire in the local dashboard (D42). What happens next differs.

| | Default TTL | Range | Renewed by | On expiry |
|---|---|---|---|---|
| Path hold | 30 minutes | 1 minute to 4 hours | Session start, each prompt, and the post-tool hook once a minute for holds past half their TTL; `another hold renew` | Stops conflicting at once. The next sweep records it as expired. Nobody is told; the holder finds out at its next check |
| Request due time | None | Positive seconds within the supported timestamp range; zero, negative and overflowing values are refused | A claim with `--due`; a takeover without one clears a past-due deadline | Status becomes `stale` at the next sweep or listing. The owner keeps the request until someone claims it away. Without a due time nothing expires by itself; only the dashboard's confirmed Expire marks it stale |
| Resource lease | 30 minutes | 1 minute to 24 hours (D13) | `another heartbeat`, the session hooks, `another resource heartbeat` | The lease ends. With no live lease the resource is `lapsed`: still listed, an orphan candidate, adopted by the next register or borrow. Only a human can reap it |

Automatic expiry is silent in all three cases. There is no notification to the holder, the owner or the people waiting; the state is visible to whoever looks next, through `another holds`, `another requests`, `another orphans`, the hook digest, or the dashboard.
