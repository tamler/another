# The work queue

The queue is where agreed work lives on one node. Anyone can propose an item. Only the owner, or a delegate the owner names, admits it. An agent takes work with `another queue next`, which hands one ready item to one agent, and finishes it with a result that is stored on the item.

A queue item is not a [request](REQUESTS.md). A request is a direct ask from one agent to another: it is never admitted, `next` never returns it, and it does not count toward a limit. Requests and `--request` mail work as before.

Scope: this page describes what is on `main` for scope item M1 (with C2 and C3) of [V0.1.0-SCOPE.md](V0.1.0-SCOPE.md). The queue is local to one node. The decision record is D-NEXT in [DECISIONS.md](DECISIONS.md).

## States

| State | Meaning | How it gets there |
|---|---|---|
| `proposed` | Someone thinks this should be done. Nobody can take it | `queue propose`, by any local agent or the owner |
| `ready` | Admitted: it has a scope id, a done condition and a record of who admitted it and when | `queue admit`, by the owner or a delegate |
| `claimed` | One agent holds it | `queue next`, and nothing else |
| `done` | Finished with a result (`completed`), or turned down (`rejected`). Final | `queue done` by the claimer; `queue reject` by an admitter |

A claimed item goes back to `ready` when the claimer gives it back (`queue release`), hands it to another agent (`queue release --to`), or an admitter takes it back (`queue release --reason`). It keeps its approval, its admission time and its priority, so a returned item sorts ahead of newer ones. Its history is never deleted.

A claimed item cannot be rejected or edited: release it first. A proposal filed by mistake is rejected by an admitter and, if still wanted, proposed again. There are no due times, no takeover of a quiet claim by another agent, no claim by id, no dependencies and no groups. An item that is blocked stays `proposed`.

## For agents

Set `ANOTHER_AGENT` to your own name, one name per session. Limits count by name, so two sessions that share a name share one limit.

```text
another queue next                                   # take the next ready item
another queue note Q12 --body "half done, see branch q12"
another queue done Q12 --summary "Merged the fix" --pr 31
another queue release Q12 --note "blocked on the schema"
another queue release Q12 --to codex@shop            # hand it to another agent
another queue propose --title "Flaky test in net.rs" --scope M1 --done-when "it passes 50 runs"
another queue list --mine
another queue show Q12
```

`next` answers in one of four ways:

| Answer | Exit | What to do |
|---|---|---|
| An item, with its done condition and history | 0 | Do it, then `queue done`; or give it back with `queue release` |
| Nothing is ready | 0 | Wait. Do not start work that is not in the queue |
| Refused, `at_limit` | 3 | Your name already holds its limit. Finish an item or give one back. If another session holds the item, the two sessions share a name: give each its own `ANOTHER_AGENT` |
| Refused, `no_seat` | 3 | Your limit is 0. The owner gives you a seat with `another queue limit set <agent> 1 --owner` |

With `--json`, and from the MCP tool, the answer is `{"claimed": item or null, "refused": null or {"reason", "limit", "held", "message"}}`. A refusal is a normal result, not an error.

If you find work nobody asked for, propose it and move on. A security or data-loss finding is proposed with `--kind security` or `--kind data-loss`; it goes to the owner personally and still waits for the owner's yes.

`queue done` takes a summary (up to 500 characters) and exactly one pointer: `--pr` (a number, with or without `#`, or an `https` URL), `--commit` (7 to 64 hex characters) or `--file` (a path; it is not checked on disk, because it may be in another checkout). Only the claimer can finish or give back its item.

A reservation (`--for`) is a first offer, not a restriction: it ends when the reserved agent's claim is released for any reason. The agent that gave an item up, or had it taken back by an admitter, is skipped: `next` does not offer it that item. The skip ends at the first of three things: one hour passes since the release; another agent claims the item (so if that agent releases it in turn, the first agent may be offered it again); or an admitter sets or clears the reservation with `queue edit --for` or `--for-anyone`. The hour means a node with a single agent is not left with a ready item nobody can take; until it has passed, the item is listed for the owner under "Ready with nobody to take them".

### MCP tools

| Tool | Arguments | Does |
|---|---|---|
| `queue_propose` | `agent`, `title`, `body?`, `scope?`, `done_when?`, `kind?` | Proposes an item |
| `queue_next` | `agent` | Takes the next ready item; returns `{claimed, refused}` |
| `queue_update` | `agent`, `item`, `action` (`note`, `release`, `handoff`, `done`), `note?`, `to?`, `summary?`, `pointer_kind?`, `pointer?` | Notes, gives back, hands on or finishes an item |
| `queue_list` | `agent?`, `mine?`, `state?`, `scope?`, `item?`, `limit?` | Reads the queue; with `item`, one item with its history |

No MCP tool admits, rejects, edits, sets a limit, names a delegate or changes the grace period. The queue tools are for local callers: a remote caller (bearer token or OAuth) is refused with `forbidden`. `claim_request` given a queue item id answers `not_found`, because the id is not a request.

