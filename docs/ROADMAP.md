# Roadmap

What's planned after the current release. Product vision lives in [SPEC.md](SPEC.md); decisions in [DECISIONS.md](DECISIONS.md).

## Post-0.1.0

Work deferred out of 0.1.0. Each item names where it came from.

### From the 0.1.0 TODO sweep

The sweep ran `grep -rn "TODO\|FIXME\|XXX\|unimplemented!\|todo!" crates/` on the v0.1.0 readiness branch.

- **Markers:** none found. Nothing was deferred, and no stale markers had to be deleted.
- **`unwrap` / `expect` outside tests:** fixed in the sweep, nothing left to defer:
  - `spend::period_bounds` now returns `Result` and rejects an out-of-range month or `reset_day` as `invalid`. Before, a malformed `PeriodSel::Month` or a bad stored `reset_day` panicked.
  - Borrowing spend without a resolved agent is an `invalid` error, not a panic.
  - The remaining non-test panics are two `unreachable!` arms (`holds::patterns_overlap`, the CLI command dispatch). Code just above each one rules it out, and no input can reach it.

### v0.1.x

- Nothing scheduled yet. Add items here as `- **Area:** what and why (source: issue, PR or review)`.
