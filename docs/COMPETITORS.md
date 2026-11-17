# Competitors

Status: reviewed by the project owner, 17 Nov 2026. Read-only review from public pages, repositories and directory listings retrieved on 17 Nov 2026. Nobody installed or ran these tools, so every statement about them is what their own material says, not observed behaviour. "Unverified" marks a point the public material does not settle. Scope ids (M1 to M10, N1 to N5) refer to [V0.1.0-SCOPE.md](V0.1.0-SCOPE.md); principles refer to [VISION.md](VISION.md).

Another's column describes `main` today. Where a capability is in the v0.1.0 scope but not built, the cell says so.

## The three tools

### Zipline (zipline.li)

**What it does.** One conversation shared by ChatGPT, Claude, Grok and other MCP assistants. You tag an assistant to hand it the next step with the whole conversation; it does its part in its own app. Files attached once reach every connected assistant. An inbox shows what is new, what is waiting and what needs your decision; the site says "Decisions always wait for you".

**How.** A hosted MCP server (Streamable HTTP, OAuth). The Glama listing shows 14 tools, all about messages, inbox, files, projects and participants: `post_message`, `get_inbox`, `acknowledge_inbox`, `list_messages`, `upload_file`, `register_wakeup` and others. Claude and Grok pick up tagged work in the background; other assistants check in when asked. Data is stored in Ireland. A TypeScript SDK is in preview. Access is by early-access sign-up.

**Unverified.** A "decision record" as a distinct feature: the public pages show decisions waiting in an inbox, and no tool, schema or log for recorded decisions. Pricing, licence and any self-hosted option are not published. ChatGPT background pickup being limited to Business, Enterprise and Edu plans comes from a directory listing only.

**Against Another.** Zipline is for chat assistants handing a conversation around. It has no file holds, no claimable queue, no resource registry and nothing local: it trails on everything in the core job (M1, M2, M4). It beats Another where Another has frozen its effort: getting hosted ChatGPT, Claude.ai and Grok into one thread with one approval step. Another has an OAuth server for that, and it is frozen. Zipline's inbox of "what needs your decision" matches what M7 will be.

### Polyroom (polyroom.ai, formerly agent-chat by superint)

**What it does.** A hosted chat where people and agents share rooms: they post, reply, debate in turns and review each other. An agent can be asked to split work into cards on a board and assign them. Assigned agents can ask for approval on a card. Each room sets who may approve decisions (people only by default, agents only, or both), and decisions are recorded with who approved them. Agents can publish versioned artifacts such as plans and reports.

**How.** A hosted MCP server. Claude Code and Codex join through plugins (MIT, `superint-ai/polyroom-plugins`); ChatGPT, Claude.ai and Grok through connectors; other MCP clients with a URL and token. The Claude Code plugin wakes an idle session with no terminal multiplexer: a `Stop` hook marked `asyncRewake` waits up to 30 minutes and wakes the session when the agent is mentioned, messaged or assigned a card. The session must be put into listening mode each time it opens. Free tier: 3 people, 3 agents, 5,000 messages a month, 30 days of history. Team: 15 US dollars per person per month. The server and database run in Brazil.

**Unverified.** A dedicated "manager agent": the site describes asking any agent to plan and assign. How an unassigned card is claimed, and whether two agents can take the same card, is not described. Whether the `asyncRewake` wake is reliable in practice. No self-hosted option, file locks or multi-machine behaviour are mentioned.

**Against Another.** Polyroom is the closest to Another's queue idea, and it is ahead on two things Another has only scoped: cards on a board that agents are woken for (M1, M5), and recorded approvals with a named approver (M1 admission, M2). Its wake needs no Herdr. It trails on principle 7: it is hosted, account-based, priced per person and built for teams, which VISION lists as not for now. It has no path holds (M4), no resource registry, no local database and no stated guarantee that a card is handed to exactly one agent.

### KittyClaw (github.com/Ekioo/KittyClaw)

**What it does.** A self-hosted kanban board that runs agents. Tickets move through columns; a column can own a processor (mission, prompt, model, skills, routing) that launches an agent CLI on the ticket. The owner watches runs live, can steer or stop them, and validates before release. Supports Claude Code, Codex, Grok Build and Mistral Vibe.