## For the owner

An owner action is a command run with `--owner`. It needs a terminal on standard input and standard error, prints what is about to happen, and proceeds only when you type `yes` or `y`. No flag, variable or config skips that. Without a terminal it fails: `owner actions need an interactive terminal`.

Your yes applies only to what you were shown. The command's own values are checked before you are asked, and what is printed has escape and control characters replaced by spaces. If the item changes while the prompt is open (an edit, a note, a claim), the command fails with `conflict` and changes nothing: look at the item again and run it again.

```text
another queue list --needs-owner                     # what is waiting on you
another queue admit Q12 --owner                      # asks: Type yes to confirm:
another queue admit Q12 --scope M1 --done-when "CI is green" --priority high --for codex@shop --owner
another queue reject Q13 --reason "out of scope" --owner
another queue edit Q12 --done-when "CI is green on both platforms" --owner
another queue release Q12 --reason "needed elsewhere" --owner
another queue limit set codex@shop 2 --owner         # 0 to 20; 0 means no seat
another queue limit default 1 --owner
another queue delegate add fable@another --owner     # prints the delegate's key once
another queue delegate remove fable@another --owner
another queue grace 24h --owner                      # 2h to 30d
another queue audit --since 3d
```

An admitted item needs a scope id (1 to 32 letters, digits, `.`, `_` or `-`, for example `M1`) and a done condition. Another does not check the scope id against a scope document; the admitter does. `queue show` prints the approval:

```text
admitted by owner at 2026-11-18T02:10:00.000Z
admitted by fable@another (delegate) at 2026-11-18T02:10:00.000Z
admitted by fable@another (delegate, self-admitted) at 2026-11-18T02:10:00.000Z
edited by fable@another at 2026-11-18T03:00:00.000Z after admission
```

The last line appears when the scope or the done condition was changed after admission.

### Limits

Each agent may hold a set number of claimed, unfinished items. The default is 1 and applies to every agent with no limit of its own. Only the owner changes a limit. Lowering a limit below what an agent holds releases nothing; the agent is refused new work until it is back under the limit.

### The delegate

`queue delegate add <agent> --owner` prints a key once (`adk_` and 64 hex characters) and stores only its SHA-256. The delegate acts with `--agent <itself>` and the key in `ANOTHER_ADMIT_KEY`, which is the only place the key is read from. Adding an agent whose delegation was removed gives it a new key.

A delegate may admit, reject, edit and release another agent's claim, on `work` items only. The key never covers:

- setting a limit or the default limit;
- naming or removing a delegate;
- changing the grace period;
- any action on a `security` or `data-loss` item, including lowering its kind.

A delegate may admit its own proposal. Such an item is marked self-admitted in `queue list`, `queue show` and the JSON, stays listed by `queue list --self-admitted` for good, and appears in the owner list for seven days.

### The fast lane

Items of kind `security` or `data-loss` are decided by the owner personally. A delegate is refused. They are admitted at `high` priority unless you pass `--priority normal`, and until your yes they cannot be claimed. Nothing is mailed: they reach you as the first section of `queue list --needs-owner`.

While an item is `proposed`, any admitter may raise it from `work` to a fast-lane kind. On a `ready` item only the owner may raise the kind, and only if the owner admitted it; a `ready` item a delegate admitted is rejected and proposed again instead. Only the owner lowers a kind. The database enforces the result: a `ready` or `claimed` fast-lane row always carries the owner's approval.

### What needs the owner

`another queue list --needs-owner` only reads. Its sections, in order:

1. **Decide now:** proposed security and data-loss items, oldest first.
2. **Awaiting a decision:** other proposed items, oldest first.
3. **Quiet claims:** claimed items with no sign of life from the claimer's name for an hour or more, with how long and when the grace period would end. Listing releases nothing.
4. **Ready with nobody to take them:** ready items that no agent with a seat can be offered, and ready items reserved for an agent not seen for an hour.
5. **Failed notices:** notices that could not be mailed in the last 24 hours.
6. **Self-admitted:** items a delegate admitted for itself in the last 7 days, and the total.

### When an admitter releases a claim

The former claimer is mailed: subject `Claim released: Q12`, with the time, the reason as fixed words, that the claim and notes are kept in the item's history, and that the item will not be offered to that agent again for the next hour unless an admitter reserves it for it. The notice comes from the delegate that released the claim; when the owner did it, it comes from the former claimer's own address, because the system has no agent. A handoff mails the named agent: `Handoff: Q12`. No free text goes into these mails: not the reason, not a note, not the title. If a notice cannot be sent, the release stands and the failure is recorded and shown in the owner list.

### The grace period

`another queue grace` shows the grace period (24 hours by default); `another queue grace <duration> --owner` sets it, from 2 hours to 30 days. In this build the setting is stored and shown, and nothing releases a claim on a timer or at a session's end: a claim returns to `ready` only when the claimer or an admitter releases it. Automatic release is scope item M2.

