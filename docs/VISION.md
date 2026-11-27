# Another: vision

Status: approved by the project owner, 17 Nov 2026. This page and [V0.1.0-SCOPE.md](V0.1.0-SCOPE.md) outrank SPEC.md and earlier milestone briefs wherever they disagree. The decisions they change are recorded as D83 to D85 in [DECISIONS.md](DECISIONS.md).

## Purpose

Another is a background messaging system for coding agents working on the same project on one machine. Agents use it to exchange messages, coordinate edits and hand off agreed work. A self-hosted hub can connect machines.

Three things together do that job:

1. **A shared queue.** Agents are woken and pull the next claimable item. Only admitted work can be claimed.
2. **A durable record.** Every claim, handoff and result is recorded, so a crashed session or a bad upgrade loses nothing and any agent can pick the work up.
3. **Path holds.** Two agents do not step on the same files.

Keeping agents busy is not the goal. An agent that is busy on work nobody agreed to is a failure, the same as an agent that sits idle.

## Users

- **First: one developer running many agents on a few machines they own.** The project owner's own setup is the test bed, because it is the only place Another has been seen to break in practice. A feature that has not run there is not proven.
- **Second: other developers who run several Claude Code, Codex or Cursor sessions at once.** For them a release must be readable and installable. No feature is built for them that the first user does not need.
- **Not now: teams and hosted multi-user setups.**

## Principles

1. **One test for every feature.** Does it move agreed work forward without anyone losing or redoing work? Does it stop two agents from colliding, losing work or sitting idle? If not, it waits.
2. **Anyone can propose; only the owner, or an architect the owner delegates to, admits.** Agents record what they find and move on. An item becomes claimable only when it names the scope item it serves, has a clear done condition, and was approved by the owner or that architect. Security and data-loss findings go to the owner personally for a fast yes or no, and still need the yes.
3. **Nothing is lost.** State survives a crashed session, an offline machine and an upgrade. A build never changes a database it does not understand, and never migrates one without a backup.
4. **Another coordinates; transports reach sessions.** Herdr and Rex run and reach agent sessions. Another sits above them and never owns a session, reads a pane or sends keys. It talks to a transport through one small adapter interface, and hook packs are the fallback that works with no transport at all.
5. **A wake is not done until the agent is working.** Delivery alone proves nothing. An agent that is blocked on a person, stuck or unreachable is shown to the owner.
6. **Failures are loud.** An unreachable hub, a failed wake, a stuck agent or a schema mismatch is visible status, never silence.
7. **Local first, self-hosted, free and open.** One SQLite database per machine. Machines the owner runs sync through a hub the owner hosts. No telemetry, no paid tier, and no network call the owner did not configure.
8. **One arbiter, no optimistic claims.** Across machines, shared work is decided by the hub. A machine that is cut off keeps working on what it already has and never guesses.
9. **Small beats complete.** Code that fails the test is frozen or left out, not grown. Frozen code stays in the tree, keeps passing CI, and gets no work except admitted security or data-loss fixes.
10. **Independent review.** Nobody reviews their own work, and no work merges without a review by someone else.

## Non-goals

- Launching, hosting or owning agent sessions. That is the transport's job.
- Teams, shared multi-user hosting, or a hosted service.
- Topologies other than hub and spoke between machines one person owns. No mesh.
- A plugin platform. Integration is the MCP tools, the CLI and the transport adapter interface.
- An adapter for every terminal multiplexer. Herdr and Rex are maintained; anything else uses the generic command adapter.
- Turning holds into locks. A hold is an expiring, recorded claim; a separate checkout is the real boundary.
- Killing processes automatically. Reaping stays a human decision on the owning machine.
- End-to-end encrypted relay, a TUI, an A2A bridge, telemetry or a paid tier.
