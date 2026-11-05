# Reviewing the v0.1.0 stack

Updated 2026-11-05 after the PR renumbering and repair passes. This is a dependency map for review, not merge or release approval. Earlier dry-run SHAs, conflict resolutions and test counts described older trees; they do not establish acceptance of the current stack.

## Current PR map

| PR | Branch | Base |
|---|---|---|
| #42 | `claude/background-service` | `main` |
| #43 | `claude/phase1-lanes` | `main` |
| #44 | `claude/phase2-push` | `claude/phase1-lanes` |
| #45 | `claude/phase3-decisions` | `claude/phase2-push` |
| #46 | `claude/phase4-lane-holds` | `claude/phase3-decisions` |
| #47 | `claude/phase5-spend` | `claude/phase4-lane-holds` |
| #48 | `claude/phase6-summary` | `claude/phase5-spend` |
| #49 | `claude/phase7-remote-agents` | `claude/phase6-summary` |
| #50 | `claude/v0.1.0-readiness` | `claude/phase7-remote-agents` |
| #51-#58 | Changelog, reset-day alternatives, hardening, examples, docs and coverage | `claude/v0.1.0-readiness` |
| #59 | `launch/seo` | `main` |
| #60 | `claude/service-followups` | `claude/hardening-review` |
| #61 | `claude/docs-regen-v010` | `claude/docs-audit-v010` |
| #62 | `claude/test-coverage-r3` | `claude/test-coverage-v010` |
| #63 | `launch/seo2` | `launch/seo` |
| #64 | `launch/seo3` | `launch/seo2` |
| #65 | `claude/install-msrv-compat` | `main` |
| #66 | `claude/fix-spend-upsert` | `claude/v0.1.0-readiness` |
| #67 | `claude/test-coverage-r4` | `claude/test-coverage-r3` |
| #68 | `claude/docs-walkthrough-fixes` | `claude/docs-regen-v010` |
| #69 | `claude/name-scrub` | `claude/v0.1.0-readiness` |
| #70 | `claude/fed-optA` | `claude/test-coverage-r3` |
| #71 | `launch/seo-audit-r4` | `launch/seo3` |
| #72 | `launch/seo4` | `launch/seo3` |

Base names alone do not show every incorporated branch. In particular, #60 includes the background-service implementation from #42, and #61 includes service follow-ups from #60. Recheck the graph and actual content before combining changes.

## Dependencies that require a deliberate decision

- **Remote-agent authority:** #49 must not be accepted on its own while its P1 federation findings remain. #70 supplies the origin-authority and lease-admission work; its batch-settlement behavior needs its own exact-head review and tests. Keep this security dependency visible in any integrated candidate.
- **Reset-day alternatives:** #52 clamps stored reset days while #57 uses a fallible calculation. They overlap; choose the intended behavior before resolving that code rather than combining both blindly.
- **Service-aware doctor:** the service starts on #42, doctor starts on #50, and their integration is on #60. Release notes on a branch without those companions must describe the service conditionally.
- **Spend upserts:** include #66's refusal of fields the upsert cannot change, then regenerate references when the MCP contract changes.
- **Launch material:** #59 and its follow-ups prepare site/content changes. Source delivery does not authorize deployment, domain attachment, DNS changes or public submission.

## Repair propagation and conflicts

Repairs can be replayed as narrow patch commits without merging a base branch. A dependent head then does not contain the base's new repair commit, even when its file changes are equivalent. Its merge-base diff may include the replayed repair. Review that patch separately from the feature diff, and reconcile equivalent changes once when assembling an integrated tree.

Do not restore an older implementation simply to resolve a conflict. Keep decision timeout/savepoint behavior, local role-confirmation checks, schedule validation, recipient checks and the once-per-day failure claim together. #58 changes the same summary paths as #48, so inspect their combined behavior rather than selecting one whole file.

Likely overlap points include CHANGELOG.md, README.md, DECISIONS.md, the examples/docs CI jobs and appended doctor tests. Preserve both CI jobs as complete jobs, all relevant guide links and independently needed tests. A historical conflict list is not an exhaustive current one.

## Verification before acceptance

Freeze the then-current heads and bases, prepare the intended integrated tree, and run the full gate in [CONTRIBUTING.md](../CONTRIBUTING.md). Use a separate Cargo target for each branch under review so a binary from another branch cannot stand in for it.

Regenerate docs/CLI.md and docs/MCP.md from the binary built from that exact tree whenever commands, settings or MCP schemas change, then run scripts/gen-docs.py --check. Also exercise examples and documentation commands with isolated homes and ports. A symlinked temporary-path failure is not evidence about an unrelated repair.

Record exit statuses and concrete failures. One branch's passing gate does not establish that another head or the integrated stack passes. This document makes no current integrated test, CI, real-systemd, launchd, multi-machine federation, reboot or human-acceptance claim.

Author-only history rewrites change commit identities while preserving trees. Re-freeze review SHAs afterward; retain private before/after provenance rather than putting obsolete SHA pins in this map. Cached commits and read-only PR refs can remain available after branch rewrites.
