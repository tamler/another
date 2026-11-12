# Roadmap (draft, not approved)

Status: a draft written on 12 Nov 2026 from [SPEC.md](SPEC.md) (Scope, Stale-hold design, Risks), the README status table, [DECISIONS.md](DECISIONS.md), the two feature specs under review, and gaps the current docs state plainly. Nothing here is committed to. Each item is a candidate that gets its own short spec and an independent review before any code; the order is a proposal, and the owner of the project decides it. The site's [changelog](https://toanother.one/changelog/) describes only what is on `main`; this file is the only place that lists what is not.

## What is on `main` today

Milestones 1 to 7 (local core, hook packs, resource registry, authenticated remote peer, federation, wake adapters and release builds, hosted connectors with OAuth), the loopback dashboard, and push delivery with enforced holds (mid-task notices, the edit guard, holds across worktrees, the launchd daemon, Herdr wakes). No GitHub Release has been published yet; the release workflow exists and install is from source.

## In review now (not roadmap items)

- #108, #110, #111: automatic edit holds that stick, back-off for blocked edits with a ping when the hold clears, closing a linked request on release, push de-duplication. Reviewed with findings; fixes queued. These carry schema migrations (v9, v10), so they are not run against a live home until they merge.
- #112: Codex hook trust wording in install and status output.
- Two specs, both waiting on independent review: telling a blocked session when a hold ends, and a "needs you" list of items that wait on a person. Neither is implemented. The "needs you" build is held until the project owner's verdict.

## Ranked candidates

Ranking weighs three things: feedback already received from people trying Another, how much a change reduces the risk that a hold or lease gives false confidence (SPEC, Risks), and whether the current docs already have to apologise for the gap.

| Rank | Candidate | Why now | Source | What it needs first |
|---|---|---|---|---|
| 1 | **Publish v0.1.0** | The README, install page and `docs/VERIFY.md` all describe archives that do not exist yet. Everything below is easier to try with a binary | README install; `release.yml` | A tag push by the project owner after CI is green. Not an agent action |
| 2 | **Tell the blocked session when a hold ends** | Direct user feedback: an agent stopped by the edit guard learns the file is free only by retrying. The spec exists | Spec under review; SPEC Stale-hold design point 4 | Codex PASS on the spec, then a small PR with tests; no migration against a live home during a test window |
| 3 | **Pre-commit hold guard (opt-in)** | The edit guard stops client edits, but a shell write gets through and nothing checks a commit. The changelog says so in public. A commit hook that warns when staged files are held by someone else closes the loudest gap without making a hold a lock | SPEC Later; Stale-hold design point 3 (`optional git-commit PostToolUse (opt-in)`) | A spec that keeps it advisory by default and never blocks a commit on a database error |
| 4 | **`another daemon` on Linux** | The daemon manager writes a launchd LaunchAgent on macOS only (D52). DAEMON.md gives an example systemd user unit that Another's tests do not exercise, so Linux users wire the daemon by hand | DAEMON.md, D52 | A spec for `another daemon` managing a systemd user unit with the same install, uninstall, status and restart verbs, tested the way the launchd path is |
| 5 | **MCP resources and `subscribe` for clients that catch up** | Hosted clients poll `whats_new`; a client that can subscribe would get mail without a hook or a wake | SPEC Later | Check which MCP clients honour subscriptions before writing anything |
| 6 | **An edit guard for Cursor** | Cursor gets no edit guard because it blocks a tool call when a `preToolUse` reply is not valid JSON, and a hook that fails open prints nothing (HOOKS.md) | HOOKS.md client table | A spec that proves a fail-open reply shape Cursor accepts; otherwise stays out |
| 7 | **"Needs you" list** | Items that wait on a person (pending OAuth approvals first) are scattered. Four review rounds so far; the design is still open | Spec under review; held | The project owner's verdict, then a passing spec |
| 8 | **End-to-end encryption for relayed mail** | A hub can read the mail it relays (D28). Self-hosting is the mitigation today; a community relay would need this | D28, FEDERATION.md, SPEC Risks | A design that keeps the event log signable; large, and only worth it once anyone runs a shared hub |
| 9 | **Cross-repo contact directory, A2A bridge, Beads deep link** | Ideas from the original spec; no user has asked | SPEC Later; Beads deferred in DECISIONS | A user with the problem |
| 10 | **Mesh federation** | Explicitly after hub-and-spoke is solid, which it has not had time to be | SPEC v2 | Evidence from hub use |
| 11 | **TUI** | The loopback dashboard covers the viewing need; a terminal UI is nice to have | SPEC Later; D39 | Nothing until someone misses it |

## Not planned

- **Auto-reap policies.** The locked default is that nothing is killed without a human `--confirm` on the owning machine (D8, D17). The risk of a false orphan kill outweighs the convenience. The orphan list and the reap plan stay; the decision stays with a person.
- **Turning holds into locks.** Holds are advisory outside the edit guard, and a separate checkout is the real boundary. Candidate 3 adds a warning, not a lock.
- **Telemetry or a paid tier.** Locked defaults.

## How an item leaves this list

1. A short spec in `docs/` or the lane directory, read against current `main`, naming the exact commands, events and docs it touches.
2. An independent review that passes.
3. One small PR with tests, the matching README and site change, CI green, independent review again, then merge.

If an item here turns out to be wrong, delete it rather than keep it for completeness.
