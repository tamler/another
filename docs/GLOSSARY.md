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

## Work queue

- **Queue item**: a piece of agreed work on one node, shown as `Q12`. It is `proposed`, `ready`, `claimed` or `done`. It is not a request: a request is a direct ask between agents and is never admitted. [QUEUE.md](QUEUE.md).
- **Propose**: file an item with `another queue propose` or the `queue_propose` tool. Any local agent may. A proposed item cannot be taken.
- **Admit**, **admitter**: move a proposed item to `ready`, giving it a scope id and a done condition and recording who admitted it and when. The admitter is the owner, or a delegate.
- **The owner (of the queue)**: the person the node belongs to, acting with `--owner` at a terminal and a typed yes. Not the same word as the owner of a request.
- **Delegate**: an agent the owner named to admit, reject, edit and release `work` items, acting with its key in `ANOTHER_ADMIT_KEY`. **Self-admitted** marks an item a delegate admitted after proposing it itself.
- **Scope id**: the id of the scope item the work belongs to, such as `M1`. The admitter checks it against the scope document; Another does not.
- **Done condition**: what must be true for an item to be finished, set by the time it is admitted.
- **Fast lane**: items of kind `security` or `data-loss`. Only the owner decides them, and they are first in the owner list.
- **`next`**: `another queue next` or the `queue_next` tool. It hands one ready item to one agent, and is the only way an item becomes claimed.
- **Limit**, **seat**: how many claimed, unfinished items one agent name may hold, set by the owner; 1 by default. A limit of 0 is no seat.
- **Reservation**, **skip**: an item reserved with `--for` is offered to that agent first; the agent that gave an item up is skipped. Both end as [QUEUE.md](QUEUE.md) describes.
- **Handoff**: the claimer gives its item to a named agent with `another queue release --to`. The item returns to `ready`, reserved for that agent.
- **Result**: the short summary and one pointer (pull request, commit or file) stored on an item by `another queue done`.
- **Owner list**: `another queue list --needs-owner`: decisions waiting, quiet claims, ready items nobody can take, failed notices and self-admitted items.
- **Grace period**: the owner-set length of silence after which a claim may be released automatically (24 hours by default). The setting exists; automatic release is scope item M2.
- **Queue audit**: `another queue audit`, a read-only check of the queue's events against themselves and the tables for duplicated, lost, unadmitted or over-limit claims.

## Coordination

- **Path hold**: an expiring claim on project-relative files or globs, with a TTL. "I am changing these files." An exclusive hold conflicts with any overlapping hold of another agent; a shared hold conflicts only with exclusive ones. A hold blocks only inside the checkout (working directory) it was taken in; other worktrees and clones of the repository are separate (D92). [HOLDS.md](HOLDS.md).
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
- **Sweep**: the pass that records expired holds, marks overdue requests stale and expires lapsed leases. `another sweep` runs it once; `another serve` runs it every 30 seconds; `another requests` and `another inbox` run it before they list. The **queue sweep** is a separate pass that releases queue claims with no sign of life for the grace period; it runs in `another sweep`, in `another serve` and in other agents' hooks, heartbeats and `queue next`, and never before a read ([QUEUE.md](QUEUE.md)).
- **Transport**: a tool that runs agent sessions, reached through one interface with three operations: `wake`, `status` and `list`. This build has the Herdr transport. [WAKE.md](WAKE.md).
- **Wake**: one line delivered into an idle agent's session through a transport, with an answer on whether it was delivered and whether the agent started working. The daemon wakes idle Herdr sessions that have unannounced mail; `another wake` does it on demand, also for tmux and cmux panes. Hooks are the fallback everywhere. [WAKE.md](WAKE.md), [DAEMON.md](DAEMON.md).
- **Session row**: what Another knows about one agent session inside a transport: where it runs, its state, its last wake, and whether it needs the owner. `another transport status`. [WAKE.md](WAKE.md).
- **Stuck**: shown for a session whose last wake was delivered but did not start work. It is not typed into again until it is seen working, one of its hooks runs, or the owner clears it. [WAKE.md](WAKE.md).

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
