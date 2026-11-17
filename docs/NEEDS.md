# What needs the owner

`another needs` is the one list of what will not move without you: decisions that wait, agent sessions that are blocked, stuck or unreachable, claims that have gone quiet, and ready work nobody can take. Each row prints its next step. It is scope item M7 of [v0.1.0](V0.1.0-SCOPE.md), with C2; the decision is D94 in [DECISIONS.md](DECISIONS.md).

```bash
another needs                 # the list
another needs --brief         # one line, for a shell prompt or a status line
another needs --json          # the same rows as data, for a coordinating agent
another needs --limit 50      # rows shown per kind (1 to 200, default 10)
```

"What is waiting on me" is the sections `Decide now` and `Proposals`. "What is stuck" is `Agents` and `Claims`.

## Who reads it

| Reader | When | What they do next |
|---|---|---|
| You, at a terminal | Start of day, or back after an hour | Work down the list. Each row shows its age |
| You, when something feels stuck | Any time | Go to the pane the row names, or release a claim whose session is gone |
| You, at a glance | All day | `--brief` in a prompt or a Herdr status line; run the full list when it is not empty |
| You, on a phone | Away from the desk | The same text over ssh. Nothing goes through the hub in v0.1.0 |
| A coordinating agent acting for you | At its own turn | Reads `--json`, runs what a delegate may, reports the rest |

## It only reads

The command opens the home the way `another doctor` does. It creates no home, migrates no database, runs no sweep, writes no event, takes no hold, sends no mail, wakes nobody, calls no transport and runs no program. It reads the database, `build.json` and `transports.json`. Like any reader of the database, it makes SQLite's two side files beside it when they are not there: `another.db-wal`, which stays empty (0 bytes), and `another.db-shm`, the 32 KB index every reader shares. On a home another process has open, the `-shm` file's bytes can change. No data is written: the database file and the contents of the `-wal` file are the same before and after.

It is local only. No MCP tool, HTTP route or dashboard panel returns the list, so a federation peer, the hub, a bearer token and an OAuth grant see exactly what they saw before.

The printed commands are not an authority check. They still need `--owner` at a terminal, or a delegate's key.

## Exit codes

| Exit | When | Standard output |
|---|---|---|
| 0 | The list was read, with rows or without | The list. `--brief` with nothing waiting prints nothing |
| 1 | It could not be read: no database, a schema this build would refuse or would have to migrate (after an upgrade, until a normal command has run), or the database stayed locked for 2 seconds | Nothing. The reason and `run another doctor` go to standard error. With `--brief`, standard output gets exactly `needs: cannot read, run another doctor`, with or without `--json`. Unlike other commands, `--json` prints no error object here |
| 2 | A bad command line, including `--limit` out of range | |

Rows never change the exit code. A script tests `--brief` for empty output and exit 0. A list that cannot be read is never an empty output, so a blank prompt always means nothing is waiting.

## The list

```text
$ another needs
7 need the owner (mac, 2026-11-17T16:05:18.244Z)

Decide now (1)
  Q8 security  Token printed in debug output  proposed by codex@shop 3h ago
    next: another queue show Q8
          another queue admit Q8 --owner
          another queue reject Q8 --reason "<why>" --owner

Agents (3)
  blocked      claude@shop 7f3a..  herdr w1:p2  12m  claims Q1
    next: answer the dialog in herdr pane w1:p2
  stuck        codex@shop -  herdr w1:p3  25m  last wake: delivered, did not start  name also holds Q2 (not bound)
    next: in herdr pane w1:p3, clear the input box, then send the agent any prompt
  unreachable  claude@api 91c0..  herdr w2:p1  gone 2h  claims Q3
    next: another queue show Q3
          another queue release Q3 --reason "session gone" --owner

Claims (2)
  Q4 M4  Hold fixes  claimed by codex@api, quiet for 95 min, not bound to a session, grace ends 2026-11-18T14:30:17.484Z
    next: another queue show Q4
          ask codex@api whether a session is working on Q4: another send --from <your agent> --to codex@api --subject "Who holds Q4?"
          another queue release Q4 --reason "<why>" --owner
  Q7 M9  Hub backup restore  ready, reserved for claude@vps, which cannot take it now
    next: another queue edit Q7 --for-anyone --owner

Proposals (1)
  Q9 work  Flaky test in net.rs  proposed by claude@shop 40m ago
    next: another queue show Q9
          another queue admit Q9 --owner
          another queue reject Q9 --reason "<why>" --owner

4 sessions gone with nothing waiting: another wake prune --dry-run
1 notice could not be sent in the last 24 hours (Q1 to claude@shop): another queue show Q1
2 self-admitted in the last 7 days (Q6, Q5), 2 in all: another queue list --self-admitted
```

