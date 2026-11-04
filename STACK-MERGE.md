# Merging the v0.1.0 stack

The order is #9 → #10 → #11 → … → #17:

| PR | Branch | Base |
|---|---|---|
| tamler/another#9 | `claude/background-service` | `main` |
| tamler/another#10 | `claude/phase1-lanes` | `main` |
| tamler/another#11–#16 | `claude/phase2-push` … `claude/phase7-remote-agents` | the previous phase |
| tamler/another#17 | `claude/v0.1.0-readiness` | `claude/phase7-remote-agents` |

#9 and #10 are siblings on `main`. #11–#17 are a chain on #10, so once #10 is in, each later PR adds only its own phase.

## Dry run (2026-11-04)

I merged each branch into a local scratch branch from `origin/main`, in order, with `git merge --no-ff`. It was not pushed. After resolving the conflicts below, the result builds and passes everything: `cargo test --workspace` (209 tests), clippy `-D warnings`, `cargo fmt --check` and `cargo +1.88 check --locked`. Nothing needed changing beyond the conflict resolutions; there was no behavioural drift between #9 and the later phases.

| Step | Result |
|---|---|
| #9 `background-service` into `main` | clean |
| #10 `phase1-lanes` | conflicts: `crates/another/src/cli.rs` (3), `docs/DECISIONS.md` (1) |
| #11 `phase2-push` | conflicts: `cli.rs` (2), `docs/REMOTE.md` (1) |
| #12 `phase3-decisions` | conflicts: `cli.rs` (2), `docs/REMOTE.md` (1) |
| #13 `phase4-lane-holds` | clean |
| #14 `phase5-spend` | conflicts: `cli.rs` (1), `docs/REMOTE.md` (1) |
| #15 `phase6-summary` | conflict: `docs/REMOTE.md` (1) |
| #16 `phase7-remote-agents` | conflicts: `cli.rs` (3), `docs/REMOTE.md` (1) |
| #17 `v0.1.0-readiness` | conflicts: `cli.rs` (2) |

All of them come from one cause: #9 added the `service` command and its docs at the same places where each phase adds its own. No conflict involves conflicting logic. **Every resolution keeps both sides.**

## How to resolve each conflict

### `crates/another/src/cli.rs`

1. **`Command` enum** (#10): keep both variants, `Service(ServiceCmd)` from #9 and then `Lane(LaneCmd)`, each with its doc comment.
2. **Subcommand enums** (#10 `LaneCmd`, #11 `WebhookCmd`, #12 `DecisionCmd`, #16 `InviteCmd`): the two sides are whole enums that git interleaved.
   - Keep `ServiceCmd` complete: its last variant, the closing `},` and `}`.
   - Then add a blank line, `#[derive(Debug, Subcommand)]`, and the new enum.
   - git puts the shared closing lines after the conflict, so they belong to the new enum.
3. **Command handlers** (#10 `lane_command`, #11 `webhook_command`, #12 `decision_command`, #14 `spend_command`): keep `fn service_command` complete, with its final `}`. Then add a blank line and the new function.
4. **Early returns in `run`** (#16, #17): keep `if let Command::Service(cmd) = &cli.command { return service_command(…); }` (closing `}` included), then the new early returns: `Join`/`Remote` from #16 and `Doctor` from #17.
5. **The "handled above" match arm** (#16, #17): one arm listing every command handled before the store opens:

   ```rust
   Command::Serve { .. }
   | Command::Hook(_)
   | Command::Service(_)
   | Command::Join { .. }
   | Command::Remote(_)
   | Command::Doctor { .. } => {
       unreachable!("handled above")
   }
   ```

   Keep the arms that follow it on the incoming side (`Invite`, `Agent(Kick)`, `Agent(Remotes)` from #16).

Run `cargo fmt` after resolving. The compiler then checks the result: a missing variant or arm shows up as a non-exhaustive match or an unclosed delimiter.

### `docs/DECISIONS.md` (#10)

Keep #9's row **D36** (background service) and then #10's **D37** (lanes); the rows are numbered in order. Later phases add D38–D44 below them without conflict.

### `docs/REMOTE.md` (#11, #12, #14, #15, #16)

The same paragraph conflicts each time.
- **#9's side:** its first line is the sentence "To keep this server running across logins and reboots, install it as an opt-in background service … (see the README section "Run it in the background")." It is followed by the original "With `--auth`, …" paragraph.
- **The incoming side:** the newer "With `--auth`, `/mcp`, `GET /activity` and the push endpoints …" paragraph, extended by each phase.

Resolve by keeping #9's background-service sentence, a blank line, then the **incoming** "With `--auth` …" paragraph. Drop #9's older copy of that paragraph.

## Alternative: fewer conflicts

If #9 merges **after** the coordination series, the conflicts move into #9 itself, in the same places and with the same resolutions. Merging #9 first, as above, means each later PR resolves its own small conflict.

Either way, run the full checks before merging the last PR, because the stack has never been built in exactly that shape in CI:

```bash
cargo test --workspace
```

```bash
cargo +stable clippy --all-targets --all-features -- -D warnings
```

```bash
cargo fmt --check
```

```bash
cargo +1.88 check --workspace --all-targets --locked
```
