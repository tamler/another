# Design note 01: Lanes

Status: implemented in phase 1. Part of the manager-bot coordination series (01 lanes, 02 push, 03 decisions, 04 lane-aware holds, 05 spend ledger, 06 daily summary).

## Problem

A manager bot (a remote `grok_bot` agent with a bearer token) hands work to coding agents in terminal panes. Today it types instructions into panes and polls `STATUS` and `DONE` markdown files in a shared folder. There is no structured record of who owns a piece of work, what state it is in, or how it finished.

## Model

A **lane** is a named unit of work on one node.

| Field | Notes |
|---|---|
| `lane_id` | ULID |
| `name` | 1–64 chars from `[a-z0-9._-]`, case-folded; unique among the open lanes **of its project**. Address a lane by id, `name`, or `name@project` (`api@shop`). A bare name used in several projects is `ambiguous` for local callers. For a scoped token, lanes outside its projects don't exist: lookups return the same `not_found` as an unknown name, and "already named" only considers the project being created in. A closed lane's name can be reused. |
| `project` | **Required.** It is what remote token scopes are checked against. |
| `charter` | Free text, up to 16 KiB: the brief. |
| `lead` | Local agent that owns the work. |
| `created_by` | The agent that created the lane, typically the manager. |
| `status` | `planned → working ⇄ blocked ⇄ needs_decision → done \| abandoned`. Any open state can move to any other open state; `done` and `abandoned` are terminal and set `closed_at`. |
| `thread_id` | The mail thread that carries this lane's reports, created by the first report. |
| `outcome`, `pr_url` | Set by the DONE report or by close. |

**Links** (`lane_links`) attach existing objects: `thread`, `request`, `hold`, `message` (by id). Linking checks that the object exists and is in the lane's project where that is knowable. Phase 4 adds a real `lane_id` on holds; links remain for everything else.

**Reports** (`lane_reports`) are structured status messages:

| Field | Notes |
|---|---|
| `kind` | `status`, `done`, or `auto` (written by the stop hook) |
| `summary` | Required, up to 2 KiB |
| `progress` | Optional, 0–100 |
| `status` | Optional new lane status; it is applied |
| `links` | Optional list of URLs or ids |
| `artifacts` | Optional list of paths or URLs |
| `outcome`, `pr_url` | DONE reports only |

A `done` report sets the lane status to `done`, records `outcome` and `pr_url`, and closes the lane.

Every report is also **delivered as mail**, so it shows up in inboxes, digests and wakes:
- Reports go to the lane's counterpart: the creator when the lead reports, and the lead when anyone else reports.
- Mail goes into the lane thread.
- The subject is `[lane <name>] <status>: <summary>`.
- The body is a short rendering of the structured fields, plus `report <id>`.

Delivery reuses `send_message`, so federation, hooks and wake all work unchanged.

## Events

All lane events go into the append-only log and carry `lane_id`, `project_id` and the lane `name`. Following the feed rule (summaries and ids, never bodies), **no event carries a charter, report summary, progress, outcome, links or artifacts**. Those live only in the lane tables, which `lane_show` returns to callers allowed to see the lane. This applies to federated peers too, since they receive the same events.

- `lane.created {lead_id, status}`
- `lane.updated {lead_id?, charter_changed?}`
- `lane.status_changed {from, to}`
- `lane.linked {kind, ref_id}`
- `lane.reported {report_id, kind, status}`
- `lane.closed {status}`

Report mail is shown in the feed by subject, so its subject is `[lane NAME] STATUS: KIND report by AGENT`, and the summary goes in the body.

They get a new activity category, `lane`. For a remote token, `activity_feed`, `activity_summarize` and `GET /activity` only include lane events from its projects. (Phase 2 extends this scoping to every event kind.) Federation treats lane events as feed events: stored and shown, not projected into foreign lane tables in this phase.

**Links** (threads, messages, requests, holds) must belong to the lane:
- A message, thread or request belongs when its sender or a recipient is an agent of the lane's project, or is the lane's lead or creator.
- A hold belongs when it is in the lane's project.

Anything else is refused with the same `not_found` as an id that doesn't exist, so linking can't be used to probe other projects.

## Surfaces

| CLI | MCP tool |
|---|---|
| `another lane create <name> --project P --lead A --charter T [--status planned]` | `lane_create` |
| `another lane list [--status S] [--project P] [--lead A] [--all]` | `lane_list` |
| `another lane show <lane>` | `lane_show` (lane, links, last 20 reports) |
| `another lane update <lane> [--status] [--charter] [--lead] [--link-thread\|--link-request\|--link-hold\|--link-message ID]...` | `lane_update` |
| `another lane report <lane> --summary S [--progress N] [--status S] [--link L]... [--artifact A]...` | `lane_report` (`kind=status`) |
| `another lane done <lane> --outcome O [--pr URL] [--summary S]` | `lane_report` (`kind=done`) |
| `another lane close <lane> [--abandon] [--outcome O]` | `lane_close` |
| `another lane export <lane> [--out FILE \| --dir DIR]` | (CLI only) |

The acting agent comes from `--agent` / `$ANOTHER_AGENT`, as everywhere else.

**Export.** `--dir DIR` writes `<name>-STATUS.md`, plus `DONE-<name>.md` once the lane is done, matching the old `.lanes/*.md` convention for anyone who still wants files. Export is a rendering only; the database stays the source of truth.

## Remote access (`serve --auth`)

- Acting tools (`lane_create`, `lane_update`, `lane_report`, `lane_close`) must act as the token's agent; the agent is filled in when omitted.
- `lane_create` needs a project inside the token's globs.
- `lane_show`, `lane_update`, `lane_report` and `lane_close` check the **lane's** project against the globs; out-of-scope lanes are reported as "forbidden".
- `lane_list` is **filtered** to lanes whose project matches the globs. A scoped token never sees another project's lanes.
- Every call is audited by the existing `call_tool` wrapper.

## Hooks: reporting automatically

`another hook install --client claude|codex|grok|cursor --lane-reports` additionally installs a **Stop** hook (`Stop`, or `stop` for Cursor). It is opt-in, so existing installs don't change. Before storing the agent's last message as the report summary, the hook **redacts likely secrets**: tokens with known prefixes (`ano_`, `sk-`, `ghp_`, `github_pat_`, `glpat-`, `xox*-`, `hf_`, `AKIA`, `whsec_`) and long hex or mixed-case base64 runs become `[redacted]`.

`another hook run stop` does the following:
- It resolves the agent as the other hooks do.
- It resolves the lane: `$ANOTHER_LANE`, else the single open lane this agent leads in the project; if there is none, or several, it does nothing.
- It posts an `auto` report whose summary is the client's `last_assistant_message`, truncated to 300 chars, or "Turn finished".
- It is **debounced** to one auto report per lane per agent every 15 minutes, so a chatty session doesn't flood the lane.
- It fails open and never blocks the client.

Milestone reports come from the agent itself through `lane_report`. AGENTS-style snippets for that are in `docs/LANES.md`.

## Migration

Migration 6 is additive: three new tables and indexes, with no changes to existing ones, so it is safe on a populated database. A test builds a database at schema 5, writes data, then opens it with the current build and checks the data is intact and lanes work.

## Out of scope (later phases)

- Push delivery of lane events (phase 2).
- `needs_decision` linked to a decision request (phase 3).
- Lane-owned holds and conflicts (phase 4).
- Spend per lane (phase 5).
- Daily summary (phase 6).
