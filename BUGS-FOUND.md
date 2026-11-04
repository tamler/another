# Bugs found by the v0.1.0 coverage pass

Found while adding edge-case tests for the ten lowest-covered public functions (see the PR for the `cargo llvm-cov` numbers). This PR **records these and does not fix them**. Each test that shows one asserts today's behaviour and points here, so whoever fixes a bug updates its test in the same change.

## 1. A daily summary to an unknown person is accepted, then fails on every sweep

**Where:**
- `summary_to` validation in `another-core/src/settings.rs`;
- `Store::send_scheduled_summary_at` in `another-core/src/summary.rs`.

**What:**
- `another summary schedule --daily 08:30 --to mail:nobody` and `another config summary_to mail:nobody` both succeed. Validation checks only that `nobody` is a well-formed name.
- At the scheduled time, every sweep of `another serve` tries to resolve `nobody`, fails with `not_found`, rolls back, and logs `daily summary failed`. The day is never marked as sent, so this repeats on every sweep, all day, every day.
- The person never gets a summary, and nothing tells them why except a server log line.

**Reproduce:**

```bash
another summary schedule --daily 00:00 --to mail:nobody
```

Then run `another serve` and watch it log `daily summary failed` on every sweep.

**Test:** `another-core/tests/coverage_edges.rs::scheduled_summary_to_an_unknown_person_fails_every_sweep`

**Suggested fix:** resolve the agent when `summary_to` is set, and refuse an unknown one. Optionally, also mark the day as attempted, so a later failure (the agent was removed) logs once a day instead of on every sweep.

## 2. The session digest can hide urgent mail behind newer, ordinary mail

**Where:** `another/src/hooks.rs::session_digest`

**What:**
- The session-start digest lists the five newest unread messages (`InboxQuery { limit: 5 }`), whatever their importance.
- Once five newer normal messages arrive, an urgent one drops out of the list. The header still says `(1 urgent)`, or `(2 urgent)` when counting high, but the agent can't see which message it is or who sent it.
- The digest is Another's delivery guarantee for agents that don't poll. Urgent mail is exactly what it must not drop.

**Test:** `another/tests/coverage_edges.rs::the_session_digest_flags_importance_and_caps_requests_at_five`

**Suggested fix:** list unread high and urgent messages first (the inbox already has `urgent_only`), then fill up to five with the newest of the rest. Or always include every urgent message and note "+N more".

## 3. Documentation gap: path globs don't cover projects mirrored from other nodes

**Where:**
- `docs/REMOTE.md` (token globs);
- `docs/FEDERATION.md`;
- the mirrored decisions and lanes in `another-core/src/decisions.rs` and `lanes.rs`.

**What:**
- A decision or lane mirrored from another node keeps that node's project, with that node's path (for example `/src/shop` on the laptop).
- A hub token scoped by the hub's path (`/work/shop`) therefore doesn't see mirrors from the laptop's `shop`. A token scoped by the slug (`shop`) sees both.
- This is consistent with "globs match a project's path or slug" (REMOTE.md), so it isn't a code bug. But neither doc says that **path globs are node-local**, and that a manager on a hub should use slug globs to see federated work.

**Test:** `another/tests/coverage_edges.rs::mirrored_decisions_are_found_by_id_only_within_scope` (path glob: hidden; slug glob: visible)

**Suggested fix:** one sentence in REMOTE.md ("Project paths are per node; use slug globs to cover a project across federated nodes") and in FEDERATION.md's section on mirrors.

## Checked and not a bug

- **"Urgent" counts high-importance mail.** `urgent_unread` and the `urgent_only` filter both count `high` and `urgent`, consistently, so the digest header follows that rule.
- **`remote_agents` coverage didn't move** (79.5% of regions before and after). Its uncovered regions are the `?` error arms of row decoding, which only fire on a corrupt database. The new presence test (online → idle → gone, persistent versus ephemeral, hostnames clipped to 100 characters) passes and exercises the logic, which was already covered.
