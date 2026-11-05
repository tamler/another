# Lanes

A **lane** is a named unit of work that a manager (often a remote `grok_bot` agent) hands to a lead agent: a charter, a project, a status, linked threads, requests and holds, and a history of structured reports. Lanes replace typing instructions into terminal panes and polling `STATUS.md` / `DONE.md` files. The design is in [design/01-lanes.md](design/01-lanes.md).

## Statuses

`planned → working ⇄ blocked ⇄ needs_decision → done | abandoned`. Any open status can move to any other open status. `done` and `abandoned` close the lane, and a closed lane's name can be used again.

Asking a [human decision](HUMAN-DECISIONS.md) with `lane` set moves the lane to `needs_decision` automatically. When its last open decision is answered, expires or is cancelled, the lane returns to `working` (if it is still `needs_decision`).
**Names** are unique among a project's open lanes, so `shop` and `vault` can each have an `api` lane. Refer to a lane by name, by `name@project` (`api@shop`) when the name is used in more than one project, or by id.

**Events** carry ids, the lane name, project and status, never the charter or report text; read those with `lane show`.

## CLI

```bash
another lane create billing --agent manager --project ~/work/shop --lead claude@shop --charter "Add invoices. Done = tests + docs + PR."
```

```bash
another lane report billing --agent claude@shop --summary "Schema and migration done" --progress 40 --artifact docs/billing.md
```

```bash
another lane report billing --agent claude@shop --summary "Need a call on currency rounding" --status needs_decision
```

```bash
another lane done billing --agent claude@shop --outcome "Invoices shipped behind a flag" --pr https://github.com/you/shop/pull/12
```

```bash
another lane list
```

```bash
another lane show billing
```

```bash
another lane update billing --agent manager --link-thread <thread-id> --link-request <request-id>
```

```bash
another lane close billing --agent manager --abandon --outcome "superseded by billing-v2"
```

Every report is stored on the lane and also **mailed** to the lane's counterpart: the creator when the lead reports, the lead otherwise. The mail goes into the lane's thread with a subject like `[lane billing] needs_decision: status report by manager@shop`; the summary is in the body. Blocked and needs-decision reports are sent as high importance, so they appear in digests and `another wake`.

## Path holds and lanes

**Which lane a hold is for:** the `lane` argument of `hold_paths`, or `another hold acquire --lane` (defaults to `$ANOTHER_LANE`; `hold check` takes the same flag and gives the same answer), looked up in the hold's project only; another project's lane is an unknown lane. Without one, it is the single open lane the agent leads in that project. `-` means no lane, without that fallback. Holds without a lane behave as before.

**Conflicts between lanes:**

| You (lane) | Overlapping hold by another agent | Result |
|---|---|---|
| lane A | lane A, one of the two holds shared | Not a conflict: listed under `warnings` with `same_lane: true` |
| lane A, exclusive | lane A, exclusive | Conflict: lane-mates can't both hold the same file exclusively |
| lane A | lane B, or no lane | Conflict. You get `hold.conflict`, plus `lane.hold_conflict` when both sides have a lane (the `hold_conflict` webhook filter delivers it), and lane B's lead is mailed by `another` (at most once per lane pair every 10 minutes) |
| no lane | anything | Conflict, as before (when either hold is exclusive) |

**Seeing who holds what for which lane** (a token's `project`/`agent` filters only match holds it may see):
- `another holds --by-lane`
- MCP `holds_by_lane`
- `lane show` lists the lane's live holds.

For holds taken automatically before each edit, see the [pre-edit hook](HOOKS.md#optional-hold-files-before-they-are-edited).

## Markdown export (the old `.lanes` files)

```bash
another lane export billing --dir ~/Projects/.lanes
```

This writes `billing-STATUS.md`, and `DONE-billing.md` as well once the lane is done. `another lane show billing --markdown` prints the same text. The database stays the source of truth; export is a convenience for anyone who still reads files.

## MCP tools

`lane_create`, `lane_list`, `lane_show`, `lane_update` (status, charter, lead, links), `lane_report` (`kind`: `status` or `done`), and `lane_close`.

Over remote MCP (`serve --auth`), a token only sees and changes lanes whose project matches its globs, and acts as its own agent. A lead it names is looked up among the agents it may see (its projects, plus project-less agents), so a hidden agent is the same `not_found` as a missing one. Out-of-scope lanes return `not_found`, the same as missing ones, and `lane_list` leaves them out.

## Reporting automatically from Claude Code and Codex

Install the opt-in Stop hook:

```bash
another hook install --client claude,codex --lane-reports
```

At the end of a turn, the hook posts an `auto` report into the agent's lane. The lane is `$ANOTHER_LANE` if set, otherwise the single open lane the agent leads in that project. The report's summary is the client's last reply, truncated to 300 characters; Codex sends it, and so do Claude Code versions that include `last_assistant_message`. Reports are debounced to one every 15 minutes per lane and agent. Run `another hook install` again without `--lane-reports` to remove the Stop hook.

## Snippet for your project's AGENTS.md / CLAUDE.md

```markdown
## Lanes (Another)

You may be leading a lane (check `lane_list` with `lead` set to you, or `$ANOTHER_LANE`).
- Read the charter with `lane_show` before starting.
- On each milestone, call `lane_report` with a one-line summary, `progress` (0-100), and
  `links`/`artifacts` for anything you produced.
- If you are stuck, report with `status: blocked`; if you need a human call, report with
  `status: needs_decision` and say exactly what you need decided.
- When finished, call `lane_report` with `kind: done`, an `outcome`, and the `pr_url`.
- Do not write STATUS/DONE markdown files; Another is the record.
```