The first line is the count, the node's name and the time, or `nothing needs the owner`. A section with no rows is not printed. Inside a kind the oldest row comes first. A kind with more rows than `--limit` ends with `and 14 more:` and the command that lists them; the counts are never cut. The lines at the bottom are the closing lines, described below.

The one-line form prints counts and fixed words only, so nothing an agent wrote reaches a prompt:

```text
$ another needs --brief
needs you: 7 (1 decide now, 1 blocked, 1 stuck, 1 unreachable, 1 quiet claim, 1 no taker, 1 proposal)
```

Only kinds above zero are named, in the order of the sections. `checks_stopped` reads `checks stopped` and `transport_config` reads `transports.json refused`.

## Sections and kinds

| Section | Kinds, in order |
|---|---|
| `Another` | `checks_stopped`, `transport_config` |
| `Decide now` | `decide_now` |
| `Agents` | `blocked`, `stuck`, `unreachable`, `transport_silent` |
| `Claims` | `quiet_claim`, `no_taker` |
| `Proposals` | `proposal` |

A counted row exists only for a condition that stops agreed work or waits on a decision, and every kind names what clears it.

### Another

| Kind | Raised | Cleared | Next |
|---|---|---|---|
| `checks_stopped` | Herdr sessions are live (a hook of each ran in the last 10 minutes) and none of them was checked in the last 5 minutes | A check reaches any live Herdr session, or every such session's hooks stop for 10 minutes | `another daemon status` |
| `transport_config` | `transports.json` was refused | The file loads | `another doctor` |

`checks_stopped` means the rows under `Agents` may be out of date. The daemon checks every 30 seconds, so the row can show for up to that long when the first Herdr session on a node starts or the machine wakes from sleep. A server started with `--no-wake` or `--auth` runs no checks. A node whose sessions are all on command transports gets no such row: whether those can be listed depends on your config.

### Decide now and Proposals

| Kind | Raised | Cleared | Who |
|---|---|---|---|
| `decide_now` | A proposed item of kind `security` or `data-loss` | Admitted or rejected | The owner only |
| `proposal` | A proposed item of kind `work` | Admitted or rejected | The owner, or a delegate |

Next: `another queue show`, then `another queue admit` or `another queue reject`. A proposed item is the one way a decision reaches this list: mail and requests are not read.

### Agents

