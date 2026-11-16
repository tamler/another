# Work board

One read-only answer, per project, to three questions: who holds which paths, what each agent touched recently, and what is waiting on whom.

```bash
another board
```

```bash
another board --project shop --since 2h --json
```

The MCP tool is `work_board` (`project`, `since`, `limit`, all optional). The dashboard shows the edit part in its **Edits** section.

## What it shows

For each project with live holds, recent edits or stopped edits, most recently active first:

- **Holds.** Agent, path or glob, exclusive or shared, the reason, whether the edit hook took it, how long it has been held and when it lapses.
- **Recent edits.** Files the edit hook held for an agent before an edit, newest first, one row per agent and file.
- **Stopped edits.** Edits the edit hook refused because another agent held the file: who was stopped, on which path, and whose hold it was.

Then, for the whole node or the filtered project:

- **Agents.** Each agent with a hold, unread mail, a message waiting for its acknowledgement, or a pending request. Presence (`live`, `idle`, `offline`) follows the same rule as the dashboard.
- **Pending requests.** Open, claimed and stale requests, oldest first, with sender, recipients and owner. Claimed work counts against its owner; open or stale work counts against its recipients.

No message bodies are included.

## Options

| Option | Default | Meaning |
|---|---|---|
| `--project`, `-p` | all projects | A slug or a path. Sibling checkouts of the same repository (see [HOLDS.md](HOLDS.md)) are included, because their holds conflict |
| `--since` | `24h` | Window for recent and stopped edits: a duration (`30m`, `2h`, `2d`) or an RFC 3339 time. Holds and requests are always current |
| `--limit` | `20` | Items per list, at most 200. Holds are never cut. A cut list says so |

## Sample

Two checkouts of one repository, after Claude edited two files in `shop`, Codex was stopped on one of them in `shop-review`, and Claude asked Codex for a review:

```text
Board for local, shop at 2026-11-16T02:35:52.819Z (edits since 2026-11-15T02:35:52.819Z)

shop-review  /work/shop-review
  holds
    codex@shop-review  src/cli.rs  exclusive, held 0s, lapses in 29m (edit hook)
    codex@shop-review  docs/CHANGELOG.md  shared, held 0s, lapses in 29m (release notes)
  recent edits (paths the edit hook claimed)
    0s ago  codex@shop-review  src/cli.rs
  stopped edits
    0s ago  codex@shop-review on src/parser.rs, held by claude@shop (shop:src/parser.rs)

shop  /work/shop
  holds
    claude@shop  src/parser.rs  exclusive, held 0s, lapses in 29m (edit hook)
    claude@shop  src/lexer.rs  exclusive, held 0s, lapses in 29m (edit hook)
  recent edits (paths the edit hook claimed)
    0s ago  claude@shop  src/lexer.rs
    0s ago  claude@shop  src/parser.rs

Agents
  codex@shop-review  live, seen 0s ago: 1 unread, 1 to acknowledge, 1 request(s), 2 hold(s)
  claude@shop  live, seen 0s ago: 2 hold(s)

Pending requests
  open  Review the parser change  from claude@shop to codex@shop-review, opened 0s ago  [01M7H3C0K20TE0AY0CTEW62YT8]
```

## It writes nothing

`another board` opens the database the way the hooks do: it does not create a data directory, run a migration or sweep. No event is written. If there is no database yet it says to run `another init`.

Because nothing is swept, a hold past its expiry is left out by its expiry time, and a request past its due time that no sweep has marked `stale` yet is shown with `overdue: true` (`overdue` in the text output).

## Limits

- **Edits are the edit hook's record, not a diff.** The hook writes an event when it takes a hold. Later edits under a hold the agent already has write nothing, so the board shows the first edit of each file per hold, not every edit, and never line counts. Edits made without the hook (an agent with no hook pack, a human in an editor) are not on the board.
- **This node only.** Holds and edit events are node-local, like `another holds`. Requests and mail stored on this node are included.
- **Sibling checkouts** come from the repository cache Another fills in as projects are used (the edit hook, hold commands, `another holds`). The board reads that cache and never refreshes it, so a checkout no command has touched yet is not grouped with its siblings.
- **Remote connections** (bearer token or OAuth grant) must pass a `project` inside their globs. They see that one project and its agents, without sibling checkouts.