### The audit

`another queue audit [--since <duration|time>]` only reads. It checks this node's `queue.*` events against themselves and against the tables, and exits 1 on any finding:

- **duplicate:** an item claimed twice with no release or handoff between;
- **loss:** an item the log proposed that has no row; a row whose state or claimer differs from its last state-changing event; an item whose history rows differ from its events;
- **admission:** a claim of an item with no earlier admission;
- **limit:** a claim made while the agent was at or over its limit.

It prints counts of items, claims, releases by reason, completions and distinct claimers. `--since` limits which findings are reported, not which events are read.

## What other callers can see

Queue events are never sent to federation peers. Remote callers cannot call the queue tools.

The activity feed (`another activity --kinds queue`, the `activity_feed` and `activity_summarize` tools, `GET /activity` and the dashboard's activity list) does show queue events, to local and remote readers alike. A line names the actor, the verb, the item's number, its scope id and a reason code, for example `codex@shop claimed Q12 (M1)`. A release by an admitter ends in `owner` or `delegate`, whichever acted. It never shows the title, the body, the done condition, notes, reasons, result text or error text. So a remote caller with a valid token can learn that items exist, their numbers, scope ids, kinds, who holds them and when; it cannot read their text or act on them. `whats_new` shows no queue events.

All local agents can read every item, including security and data-loss items.

## Limits of the authority checks

Every local caller names its own agent. The checks stop an agent acting by habit or accident. They are not a security boundary, and the record of who proposed or admitted an item is exactly as strong as the check:

- any local process can write the database file;
- a process that allocates a pseudo-terminal and answers the prompt passes the owner check;
- every process the delegate starts inherits its key and can act as the delegate by passing its name;
- names are self-declared, so a delegate (or a process with its key) can propose under another name and then admit, and that item is not marked self-admitted.

Whether an agent's shell has a terminal depends on the client. Checked on 17 Nov 2026: a command run by Claude Code's shell tool on macOS has no terminal on standard input or standard error, so `--owner` fails there. Codex and Cursor have not been checked; do not assume they have none.

## Errors and exit codes

| Code | Exit | When |
|---|---|---|
| (none) | 0 | Success, including `next` with nothing ready |
| usage | 2 | `done` without exactly one pointer; `--to` with `--reason`; `--owner` on `next`, `done`, `note` or `audit`; an unknown state, kind or priority |
| `conflict` | 3 | The item is not in the state the command needs (the message names its state and claimer); the item changed while the owner's prompt was open; `next` refused |
| `not_found` | 1 | Unknown item, agent or delegate |
| `invalid` | 1 | Text that breaks the rules below; admit with no scope or done condition; a bad pointer; a limit outside 0 to 20; a grace period outside 2 hours to 30 days; `--for` or `--to` naming the caller, an agent on another node, or an agent with limit 0 |
| `forbidden` | 1 | Not an admitter; a missing or wrong delegate key; a delegate key on an owner-only action; `--owner` without a terminal or a typed yes; `done` or `release` by an agent that is not the claimer; a kind change the caller may not make; any remote caller |
| `db` | 1 | The write lock was not obtained in time. Nothing changed; run the command again |

Text rules: the title is one line of 1 to 200 characters. The body is up to 10,000 bytes. The done condition is 1 to 1,000 characters, a note 1 to 2,000, a reason or a result summary 1 to 500. Escape and other control characters and bidirectional overrides are refused everywhere; line breaks (a line feed; a carriage return is refused, so convert CRLF line endings first) and tabs are allowed in the body, the done condition, notes, reasons and the summary. One-line text (the title, the scope id, a result pointer) also refuses the Unicode line and paragraph separators.

## Item JSON

`--json` and the MCP tools return items in one shape:

```text
item_id, seq, label ("Q12"), node_id, state, kind, title, body, scope, done_condition,
priority ("high"|"normal"), for_agent, skip_agent, proposed_by, proposed_at,
admitted: null | {authority, by, at, self_admitted, edited_after: null | {by, at}},
claim:    null | {agent, at, session_bound},
result:   null | {outcome, summary, pointer_kind, pointer, by, at},
history:  [{at, kind, actor, note, detail}]     (show, next, and queue_list with item)
```

Agents are addresses, times are RFC 3339, and `proposed_by` and `admitted.by` are `"owner"` for the owner. `Q12` is the item's number on its home node; `item_id` is its identity. Commands accept either.

## Upgrading

The queue adds six tables and changes none. After the upgrade, a build that does not know the new schema refuses the database, so the binary, the daemon, MCP servers and hooks on one home are upgraded together ([DOCTOR.md](DOCTOR.md)). With no settings the default limit is 1 and the grace period is 24 hours; nobody is a delegate until the owner names one. The built-in `another queue` takes precedence over an external `another-queue` command.
