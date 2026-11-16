# Group work queues

A group is a named set of agents in one project. A request sent to a group is a queue item: nobody gets mail for it, and any member takes the next item with `another request next`. Two members never get the same item. Every step is in the event log. See decision D77.

Use a queue when any one of several agents could do a piece of work ("review this PR", "port this fixture"). Use a direct request when you know who should do it.

## Groups

```bash
another group create reviewers --agent alice@shop --project ~/src/shop   # alice is the first member
another group join reviewers --agent bob@shop
another group add reviewers --agent alice@shop --member carol@shop
another group list                                                       # members and queue counts
another group leave reviewers --agent bob@shop
another group delete reviewers --agent alice@shop
```

- **Names** are 1 to 40 characters from `a-z`, `0-9`, `-` and `_`, unique within a project. The address is `reviewers@shop`. A bare name means a group in the acting agent's own project.
- **Members** may be any local agent, in any project.
- **Leaving** keeps the items you own: you can still complete or release them, but you can no longer take new ones from that group.
- **Deleting** a group is refused while it has open, claimed or stale items.

## Queueing work

```bash
another send --from lead@shop --to group:reviewers --priority high --subject "Review #143" --body "..."
another requests --group reviewers@shop
```

- **Requests only.** Sending to `group:<name>` needs `--request`, `--due` or `--priority`. Plain mail to a group is refused: groups queue work, they do not broadcast.
- **Mixed recipients:** a send can also name agents on this node directly, who get the message as mail as usual. It can name at most one group, and not in `--cc`.
- **Priority:** `urgent`, `high`, `normal` (the default) or `low`. Every request has a priority now; requests made before this feature are `normal`.

## Taking work

```bash
another request next --agent bob@shop                     # any of bob's groups
another request next --agent bob@shop --group reviewers   # one group
another request next --agent bob@shop --include-stale --due 3600
```

- **Order:** `next` takes the first open item, by:
  1. priority;
  2. due time (none last);
  3. age;
  4. id.

  With `--include-stale`, it also takes over stale items, after the open ones of the same priority.
- **Atomic:** the pick and the claim happen in one write transaction, so two agents (or processes) can never claim one item from the same state. A later stale takeover is a reassignment, recorded with the previous owner.
- **Nothing queued:** `next` prints `nothing queued for you` (JSON `null`) and writes nothing of its own. Like every request command, it first marks overdue requests stale, and that transition is recorded once.
- **Deadline:** without `--due`, a future due time from the sender is kept and a past one is cleared.
- **Claim by id:** `another request claim <id>` works on queue items for members only. Completing an unclaimed item takes it too, so it is also for members, plus the item's sender; an owner may complete its own item even after leaving.
- **The thread:** the claimer gets the item's message in its thread, already marked read, so `reply` and the `Done:` mail work as for any request. If the claimer was already a direct recipient, its mail is left as it was (still unread if it was).
- **Release and complete:** release puts the item back as open, dropping a past deadline. Completion mails the sender `Done:` as usual.

MCP clients use `send_message` with `to: ["group:reviewers"]`, `create_request: true` and `priority`. They take work with `claim_request` and `action: "next"`, optionally with `group`, `include_stale` and `due_in_seconds`. When nothing is queued, `next` returns a `not_found` error.

## Audit

Every queue event carries `group_id` and `priority`:

- `request.created`;
- `request.claimed`, with `via: "next"` or `"id"`;
- `request.released`, `request.cancelled` and `request.stale`, including one marked from the dashboard;
- `request.updated`, when an owner moves a queue item's deadline;
- `request.completed`, with `claimed_at` and `duration_ms` (both `null` for an item completed without a claim).

Group changes are `group.created`, `group.member_added`, `group.member_removed` and `group.deleted`. `another activity` summarizes them without message bodies.

## Limits in this release

- **Local only.** Groups, their members and queue items stay on this node and are never sent to peers. Federation shares neither group events, nor a queue item's message, nor any request event that carries a `group_id`.
- **No remote access.** Remote token and OAuth callers cannot send to a group, take or act on a queue item, and do not see queue items or group events in `fetch_inbox`, `whats_new`, `activity_feed` or `GET /activity`. Scoped remote access is planned.
- **Not covered yet:** group management through MCP, queue counts in the session-start digest and the board, priority aging, and moving items between groups.
