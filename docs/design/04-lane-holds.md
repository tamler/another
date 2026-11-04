# Design note 04: Lane-aware holds

Status: implemented in phase 4. Builds on [01-lanes](01-lanes.md) and [02-push](02-push.md).

## Problem

Holds belong to agents. With lanes, the useful questions change:
- Which *lane* is touching `src/api/**`?
- Is lane A about to collide with lane B?

Two agents working in the same lane, such as a lead and a helper, shouldn't block each other the way two unrelated agents do. Holds also only work when agents remember to call `hold_paths` before editing. A pre-edit hook makes that automatic.

## 1. Holds carry a lane

- `holds.lane_id` is a new nullable column.
- `hold_paths` and `another hold acquire` take an optional `lane` (id or name). It is looked up **within the hold's project only** (lane names are per project, maintainer's rule): a lane in another project, out of scope or missing is the same `unknown lane` error. It must be open.
- Without `lane`, the hold takes `$ANOTHER_LANE`, else the single open lane the agent leads in that project, else no lane. The env var applies to the CLI and hooks.
- Re-acquiring a pattern you hold also updates its lane.
- `Hold` gains `lane` (the lane name or null).

## 2. Conflict rules

The rule for whether two overlapping holds clash is unchanged: one of them must be exclusive, and the holders must be different agents. Lanes change it in one place:

| Requester | Existing holder | Result |
|---|---|---|
| no lane, or a lane | another agent, no lane | conflict (as before) |
| no lane | another agent, lane B | conflict (as before), reported with lane B |
| lane A | another agent, lane B | **cross-lane conflict**, reported with both lanes |
| lane A | another agent, **lane A**, at least one hold shared | **not a conflict.** Reported under `warnings` with `same_lane: true`, because a lane is one unit of work and its lead coordinates inside it. |
| lane A, exclusive | another agent, **lane A**, exclusive | **conflict** (maintainer's decision): two exclusive holds block each other even inside a lane, so lane-mates can't both edit the same file. No `lane.hold_conflict` (it isn't between lanes). |

- `HoldConflict` gains `lane` (the holder's lane) and `same_lane`.
- Without lanes, behaviour is exactly as before.

**Events:**
- `hold.conflict` (from phase 2) now also carries `lane_id` (the requester's lane) and `held_lane_ids`.
- When lanes are on both sides, a second event, `lane.hold_conflict`, is emitted with `lane_id`, `other_lane_id`, `project_id`, the paths and both agents. It reaches the manager through the `hold_conflict` webhook filter.
- **The holding lane's lead is mailed** (maintainer's decision), from the system agent `another`: subject `[lane HOLDING] hold conflict with lane REQUESTING`, with the paths, holders and requester. That is at most one mail per lane pair every 10 minutes, so a retrying pre-edit hook doesn't flood the inbox. Events and webhooks are unchanged. The requester sees the conflict in the tool result, or in the hook's denial.
- `hold.conflict` and `lane.hold_conflict` are **node-local**: never federated (D38).

## 3. Pre-edit hook

This is opt-in, like the Stop hook: `another hook install --client claude,codex --pre-edit`.

**Claude Code** (`~/.claude/settings.json`):
- A `PreToolUse` entry with matcher `Edit|Write|MultiEdit|NotebookEdit`.
- The file comes from `tool_input.file_path`, or `notebook_path` for notebooks.

**Codex** (`~/.codex/hooks.json`):
- A `PreToolUse` entry with matcher `apply_patch`.
- Codex versions differ in where the patch text appears: in `tool_input.patch`, in `tool_input.command`, or as a string `tool_input`. The hook accepts all three.
- Codex runs a hook only after it is trusted under `/hooks` (tied to the hook's hash), and `codex exec` skips untrusted hooks. Enforcement under `codex exec` therefore needs prior trust or `--dangerously-bypass-hook-trust` (checked on Codex 0.166.0; see HOOKS.md).
- File names come from the patch's `*** Add File:`, `*** Update File:`, `*** Delete File:` and `*** Move to:` lines.

**At run time**, `another hook run --event pre-edit` does this (file paths and the project root are compared **after resolving symlinks**, so `/var/…` and `/private/var/…` match):
1. Resolves the agent and project the same way the other hooks do. Files outside the project are ignored.
2. Acquires an **exclusive** hold on each file:
   - for the agent's lane, using the rules above;
   - with the default TTL (30 min) and reason `pre-edit`.
   - Holds the agent already has are renewed.
3. On a conflict, denies the tool call, using the documented `PreToolUse` output:
   ```json
   {"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "Another: src/api/routes.rs is held by codex@shop (lane billing) until 14:05. Coordinate with them (send_message) or wait; `another holds --by-lane` shows who holds what."}}
   ```
4. Otherwise prints nothing, so the normal permission flow applies. A hook that stays silent never approves anything.

The hook **fails open, visibly**: if `$ANOTHER_LANE` names a lane that is closed or not in this project, the files are still held (without a lane), and the hook prints `{"systemMessage": "…"}` (and the same on stderr) explaining why. On any other error, or if Another is unavailable, it prints nothing and the edit proceeds, as the other hooks do.

**Lifetime:** holds taken this way expire like any other. The prompt hook's heartbeat renews them, and session end releases them.

## 4. Holds-by-lane view

| Surface | View |
|---|---|
| Core | `Store::holds_by_lane(filter, scope)` groups live holds as `[{lane, project, lead, status, holds: [...]}]`. Holds without a lane come last, with `lane: null`. |
| CLI | `another holds --by-lane [--project P]` (two queries in all, whatever the number of holds) |
| MCP | `holds_by_lane` (remote: only the token's projects) |
| Lanes | `lane_show` / `LaneDetail` gain `holds`, the lane's live holds |

## Migration

Migration 9, additive: `ALTER TABLE holds ADD COLUMN lane_id TEXT`.

## Out of scope

- Hard per-lane path ownership, where a charter declares the paths a lane may touch. The manager can approximate it with long-lived exclusive holds.
- Federating lanes: cross-node clashes stay warnings, as in D26.
