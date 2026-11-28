# Roadmap

Updated 28 Nov 2026. This page tracks current work against the approved v0.1.0 plan. [VISION.md](VISION.md) says what Another is for and lists its non-goals. [V0.1.0-SCOPE.md](V0.1.0-SCOPE.md) is the plan approved on 17 Nov 2026: what v0.1.0 must have, may have, freezes and cuts, and the 1 Dec 2026 tag date. It remains the source of truth for that scope. Decisions are in [DECISIONS.md](DECISIONS.md).

## What Another is

Another is a background messaging system that lets several agents work on the same project, on the same machine, at the same time, without colliding with each other or using up the machine's resources. Agents send each other messages, hold the paths they are editing, take agreed work from a shared queue, and record what they did. It runs in the background through hooks, a daemon and a transport such as Herdr, so an agent does not have to stop and check anything by hand.

It is not an inbox or a mail tool for people. A self-hosted hub can connect a few machines one person owns. What Another will not do is listed under Non-goals in [VISION.md](VISION.md).

## Current state

This is what is on `main` today. Where a done condition from the 17 Nov plan is still open, it says so.

- **Queue with admission and atomic `next`** (scope M1, M2). Proposed, ready, claimed and done; only the owner or a delegate admits; `next` hands one item to one agent and refuses an agent at its limit; results, handoffs and claim release are events. See [QUEUE.md](QUEUE.md). Not yet done: the three-day fleet run from this queue, and release of a claim on the transport's confirmed-gone signal (only Claude Code claims are bound to a session today).
- **Upgrade safety** (M3, #260 and #261). A checked backup before any migration, unknown schemas refused, development builds kept to their own homes, and `another doctor` reports a build and database mismatch.
- **Path holds scoped to a checkout** (M4, D92). Holds are enforced by the edit hook for Claude Code and Codex and block only inside the working directory they were taken in. Several admitted hold fixes are still open pull requests (see below).
- **Herdr transport adapter and command transports** (M5, D90, D93, D95). `wake`, `status` and `list`; one row per agent session; a wake that does not start work is recorded as failed. Not yet done: the remaining live checks against Herdr; command transports have run against fake programs only; the confirmed-gone signal ships switched off.
- **Hook packs** (M6, D96, D97). Claude Code and Codex see new messages and queue lines at session start and at a prompt, with no transport needed. The Grok pack exists (N1).
- **`another needs`** (M7, D94). One read-only list of what waits on the owner, and the read-only scan in `another doctor` for lapsed resources and unregistered listeners.
- **Daemon on macOS and Linux** (M8, D71). Wake and expiry run in the background.
- **Federation as it exists** (M9). Signed event logs, pairing, and hub and spoke sync of messages, holds and activity. The Q26 to Q31 fixes landed: pairing proves key possession (D101), the hub signs its invites and answers (D103), peer hold patterns are bounded (D102), oversized events are skipped loudly, and the Q31 pre-tag P2 security findings are closed (#281). Cross-node holds are still warnings only (D26), and queue claims are local to one node.
- **FLE-23 consolidations M-A, M-B, M-C and M-E** (#287, #283, #284, #282). Shared time helpers, shared crypto helpers with constant-time digest compare, shared path and git-root helpers, and test-support helpers for forged events. #294 completed M-A's bare-seconds follow-through.
- **Docs alignment** (#285, #289). The README, guides, CLI help and site describe background messaging for agents. #289 includes the #288 audit.
- **Security fixes on main.** #290 closed P3 findings that needed no wire change. #307 added a warning for an unauthenticated loopback listener. #308 binds signed sync requests to one hub and rejects replays. #309 summarizes unauthenticated audit refusals omitted by the write budget. #310 records the canonical binary path for hooks. #313 implemented federated hold ownership and collision reporting (D111).
- **FLE-23 follow-through.** M-K part 1, M-Y, M-W, M-N, M-Z, M-O, M-AD and M-P have merged (#295, #291, #298, #299, #300, #303, #302 and #306). The daemon status and doctor follow-through (M8 F2, #297) has also merged.

No release has been tagged and no prebuilt binary is published yet.

## Near term: the foundation

These are in progress or next. They land on `main` as part of getting to the first tag. The scope page still governs what the tag requires.

- **Prebuilt binaries** (D105, M10). In progress. People install a binary instead of building from source. Publishing a tag or a GitHub Release is the owner's call. The 17 Nov scope named macOS and Linux archives; Windows x86_64 prebuilds are added by the D32 amendment in PR #293, approved by the project owner on 27 Nov 2026 at 4:30 PM Manila.
- **Remaining security fixes** (D106). Open work includes pinned dependency audit checks (#292), remote hold confinement (#314) and shared federation HTTP policy (#318). Issue #134 remains open.
- **Duplication cleanup (FLE-23).** The [27 Nov admissions table](V0.1.0-SCOPE.md#admitted-27-nov-2026-by-the-pm-delegate-fle-23-follow-through) records completed items and the conditions for work still open, including any schema, wire or hook contract changes.
- **One lease model before the first tag** (D109, SPEC-B7). Holds, resource leases and queue claims share one record, one renewal rule and one session-end rule. This lands before the first tag.
- **Hold and hook fixes revived from open pull requests.** Each was admitted and needs a rebase and review:
  - #165: MCP resolves a local caller's project paths the same way the CLI does.
  - #179: nested projects outside git see each other's holds.
  - #163: `another hook status` reports drift from what `install` would write.
  - #149: when a waiting agent's session ends, its waits end too.
  - #171: live checks of the Claude Code, Codex and Grok hooks, with recorded results.

## Bigger bets, after the foundation

None of these is started on `main`. Each needs a spec and the owner's admission first. The cross-node queue is v0.2.0 work; its implementation starts only after the v0.1.0 tag, as the scope page already says.

- **Cross-node queue and shared holds decided by the hub.** Each item and each held path has a home node. Local work is decided locally and shared work by the hub, so a cut-off spoke keeps working on what it has and never guesses. This replaces D24 and D26 as D85 records. The hub-decided lease timing in closed #70 is design input.
- **Opt-in Cursor edit guard** (#147, scope N3). Cursor gets the same hold check as Claude Code and Codex, behind a flag, failing open.
- **Opt-in pre-commit hold guard that refuses** (scope N4). A commit that touches a path someone else holds is refused. The advisory version in closed #159 is design input.
- **Queue view and "hub unreachable" banner on the dashboard** (scope N5).
- **Rex adapter** (scope N2), once the owner has beta access.

## Changes to this page

The ranked draft that was here before 17 Nov, and the table that mapped it to the v0.1.0 scope, are in this file's git history.
