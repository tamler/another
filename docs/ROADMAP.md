# Roadmap

What is planned after v0.1.0. The scope of each milestone is in [SPEC.md](SPEC.md) and the decisions behind it are in [DECISIONS.md](DECISIONS.md). This file lists only work that is deliberately not in 0.1.0.

## Post-0.1.0

### From the pre-release TODO sweep

The sweep ran on `claude/v0.1.0-readiness` (2026-11-04):

```bash
grep -rn "TODO\|FIXME\|XXX\|unimplemented!\|todo!" crates/
```

It found **no hits**: the crates have no TODO, FIXME or XXX markers and no `unimplemented!` or `todo!` calls, so nothing was deferred, fixed or deleted from that list.

The same pass checked every `unwrap`, `expect`, `panic!` and `unreachable!` outside test code (seven sites), and every byte-range slice of a string (a UTF-8 boundary panic):

| Site | Verdict |
|---|---|
| `another-core/src/spend.rs`, `period_bounds` (two `expect`s) | **Fixed.** It panicked on a `reset_day` outside 1..=28. Writes validate that range, but the value is read back from the store. It is now clamped. Test: `out_of_range_reset_days_are_clamped_instead_of_panicking`. |
| `another-core/src/spend.rs`, `ms_of` (`expect("midnight")`) | Kept: 00:00:00 is always a valid time. |
| `another-core/src/spend.rs`, `borrow_spend_tx` (`expect("agent given")`) | Kept: `attribute` is called with `Some(agent)` and returns it resolved, or an error. |
| `another-core/src/summary.rs`, UTC offset (`expect("utc")`) | Kept: an offset of 0 is always valid. |
| `another-core/src/holds.rs`, `patterns_overlap` (`unreachable!`) | Kept: the branch is entered only when one side is an escaped literal. |
| `another/src/cli.rs`, `run` (`unreachable!("handled above")`) | Kept: those commands return before the store opens. |
| String slices in `hooks.rs` (redaction), `holds.rs` (`literal_prefix`), `activity.rs` (`@<seq>` cursors) | Kept: every offset comes from an ASCII `find` or `trim`, so it lies on a character boundary. |

No `unwrap` or `expect` remains on a path that reads CLI, MCP or HTTP input.

### v0.1.x candidates

None came out of the sweep. Fixes found by the v0.1.0 test-coverage and hardening reviews are tracked in their own PRs.

### Later (from SPEC and DECISIONS)

These were scoped out of v0.1.0 on purpose and are listed in [SPEC.md](SPEC.md) ("Later") and [DECISIONS.md](DECISIONS.md):

- **Federation mesh:** optional peer-to-peer paths once hub-and-spoke is solid. 0.1.0 is hub-and-spoke only ([FEDERATION.md](FEDERATION.md)).
- **End-to-end encryption of relayed mail** (D28). The hub that relays mail can read its bodies today, so host the hub yourself.
- **MCP resources/subscribe** for clients that act on it. Hooks remain the delivery guarantee.
- **Web UI or TUI**, a pre-commit hold guard, and auto-reap policies. Reaping stays human-confirmed.
- **Cross-repo contact directory**, an A2A bridge and Beads linkage. Requests stay inside Another for now.

Not in SPEC or DECISIONS, and also not covered in 0.1.0:

- **Windows** binaries and a Windows background service. Releases cover macOS and Linux.