Each row is one session, not one agent name. It is taken from the reason the daemon stored for that session ([WAKE.md](WAKE.md#what-you-see)); a hook does not recompute it, so an agent row can lag the truth by one check, 30 seconds.

| Kind | Raised | Cleared | Next |
|---|---|---|---|
| `blocked` | The agent waits on a person at a dialog | The transport sees another state | Answer the dialog in the pane the row names |
| `stuck` | The last wake did not start work | The session is seen working or blocked, or one of its hooks runs | In that pane, clear the input box, then send the agent any prompt |
| `unreachable`, `gone` | The transport has not listed the session for 60 seconds, and the session holds a bound claim or its agent has mail no channel has announced | The claim is released or finished, the mail is announced, the session is listed again, or its row is deleted | `another queue show` for each bound claim, then the release or the wait (below). With mail only: `another inbox --agent`, on one row per agent; further gone sessions of that agent that hold no claim are counted in the closing line |
| `unreachable`, `unknown` | The transport sees a session it cannot identify for 10 minutes | It is seen in another state, or pruned after a day without hooks | Look at the pane: it may hold another session |
| `unreachable`, `undelivered` | Three wakes in a row could not be delivered | A wake is delivered | `another doctor`, `another transport status` |
| `transport_silent` | A transport has not answered for 10 minutes. One row per transport, however many sessions | The transport answers | `another doctor`; for Herdr also `herdr agent list` |

What a row shows of the queue:

- `claims Q1`: the claimed items bound to this session.
- `name also holds Q2 (not bound)`: claimed items of the agent's name that are bound to no session. No step is ever printed for them on an agent row: they may belong to a working sibling.
- `as of <time>`: the row was not checked in the last 5 minutes.

**No step on an agent row acts on the agent name.** On a machine that runs several sessions under one name, `another wake clear --agent` deletes every session row of the name, and `another wake --agent` picks whichever session ran a hook last. Neither is printed here. A blocked or stuck session is cleared in its own pane.

**A stuck session.** Sending any prompt in the pane runs the session's hook, which lifts the stop and announces its mail, and the row leaves the list at once. `another transport status --attention` keeps showing the row until the next check.

**The release on a gone session** is printed only when all four hold: no hook of that session has run since it went gone; it has been gone for 15 minutes; a listing Another trusts had seen it live; and the row was checked in the last 5 minutes. One missed listing is not enough: after a Herdr restart every live session can read `gone` for a while. Otherwise the step is `wait: the session may come back`. The release needs `--owner`; the former claimer is mailed.

### Claims

**`quiet_claim`.** A claimed item is listed for the first of these reasons that holds:

| Reason | When | Row says |
|---|---|---|
| `name_quiet` | Nothing was heard from the claimer's name for an hour or more | `quiet for <n> min` |
| `session_quiet` | The name is active, the claim is bound to a session, and that session has been quiet for an hour or more | `name active, its session quiet for <n> min` |
| `old_unbound` | The name is active, the claim is bound to no session, and the claim is at least one grace period old (24 hours unless you set it) | `name active, claimed <age> ago` |

Next steps:

- Always `another queue show`, then the ask step: mail the claimer and ask which session holds the item.
- For `name_quiet` only, a third step, `another queue release <Q> --reason "<why>" --owner`, when what is stored licenses it: the name has no session row a hook made, or every such row was checked in the last 5 minutes and is `idle` or `gone`. Any other state means ask only: `working` and `blocked` are a session at work, and `unknown`, `present` and `no_answer` may be one. A row last checked longer ago licenses nothing, whatever state it shows: with checks stopped, a session seen idle two hours ago may be inside one long tool call. A row that only holds the outcome of a wake by hand is not a session and is not counted.
- For the other two reasons there is never a release.

The list prints no command that wakes an agent, for any row. (The closing line's `another wake prune --dry-run` only reads: it asks no transport and types nothing.) Whether a pane still holds the agent is for the wake command to decide when it runs, and nothing the list reads can say it: on a transport that cannot tell who is in a pane, a wake line printed here could type into another agent's session. If you want to wake a quiet agent, look at its pane first and run `another wake` yourself.

With a command transport that cannot list its sessions, the rows of a name are never checked, so a quiet claim under such a name gets no release.

**Why no release is printed while the name is active.** A working sibling under the same name may hold the item, and nothing stored says which session made an unbound claim. Releasing it would take work from a running agent. If you have looked and know the session is gone, the command is `another queue release <Q> --reason "<why>" --owner`; it is safe once no session of that name is working on the item.

**What the list cannot see for a day.** An unbound claim whose session died while a sibling keeps the name active is not listed until the claim is one grace period old. Until then the name is at its limit for that item, and a sibling's `another queue next` says so. Claims are unbound whenever two or more sessions share a name, which is the usual case on a machine with several sessions per agent.

**An `old_unbound` row cannot be dismissed.** A task that is still being worked on after a day shows here too; the row says only to look. It stays, and stays in the `--brief` count, until the item is done or released. `another queue note` does not clear it.

A `name_quiet` or `session_quiet` row clears on a sign of life, a release, `another queue done`, or the lapse after the grace period.

**`no_taker`.** A ready item nobody can be offered:

| Case | Row says | Next |
|---|---|---|
| Reserved for an agent that has no seat, gave the item up in the last hour, or was not seen for an hour | `ready, reserved for <agent>, which cannot take it now` | `another queue edit <Q> --for-anyone --owner` |
| No agent of the node has a limit above 0 | `ready, no agent has a seat` | `another queue limit default 1 --owner` when the default limit is 0, else `another queue limit list` |
| The only agents with a seat gave it up in the last hour | `ready, <agent> gave it up less than an hour ago and nobody else has a seat` | `another queue limit list`. It clears by itself within the hour |

## Closing lines

These are shown under the list, never counted and never in `--brief`, because nothing you do clears them.

| Line | What it is |
|---|---|
| `<n> sessions gone with nothing waiting` | Sessions the transport no longer lists that hold no bound claim and have no unannounced mail, or whose agent's mail is already a row: closed panes. They are pruned after a day |
| `<n> notices could not be sent in the last 24 hours` | Queue notices that failed, with up to five named |
| `<n> self-admitted in the last 7 days` | Items a delegate admitted for itself, by number, with the total. This is how a self-admitted item appears in the owner list for seven days ([QUEUE.md](QUEUE.md)) |

## JSON

One object. Every key is always present; a value that does not apply is `null` or `[]`.

```json
{
  "now": "2026-11-17T16:05:18.263Z",
  "node": "mac",
  "total": 7,
  "limit": 10,
  "counts": {
    "checks_stopped": 0, "transport_config": 0, "decide_now": 1, "blocked": 1, "stuck": 1,
    "unreachable": 1, "transport_silent": 0, "quiet_claim": 1, "no_taker": 1, "proposal": 1
  },
  "closing": {
    "gone_idle": 4,
    "failed_notices": [{"item": "Q1", "to": "claude@shop", "at": "2026-11-17T16:05:17.492Z"}],
    "self_admitted": {"recent": ["Q6", "Q5"], "total": 2}
  },
  "rows": [
    {
      "kind": "unreachable",
      "key": "unreachable:claude@api:91c0f2aa",
      "who": "owner",
      "since": "2026-11-17T14:00:17.495Z",
      "age_seconds": 7500,
      "agent": "claude@api",
      "item": null,
      "item_kind": null,
      "scope": null,
      "title": null,
      "session": "91c0f2aa",
      "transport": "herdr",
      "target": "w2:p1",
      "reason": "gone",
      "claims": ["Q3"],
      "name_claims": [],
      "as_of": "2026-11-17T16:05:17.495Z",
      "next": ["another queue show Q3", "another queue release Q3 --reason \"session gone\" --owner"],
      "next_delegate": []
    }
  ]
}
```

| Key | Meaning |
|---|---|
| `total` | The sum of `counts`. Nothing under `closing` is in it |
| `rows` | At most `limit` rows per kind, in the order of the sections. `[]` with `--brief` |
| `key` | Stable while the condition lasts: `<kind>:<item>` for item rows, `<kind>:<agent>:<first 8 characters of the session id>` for session rows, `transport_silent:<transport>`, and the bare kind for the two `Another` rows |
| `who` | `owner`, or `admitter` where a delegate's key covers the step that clears the row |
| `next` | The steps. An entry that is not a command never starts with `another` |
| `next_delegate` | The same steps as a delegate runs them: `--agent <delegate>` in place of `--owner`, with its key in `ANOTHER_ADMIT_KEY`. `[]` when `who` is `owner`; it never holds an owner-only command |
| `agent` | The proposer, the claimer, the reserved agent or the session's agent |
| `since`, `age_seconds` | When the condition began; `null` when that is not known |
| `session` | The first 8 characters of the session id, `""` when the session has none, `null` for rows not about a session |
| `claims`, `name_claims` | Items bound to this row's session; claimed items of the name bound to no session |
| `reason` | The stored reason for a session row (`stuck` gives the wake outcome that stopped it); the quiet-claim reason; `reserved`, `no_seat` or `skipped` for `no_taker`; why the file was refused for `transport_config` |
| `as_of` | When the session row was last checked |

`<why>`, `<delegate>` and `<your agent>` in a step are for you to fill in.

## What is shown, and what never is

Shown: agent addresses; item numbers, kinds, scope ids and titles (cut at 60 characters); transport names and pane ids, because the pane is where you go; why `transports.json` was refused; times. Session ids are cut to 4 characters in text and 8 in JSON.

Never shown: mail subjects and bodies; item bodies, done conditions, notes, reasons and result text; the detail a transport reported; the directory a hook ran in; delegate keys and tokens.

Every value an agent or a peer wrote is cleaned before it is printed, in the text form and in the JSON: control characters, zero-width and invisible characters and the characters that change the direction of text become spaces. That holds for every stored value, the stored reason of a session row and the stored outcome of a wake included, and for a reason this build does not know. The checks at write time are not relied on, since any local process can write the database. An address inside a printed command is put in single quotes when it holds anything a shell reads.

## Left out

- Mail and requests addressed to you: Another has no owner address. A decision for the owner is a proposed item.
- Requests between agents, unread mail and stopped edits: they wait on agents, and `another board` shows them.
- Installation health: `another doctor`.
- OAuth clients waiting for approval: `another oauth pending`.
- Filters, an MCP tool, an HTTP route, a dashboard panel and a push when a row appears.

## Cost

No process is started, no network call is made and nothing is written. On a home with 50 agents, 1,000 queue items (200 proposed, 100 ready, 50 claimed), 50 session rows that all need attention and 13,000 events, `another needs --brief` from a release build took a median of 16 ms over 40 runs on one Mac (17 Nov 2026), of which 5 ms is the start of the process. With no session in attention it took 8 ms.
