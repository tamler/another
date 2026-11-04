# Merging the v0.1.0 stack

Dry run of 2026-11-04, on a local scratch branch that was never pushed. It starts from this branch (tamler/another#9) and merges each later PR in order with `git merge --no-ff`.

This file replaces the root `STACK-MERGE.md` that #10 adds. That one describes an earlier #9 and lists conflicts that no longer happen.

| PR | Branch | Base |
|---|---|---|
| #9 | `claude/background-service` | `main` |
| #10 | `claude/phase1-lanes` | `main` |
| #11–#16 | `claude/phase2-push` … `claude/phase7-remote-agents` | the previous phase |
| #17 | `claude/v0.1.0-readiness` | `claude/phase7-remote-agents` |
| #18–#24 | drafts | `claude/v0.1.0-readiness` |

## Results

| Step | Tested at | Result |
|---|---|---|
| #10 `phase1-lanes` | `f2f1d5a` | 1 conflict: `docs/DECISIONS.md` |
| #11 `phase2-push` | `7b6cc0e` | clean |
| #12 `phase3-decisions` | `afa366a` | clean |
| #13 `phase4-lane-holds` | `8463f06` | clean |
| #14 `phase5-spend` | `8986578` | clean |
| #15 `phase6-summary` | `16656e9` | clean |
| #16 `phase7-remote-agents` | `ac074b2` | clean |
| #17 `v0.1.0-readiness` | `7af29ef` | clean |
| #18 `changelog-v010` | `d823ba9` | 1 conflict: `CHANGELOG.md` (also without #9) |
| #19 `todo-sweep` | `b240144` | clean |
| #20 `hardening-review` | `0f958d2` | 1 conflict: `CHANGELOG.md` (also without #9) |
| #21 `examples` | `e8c933c` | clean |
| #22 `docs-audit-v010` | `4eada80` | conflicts: `.github/workflows/ci.yml` (2), `README.md` (1), all with #21 (also without #9) |
| #23 `test-coverage-v010` | `a97b0eb` | clean, but 3 of its tests fail on the merged tree; see "Other findings" |
| #24 `todo-sweep-t1azb1` | `9e7f001` | conflicts with #19: `crates/another-core/src/spend.rs`, `docs/ROADMAP.md`, `CHANGELOG.md`. Not merged; see below |

The branches are still moving (#20 and #23 changed during this run), so repeat the dry run before merging.

Earlier, #9 conflicted with #10, #11, #12, #14, #15, #16 and #17, because it added `service` at the lines where each phase adds its own code. #9 now inserts at other places in `cli.rs` and `docs/REMOTE.md`. The move is why these things sit where they do:

- `Command::Service(_)` has its own match arm, after `Init | Status`. Don't fold it into the `Serve | Hook` arm until the stack is merged, because #16 and #17 edit that line.
- `ServiceCmd` sits before `ResourceCmd`, and `service_command` before `run`.

If #9 merges after #17 instead, the result is the same one `docs/DECISIONS.md` conflict. Before the move, that order gave 7 conflict hunks: 5 in `cli.rs` and 1 each in `docs/DECISIONS.md` and `docs/REMOTE.md`.

## Resolving the conflicts

**`docs/DECISIONS.md` (#10).** Keep #9's row D36, then #10's row D37. This is the only conflict #9 still causes. It can't be avoided, because both PRs append to the same table.

**`CHANGELOG.md` (#18, #20).** Keep the entries from both sides, then remove duplicates. #18 rewrites entries that #17 also has, and moves the database notes into `### Changed`.

**`.github/workflows/ci.yml` and `README.md` (#21 and #22).** Both PRs add a CI job at the same line and rows to the same Guides table. Keep both jobs as separate entries, each with its own `steps:`, and keep all the rows. The dry run kept #21's `ci.yml` as a shortcut and ran #22's checks by hand, so **the combined `ci.yml` has not been run**.

**#19 and #24: a choice, not a merge.** They are two TODO sweeps with incompatible changes to `period_bounds`. #19 clamps `reset_day` and keeps the signature. #24 returns `Result` and rejects bad values. Each merges cleanly on its own, but they can't both go in, so one has to be dropped or rebased onto the other. This needs a maintainer's decision.

## Changes needed after merging

These are real gaps that #9 can't fix, because the code they touch doesn't exist on this branch.

1. **Regenerate the docs after #22 (blocks CI).** `scripts/gen-docs.py --check` fails on the merged tree: `docs/CLI.md` lacks the seven `another service` sections from #9 and the `--trusted-proxy-header` flag from #20, and `docs/MCP.md` lacks #20's `agent_kick` wording. #22 goes stale from #20 alone, without #9. Run `ANOTHER_BIN=target/debug/another python3 scripts/gen-docs.py` after the last PR that changes commands or tools.
2. **Add `--trusted-proxy-header` to `another service install` after #20.** #20 adds the flag to `serve` only. A tunnelled service can't set it, so all its clients share one `/join` rate limit and one audit address. Add it to `ServiceCmd::Install` and `ServiceSpec::args`, and to the flags `update_warnings` checks.
3. **Make `another doctor` aware of the service after #17.** Doctor checks `--server`, which defaults to `http://127.0.0.1:7466`, and knows nothing about the service. On the merged tree, with the service running on port 7592 and another Another server on 7466, it warned about that other server and never mentioned the service. Its fix hints also suggest `another serve` when the service is installed but stopped. It should read the installed definition through `service::status` and check that bind.
4. **Remove the root `STACK-MERGE.md`.** Only #10 has it (`cd7a60a`, `f2f1d5a`); #11 and later don't contain those two commits. It will land with #10 unless it is dropped there.
5. **Clear the "Pending" notes #18 has about #9.** They are in `CHANGELOG.md` and `docs/release-notes-v0.1.0.md`. They should also mention the update warnings and rollback.

## Other findings

- **Three doctor tests from #23 fail against #17, with or without #9.** `bug_a_missing_wrapper_passes_when_it_is_not_the_first_word`, `bug_a_missing_wrapper_passes_when_the_path_has_an_apostrophe` and `bug_codex_trust_passes_for_all_zero_hooks` assert today's buggy result, `pass`, and get `fail`. It looks like #23 (`a97b0eb`, whose history stops at `fe4f6cc`) pins behaviour that #17's newer tip already fixed in `66100cf`, so those tests would need updating. I confirmed the failures on #23 merged onto plain `7af29ef`, but did not read each fix.
- **`/events/stream` (#11) ignores shutdown.** With a stream open, a stop takes the full 3 s grace period: 3.03 s for `serve` on SIGTERM and 3.08 s for `another service stop` on real launchd. Both exit cleanly, with a warning in the log. Ending the stream on the server's cancellation token would make the stop immediate, but isn't required.
- **`examples/peer-lanes` (#21) fails on this Mac, with or without #9.** It failed every time on the merged tree and on plain `e8c933c`: `both lanes hold their own paths: expected 'checkout,search', got ''`. The second agent's hold lands in a project with the slug `shop-2`. The temp path going through the `/var` symlink could explain that, but this is unconfirmed. The other three examples pass, and the Linux CI result was not checked here.

## Verification

Results on the merged tree, on macOS:

| Check | Through #17 | Through #23 |
|---|---|---|
| `cargo test --workspace --no-fail-fast` | 222 passed, 0 failed | 293 passed, **3 failed**: the #23 doctor tests above |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass | pass |
| `cargo fmt --all -- --check` | pass | pass |
| `cargo +1.88 check --workspace --all-targets --locked` | pass | pass |
| `scripts/gen-docs.py --check` | not on this tree | **fails**: item 1 above |
| `scripts/quickstart-ci.sh` | not on this tree | pass, but another server already held port 7466 here |
| Peer walkthrough | not on this tree | pass |
| `examples/run-all.sh` | not on this tree | **fails**: `peer-lanes` only |

Real launchd, in a temporary `HOME` and `ANOTHER_HOME` with spaces in the paths, on a non-default port:

- **#9 alone:** install, update, stop, start, `KeepAlive` respawn and uninstall all work, and nothing is left behind.
- **Merged through #17:** install, `service stop` with a stream open, uninstall, the `--auth` 401 on `/decisions`, and the update warnings all work.

Not tested:

- **Real systemd.** There is no Linux machine here, so systemd has only been run against the fake `systemctl`. `systemd-analyze verify` isn't installed on this Mac either, so that test only runs in Linux CI.
- **A reboot or login restart** on either platform.
- **Rollback against a real manager.** It has only been run against the fake managers, because forcing a real activation failure isn't practical.
- **#24 on the merged tree.** It merges cleanly alone onto #9 plus #10–#17, but the checks above were not run with it.
