# AGENTS.md: build brief (v0.1.0)

**Read this before writing code.** What Another is for lives in [docs/VISION.md](docs/VISION.md). What v0.1.0 includes lives in [docs/V0.1.0-SCOPE.md](docs/V0.1.0-SCOPE.md). Locked defaults and every decision are in [docs/DECISIONS.md](docs/DECISIONS.md); contributor rules in [CONTRIBUTING.md](CONTRIBUTING.md). [docs/SPEC.md](docs/SPEC.md) is the original research and is historical.

## Status

The seven original build milestones are complete and on `main` (they are not the scope ids M1 to M10 below): local core (tamler/another#1), mail checking and the activity feed (#2), the resource registry (#3), the authenticated remote peer (#4), federation (#5), wake adapters and release builds (M6), and hosted connectors (#99). Their briefs are in this file's git history and their decisions are D1 to D81.

The current work is **v0.1.0, the single-node queue release**, approved by the project owner on 17 Nov 2026. Target tag date: 1 Dec 2026.

## v0.1.0, in scope

The scope page is the source of truth; this is the short form. Every item has an id there.

- **M1** Queue with admission: proposed, ready, claimed, done; each admitted item records who admitted it and when; atomic `next`, which refuses an agent at its owner-set limit of claimed, unfinished items.
- **M2** Durable claims, handoffs and results. SPEC-B7 participating claims use the common 120-second default TTL, requested bounds of 30 seconds through 24 hours, and immediate authenticated session-end release including dependent actors.
- **M3** Upgrade safety: backup before migrate, refuse unknown schemas, doctor flags a build and database mismatch.
- **M4** Path holds: existing behaviour, admitted fixes only.
- **M5** Transport adapter interface (`wake`, `status`, `list`), the Herdr adapter and a generic command adapter.
- **M6** Native hooks bind contract-minted actors with private adapter grants. Participating edits fail closed; routine output is limited to conflict notices and one idle release reminder. Presence never renews files. Cursor stays installed and unguarded. SPEC-B7 supersedes D96; see [docs/HOOKS.md](docs/HOOKS.md).
- **M7** One place that lists what needs the owner; the read-only scan for lapsed resources and unregistered listeners in `another doctor`.
- **M8** Daemon on macOS and Linux.
- **M9** Today's hub and spoke between the owner's Mac and VPS, with hub backups.
- **M10** A release people can install.

Nice to have (N1 to N5), frozen and cut lists are on the scope page. Frozen surfaces stay in the tree, are treated as experimental, and get only admitted security and data-loss fixes.

## How work is done (mandatory)

- **No pull request without a ready queue item.** Every queue item and every pull request names one scope id. Work that names none is not agreed work.
- **Anyone can propose; only the owner, or the architect the owner delegates to, admits.** If you find a problem, record it as a proposed item and move on. Do not open a pull request for it. Security and data-loss findings go to the owner.
- **A coordinating agent reads what waits on the owner with `another needs --json`** ([docs/NEEDS.md](docs/NEEDS.md)). It may run the steps a row lists under `next_delegate`, and nothing wider: it runs no step that is only under `next`, wakes no agent on the list's account, and reports every other row to the owner.
- **Independent review only.** Nobody reviews or merges their own work. No force pushes. No admin bypass.
- **A development build never runs against the live `~/.another`.** Use `ANOTHER_HOME=$(mktemp -d)` or `--home <tmp>`. Only the installed binary touches the live home.
- **Schema and decision numbers are assigned at merge time** by the architect. Do not reserve them.
- CI green on every pull request: test, clippy `-D warnings`, fmt, MSRV 1.88, copy lint.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** and public docs of integrated tools (tmux, cmux, Herdr, GitHub Actions) only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.
- cmux is GPL: call its CLI as a separate process only; never link or vendor it.

## Locked defaults

Apache-2.0; agent names unique per project; hooks are the fallback everywhere and wake through the adapter interface is core (D84); free and open source always; loopback-only without auth; never reap without human approval; never remote reap; MSRV 1.88.

## Explicitly OUT of scope

See the non-goals in [docs/VISION.md](docs/VISION.md) and the cut list in [docs/V0.1.0-SCOPE.md](docs/V0.1.0-SCOPE.md). In short: teams and hosted multi-user setups, a plugin platform, mesh federation, an adapter per multiplexer, auto-reap, end-to-end mail encryption and a TUI.

## Keep the README status current

When a scope item's done condition is met on `main`, update the **Status** section in [README.md](README.md) in the same pull request.