**How.** A .NET 10 and Blazor app on localhost with one SQLite database per project, a REST API, and an embedded MCP server (73 tools, off by default). AGPL-3.0. Windows installer, or run from source. Version v0.29.1 on 1 Nov 2026, with several releases a week through late October. Each ticket's work happens in its own git worktree. Durable writes go through a router that serializes their integration into the target branch, with recovery for interrupted writes, late commits and conflicts. Columns can have an enforced ticket limit, applied atomically for concurrent arrivals. A `concurrencyGroup` on agent runs groups runs, for example per assignee. It sends one anonymous daily heartbeat.

**Unverified.** "A shared resource gets its own queue" as a general feature: the public material shows a queue for one shared resource, the target branch, plus column limits and concurrency groups. What `concurrencyGroup` enforces is not explained. No multi-machine support is described.

**Against Another.** KittyClaw owns the sessions: it launches the agents. That is a non-goal for Another (principle 4), so the two are different shapes rather than rivals on every row. Where they overlap, KittyClaw beats Another today on the durable record of work (tickets, run snapshots, an activity timeline: Another's M2 is not built), on recovery from interrupted work, and on merging: a serialized integration queue is the thing Another's own development lacked when it stacked 100 pull requests. It avoids file collisions by isolation (a worktree per ticket), where Another uses holds. It trails on agents the owner starts by hand in their own terminals, which it does not coordinate; on cross-machine use; on platform (Windows first, where Another is macOS and Linux); and on principle 7 (it has telemetry, though anonymous, and its AGPL licence is stricter than Apache-2.0).

## Capability table

"Yes" means the tool's public material states it. "No" means the material describes a design that excludes it. "Not stated" means the material is silent.

| Capability | Another on `main` | In v0.1.0 scope | Zipline | Polyroom | KittyClaw |
|---|---|---|---|---|---|
| Messages between agents | Yes: mail, threads | as is | Yes: shared thread | Yes: rooms | Yes: ticket comments |
| Work items agents take | Requests, one recipient | M1: queue | No: tagging only | Yes: cards | Yes: tickets |
| Exactly one agent gets an item | Yes for a request claim | M1: atomic `next` | Not applicable | Not stated | Not stated |
| Owner admits work before it is claimable | No | M1 | Not applicable | Partly: approval on a card | Yes: owner moves the ticket to Todo |
| Recorded approvals, with approver | No | M1, M2 | Unverified | Yes | Yes: human validation before release |
| Result stored on the item | No | M2 | Not applicable | Yes: artifacts | Yes: run snapshots |
| Recovery after a crashed session | Stale requests can be taken over | M2 | Not stated | Not stated | Yes: durable retries, worktree recovery |
| File conflict prevention | Yes: holds at the edit, across worktrees | M4 | No | No | By isolation: worktree per ticket |
| Serialized merge into a shared branch | No | not in scope | No | No | Yes |
| Limit on work in progress | No | not in scope | No | Not stated | Yes: column capacity |
| Wake an idle agent | Herdr, tmux, cmux; delivery only | M5: confirmed start | Claude and Grok background pickup | Yes: hook wait, no multiplexer | It launches the agent itself |
| Know an agent is blocked or stuck | No | M5, M7 | Not stated | Not stated | Yes: live run view, Blocked column |
| One list of what needs the owner | No | M7 | Yes: inbox | Partly: approvals | Yes: board and badges |
| Shared ports, servers, GPUs | Yes: registry and leases | frozen, as is | No | No | No |
| Several machines | Yes: self-hosted hub and spoke | M9; shared claims in v0.2.0 | Hosted, so anywhere | Hosted, so anywhere | Not stated |
| Hosted chat apps (ChatGPT, Claude.ai, Grok) | Yes: OAuth connectors | frozen | Yes, the main use | Yes | No |
| Local first, no account | Yes | principle 7 | No | No | Yes |
| Self-hosted | Yes | principle 7 | Not stated | Not stated | Yes |
| Telemetry | None | principle 7 | Not stated | Not stated | Daily anonymous heartbeat |
| Licence | Apache-2.0 | | Not stated | Plugins MIT; service closed | AGPL-3.0 |
| Launches or owns sessions | No | non-goal | No | No | Yes |
| Published release | None | M10 | Early access | Live, with pricing | v0.29.1 |

## Our honest gaps

1. **No queue.** The core of the vision is not on `main`. Polyroom has cards and KittyClaw has tickets today. Until M1 ships, Another's answer is a request addressed to one agent, or a markdown file.
2. **Wake is not confirmed.** Another records that a wake was sent, not that the agent started. Panes sat at a limit menu overnight while Another counted them as woken. KittyClaw sees its runs because it owns them; M5 has to get the same knowledge through a transport.
3. **Wake depends on a multiplexer.** An idle Claude Code session with no Herdr, tmux or cmux is not woken at all. Polyroom wakes one with a hook alone.
4. **No record of results.** Claims are events, but results live in lane files outside Another (M2).
5. **Upgrade safety failed in practice.** On 17 Nov 2026 a development build migrated the live database and broke holds (M3). None of the three publishes a comparable failure, which proves nothing about them, but we cannot claim safety until M3 is done.
6. **Nothing limits work in progress or orders merges.** Another's own development produced about 100 open pull requests. KittyClaw has column limits and a serialized integration queue.
7. **Never released.** There is no tag and no archive (M10). KittyClaw is at v0.29.1; Polyroom takes payment.
8. **Hosted chat apps are frozen.** Zipline and Polyroom make ChatGPT, Claude.ai and Grok first-class. Another's connectors exist and will not be improved in v0.1.0. That is a choice, and it is a gap for anyone whose agents are chat apps.
9. **Holds stop at the edit tool.** A shell command that writes a file gets past them. KittyClaw's worktree per ticket does not have this hole, at the cost of merge work later.

## Claims we can back today

Each is on `main` and covered by tests or by the owner's daily use. None needs a comparison to make.

- An edit to a file another agent holds is stopped at the edit, with the holder named, for Claude Code and Codex, across worktrees and clones on one machine. None of the three tools stops an edit.
- One local SQLite database, no account, no telemetry, no network call you did not configure. Zipline and Polyroom are hosted; KittyClaw sends a heartbeat.
- Agents you started yourself, in your own terminals, coordinate without Another launching or owning them.
- A registry of running servers and ports with leases, so an agent borrows a model server instead of starting a second one. None of the three has this.
- Machines you own sync through a hub you host, with every event signed by its origin node.
- A request has one owner once claimed, and a stale one can be taken over.
- Apache-2.0, every feature in the open repository.

Claims we cannot make yet: a shared work queue, confirmed wakes, "nothing is lost across an upgrade", or anything that needs a published release.

## What we take from them

The plan does not change to match these tools feature for feature. Zipline and Polyroom are hosted and built around chat apps and teams; KittyClaw owns the sessions it runs. Those are non-goals in VISION.md. The plan changes only where one of them is ahead on Another's own core job: handing out agreed work, recording it, and waking the agent that should do it.

The owner decided each item on 17 Nov 2026.

| # | Item | Scope | Decision | Why | Done when |
|---|---|---|---|---|---|
| C1 | Written test: wake an idle Claude Code session from a waiting `Stop` hook, with no transport. Check the client's hook documentation first; nothing is copied from Polyroom's plugin. | M6, feeding M5 | Admitted, as a written test only | Closes gap 3 and would give the hook pack a wake of its own. Unverified that the hook behaves as Polyroom's plugin implies. | A written result: does it wake the session, what does it cost while waiting, and does Codex have an equivalent. No product code. |
| C2 | Record who admitted each queue item and when, and show owner decisions that are waiting in the M7 list. | M1, M7 | Admitted into v0.1.0 | Polyroom records approvals with the approver; Zipline and Polyroom both lead with "what needs your decision". | An admitted item shows its approver and time; pending decisions appear in the one list. |
| C3 | A limit on claimed-but-unfinished items per agent. | M1 | Admitted into v0.1.0 | Closes part of gap 6. A limit of this kind would have capped the 100 pull requests. | `next` refuses with a clear message when the agent is at its limit; the limit is set by the owner. |
| C4 | A waiting line for an exclusive shared resource, starting with the merge to `main`: one merge at a time, in order, with the next in line told when it is free. | post-v0.1.0 | Waits until after v0.1.0 | KittyClaw's serialized integration queue. Closes the rest of gap 6. | Not specified yet; needs a spec. |

## Sources

- Zipline: <https://zipline.li>, <https://glama.ai/mcp/connectors/li.zipline/zipline>, <https://mcpservers.org/servers/zipline-li> (search summary only; the page refused a direct fetch)
- Polyroom: <https://polyroom.ai/en/>, <https://github.com/superint-ai/polyroom-plugins>
- KittyClaw: <https://github.com/Ekioo/KittyClaw> (README and CHANGELOG.md), <https://ekioo.com/en/blog/kittyclaw-from-kanban-to-harness> (search summary only)
