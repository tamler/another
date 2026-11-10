# Merging the v0.1.0 stack

Dry run of 2026-11-05, on a local scratch branch that was never pushed. It starts from this branch (tamler/another#9) and merges each later PR in the order below with `git merge --no-ff`. The branches are still moving, so repeat it before merging.

| PR | Branch | Base |
|---|---|---|
| #9 | `claude/background-service` | `main` |
| #10 | `claude/phase1-lanes` | `main` |
| #11–#16 | `claude/phase2-push` … `claude/phase7-remote-agents` | the previous phase |
| #17 | `claude/v0.1.0-readiness` | `claude/phase7-remote-agents` |
| #18–#25 | drafts | `claude/v0.1.0-readiness` |
| follow-ups | `claude/service-followups` | `claude/hardening-review`, plus a merge of #9 |
| docs | `claude/docs-regen-v010` | `claude/docs-audit-v010`, plus a merge of `claude/service-followups` |

## Why there are two extra branches

Some follow-ups can't live on any PR already in the stack, because the code they touch is on separate branches:

- `service.rs` is only on #9. `doctor.rs` starts at #17. `ForwardedHeader` and `serve --trusted-proxy-header` are only on #20.
- **`claude/service-followups`** merges #9 into #20, then adds `--trusted-proxy-header` to `another service install` and makes `another doctor` aware of the service. On #9 alone, the flag would have nothing to validate it, and a service installed with it would crash-loop, because `serve` there doesn't accept it.
- **`claude/docs-regen-v010`** holds the regenerated `docs/CLI.md` and `docs/MCP.md`. They can't go on #22, because `gen-docs.py --check` compares them with the binary built from the same tree, so #22's own check would fail.

Merge both after their parents. If a later PR changes a command or an MCP tool, run `ANOTHER_BIN=target/debug/another python3 scripts/gen-docs.py` again after it.

## Results

| Step | Tested at | Result |
|---|---|---|
| #10 `phase1-lanes` | `ad88fb0` | 1 conflict: `docs/DECISIONS.md` |
| #11 `phase2-push` | `7b6cc0e` | clean |
| #12 `phase3-decisions` | `afa366a` | clean |
| #13 `phase4-lane-holds` | `8463f06` | clean |
| #14 `phase5-spend` | `8986578` | clean |
| #15 `phase6-summary` | `16656e9` | clean |
| #16 `phase7-remote-agents` | `ac074b2` | clean |
| #17 `v0.1.0-readiness` | `7af29ef` | clean |
| #18 `changelog-v010` | `78acdfe` | 1 conflict: `CHANGELOG.md` |
| #19 `todo-sweep` | `b240144` | clean. #24 in its place also merges, with 1 conflict in `CHANGELOG.md` |
| #20 `hardening-review` | `0f958d2` | 1 conflict: `CHANGELOG.md` |
| `service-followups` | `45457ae` | clean |
| #21 `examples` | `e8c933c` | clean |
| #22 `docs-audit-v010` | `4eada80` | conflicts: `.github/workflows/ci.yml` (2), `README.md` (1), all with #21 |
| `docs-regen-v010` | `07788ae` | clean |
| #23 `test-coverage-v010` | `b2b8898` | clean |
| #25 `fix-cov-bugs` | `26f2a84` | 2 hunks in `crates/another/tests/doctor.rs`, with #23 |

#9 still needs its code where it is:

- `Command::Service(_)` has its own match arm, after `Init | Status`. Don't fold it into the `Serve | Hook` arm until the stack is merged, because #16 and #17 edit that line.
- `ServiceCmd` sits before `ResourceCmd`, and `service_command` before `run`.

## Resolving the conflicts

**`docs/DECISIONS.md` (#10).** Keep #9's row D36, then #10's row D37. This is the only conflict #9 causes. It can't be avoided, because both PRs append to the same table.

**`CHANGELOG.md` (#18, #20; #24 if chosen).** Keep the entries from both sides, then remove duplicates.

**`.github/workflows/ci.yml` (#21 and #22).** Neither side is wrong: each adds a new job after `msrv`, and git sees their shared steps as one change. Keep #21's `examples` job as it is, then add #22's `docs` job after it, complete with its own `actions/checkout`, toolchain, cache and build steps:

```yaml
  examples:
    name: examples (headless)
    runs-on: ubuntu-latest
    steps:
      # checkout, toolchain, cache and build, then "Run the four examples"

  docs:
    name: docs (quickstart, walkthrough, coverage)
    runs-on: ubuntu-latest
    steps:
      # checkout, toolchain, cache and build, then gen-docs --check, the quickstart and the walkthrough
```

**`README.md` (#21 and #22).** Both add rows to the same Guides table. Keep all the rows.

**`crates/another/tests/doctor.rs` (#23 and #25).** Both only append tests. Keep #23's, then #25's.

**#19 and #24: a choice, not a merge.** They are two TODO sweeps with incompatible changes to `period_bounds`. #19 clamps `reset_day` and keeps the signature. #24 returns `Result` and rejects bad values. Each passes the checks below in the stack without the other, but they can't both go in. This needs a maintainer's decision.

## Follow-ups

Done:

- **Docs:** `gen-docs.py --check` passes on the merged tree, with `docs-regen-v010` in it.
- **`--trusted-proxy-header`:** `another service install` takes it, and an update warns if it drops or changes the header.
- **`another doctor`:** without `--server` or `ANOTHER_SERVER`, it checks the service's address when the service uses the same home. A new `service` check reports a stopped service, a missing binary, a different home, an unreadable definition and a service manager it can't reach. The fix hints name `another service` commands when the server is the service.
- **Root `STACK-MERGE.md`:** removed from #10 in `ad88fb0`.
- **#23's failing tests:** the three doctor tests now assert the fixed result. The tests that pin what #25 changes assert only what holds with or without #25, and #25's own tests cover the fixed behaviour. #23 passes both alone and merged with #25, so the two can merge in either order.

Still open:

- **"Pending" notes about #9 in #18:** remove them from `CHANGELOG.md` and `docs/release-notes-v0.1.0.md` once #9 is in, along with the release notes' line about `another doctor` not knowing about the service, and mention the update warnings, rollback, and the doctor and proxy changes.

## Other findings

- **`examples/peer-lanes` (#21) fails on macOS because a project path can be registered twice.** `--project "$SHOP"` stores the `/var/folders/…` path as given, while commands run from that folder use the working directory, which the OS reports as `/private/var/folders/…`. Another treats them as two projects, so the second agent's hold lands in `shop-2`, and the lane lookup finds nothing. With a temp directory whose path has no symlink, all four examples pass. This isn't specific to the example: anyone who passes a symlinked `--project` path and also runs from inside it gets two projects. The Linux CI result wasn't checked here.
- **`/events/stream` (#11) ignores shutdown.** With a stream open, a stop takes the full 3 s grace period: 3.03 s for `serve` on SIGTERM and 3.08 s for `another service stop` on real launchd. Both exit cleanly, with a warning in the log. Ending the stream on the server's cancellation token would make the stop immediate, but isn't required.

## Verification

Results on the merged tree, on macOS, with #19 and then with #24 in its place:

| Check | With #19 | With #24 |
|---|---|---|
| `cargo test --workspace --no-fail-fast` | 322 passed, 0 failed | 322 passed, 0 failed |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass | pass |
| `cargo fmt --all -- --check` | pass | pass |
| `cargo +1.88 check --workspace --all-targets --locked` | pass | pass |
| `scripts/gen-docs.py --check` | pass | pass |
| `scripts/quickstart-ci.sh` | pass, but another server already held port 7466 here | not run |
| Peer walkthrough | pass | not run |
| `examples/run-all.sh` | **fails**: `peer-lanes` only, as above. Passes from a temp directory with no symlink | not run |

Real launchd, in a temporary `HOME` and `ANOTHER_HOME` with spaces in the paths, on a non-default port, on `claude/service-followups` (not the merged tree): install with `--auth` and `--trusted-proxy-header`, `doctor` passing against the service, `stop` with doctor's warning and start hint, `start`, `uninstall`, and doctor's skip afterwards. No job or listener was left behind.

Not tested:

- **The combined `ci.yml`.** It parses, and the steps of its `docs` and `examples` jobs were run by hand, but it hasn't run on GitHub. The `shellcheck` lines weren't run, because it isn't installed here.
- **Real systemd.** There is no Linux machine here, so systemd has only been run against the fake `systemctl`, including all of doctor's service checks.
- **A reboot or login restart** on either platform.
- **Rollback against a real manager.** It has only been run against the fake managers, because forcing a real activation failure isn't practical.
