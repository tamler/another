# Hook packs

Reference hooks call the Another CLI and its shared `another.agent/1` core.
Installed packs never pass `--unguarded-repair`. The ordinary path is
fail-closed for participating edits, including a missing home, incompatible
schema, unavailable binary, malformed identity or exhausted lock budget.

## Install and trust

```sh
another hook install --client claude
another hook install --client codex
another hook status
another hook uninstall --client claude
```

Installation preserves other handlers and configuration permissions, backs up
changed files and uses a small POSIX wrapper. It edits no shell launcher.
Claude uses `~/.claude/settings.json`, Codex `~/.codex/hooks.json`, Grok
`~/.grok/hooks/another.json`, and Cursor `~/.cursor/hooks.json`.

Codex requires the user to review/trust new or changed hooks in `/hooks`.
Configuration presence is not execution evidence. Another cannot grant trust,
and does not recommend `--dangerously-bypass-hook-trust`.

## Identity and private storage

The CLI enrolls the adapter locally and mints an individual canonical root
`agent@session` or child `child.agent@session` before admission. Native ids,
names, model labels, `ANOTHER_AGENT` and `--agent` are not ownership proof.
Private participant grants and returned actor/epoch records live below
`ANOTHER_HOME/adapters/v1/CLIENT/`, with private directories and files.
A lost/corrupt grant refuses; it does not adopt legacy claims by name.

Claude/Codex tool hooks share a root session and identify children with
`agent_id`. A registered Claude root with no child indicators is a root.
Known child metadata with a missing/contradictory discriminator refuses rather
than crediting root. A byte-identical missing-child payload remains a stated
profile risk. Grok remapped child sessions are dependent scopes, bound by the
parent's child-start evidence. SubagentStop/Stop is not session end.

A native callback without epoch carriage cannot infer a newer resumed epoch.
The adapter retains the proof it received. Same-identity resume is available
through the authenticated private contract; stock callbacks that cannot prove
that resumed context refuse. A new native id is not evidence that a child has an
independent lifetime.

## Event behavior

| Event | Effect |
|---|---|
| Session/child start | Register or resolve under private adapter authority; record attributed presence |
| Prompt/read/search | Presence only; never renew or release files |
| Structured edit Pre | Atomically checkout exact targets and store call references; conflict or unknown identity denies |
| Repeat Pre | Reuse the live lease without moving its deadline; completed/expired calls are not execution authority |
| Successful Post | Complete the correlated call and renew only its exact, still-live generations |
| Permission denial, unknown failure, missing Post | No file renewal; the existing deadline remains |
| Session end | Close the authenticated epoch and release its complete dependent scope tree immediately |
| Late Post/failure | Record old outcome history without restoring released or expired rights |

The one default TTL is **120 seconds**, with requested checkout durations clamped
to **30 seconds through 24 hours**, stored and returned. Renew uses the stored
duration. No heartbeat, process poll, writer timer, stage or in-flight reference
adds another ownership clock. Presence is a separate subject.

Pre waits 300 ms and may use another 2700 ms when SQLite remains locked, within
a 3-second total bound. Post uses a short 50 ms store budget and may leave an
outcome unconfirmed; it cannot extend the prior lease because of a failure.
These are IO budgets, not lease clocks.

## Quiet output and idle reminder

Routine hooks emit no inbox, queue or wake digest. They emit only actual conflict
notices, the short stale-actor reminder, and required native control JSON.
The notice budget is 1000 characters.

After 120 seconds without attributed activity, the actor is unused/stale.
Before recording returning activity, show once for that episode:

> You were inactive. Release what you no longer need; re-check expired leases before writing.

The reminder neither extends nor releases rights and sends no wake. Read-only
status can show it without acknowledging it. Longer requested checkouts keep
their actual deadlines. Reads/searches do not mean the actor moved on.

## Shell coverage and queue lines

Shell Pre is installed too. An opaque shell/script/build may write and cannot be
admitted by guessing paths. The required ordering is matched Pre, then an actual
private per-call handle or proven callback/ancestry association, then epoch and
generation validation at commit. Ancestry identifies a session, not a
shared-process child. No ordinary environment injection is assumed.

Until that carriage is implemented/tested, stock shell mutation routes visibly
refuse `NO_CALL_CONTEXT`/`NO_ACTOR`. The private integration channel can
declare exact targets without model-selected actor names. Ambiguity is among
live concurrent actors only; idle registrations are not additional candidates.
There is no fallback to the one name that happens to be visible.

The old automatic queue lines and D96 digest behavior are superseded. Query the
queue explicitly; proposed items still require admission before a claim.
[Queue](QUEUE.md), [contract](AGENT-CONTRACT.md).

## Cursor

Keep the shipped install quiet and unguarded. The live profile is
`cursor-agent 2026.11.22-e9042b2`: root `session_id == conversation_id`;
Task children remap session/conversation/generation to a child UUID, corroborated
by the parent's `tool_output.agentId` and
`.../subagents/CHILD.jsonl` transcript. Tool hooks do not carry `agent_id`.
Documented subagent start/stop events did not fire and are not prerequisites.

Hook deny produces `postToolUseFailure/permission_denied`. The corrected
interrupt probe produced sessionEnd followed by a late failure with
`is_interrupt:false`; that field is not trusted. `hook_ppid` varied, so it is
not shell proof. Cursor shell/CLI mutations remain `NO_ACTOR` until actual
private carriage is tested. Payload mapping is not guarded runtime acceptance.

## Explicit legacy repair

`--unguarded-repair` is an operator's visible exit from guarded participation.
It can inspect/import or repair historical name-based observations and expose
the experimental legacy catalogue. Its requested views are not native hook
injections, and its advisory records cannot mint or adopt contract rights.
It cannot mutate a minted actor by selecting its name. No installed hook uses
it; no internal error selects it automatically.
