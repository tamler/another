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

# Bugs found by the second coverage pass

Same rules as above: recorded here, not fixed, and each test asserts today's behaviour so the fix updates it. Doctor is where most of these are. It was the least-tested area, and each of its problems is a health check that reports the wrong verdict.

## 4. Doctor passes a missing hook wrapper when it can't parse the command

**Where:** `wrapper_paths` in `another/src/doctor.rs`

**What:**
- The check reads the first shell word of each hook command and keeps it only if it ends in `another-hook.sh`. Any command it can't read that way is dropped, and an empty list means nothing is broken, so the verdict is PASS.
- **An apostrophe in the path:** `sh_quote` in `hooks.rs` writes `'` as `'\''`, and `wrapper_paths` stops at the first `'`. For an Another home like `/home/o'brien/.another`, it reads `/home/o`. This is a config Another wrote itself, so any user with an apostrophe in their home path gets no wrapper check at all.
- **A wrapper that isn't the first word**, such as `/bin/sh '/x/another-hook.sh' session-start`, is dropped the same way.
- This is the failure the check exists to catch: every hook points at a missing file, and doctor says `Codex hooks installed`.

**Reproduce:**

```bash
ANOTHER_HOME="/tmp/o'brien" another hook install --client codex
rm "/tmp/o'brien/hooks/another-hook.sh"
ANOTHER_HOME="/tmp/o'brien" another doctor   # PASS  hooks:codex
```

**Tests:** in `another/tests/doctor.rs`:
- `bug_a_missing_wrapper_passes_when_the_path_has_an_apostrophe`
- `bug_a_missing_wrapper_passes_when_it_is_not_the_first_word`

**Suggested fix:** undo `sh_quote`'s escaping when reading the word. For a command where the wrapper can't be located, report a failure or warning rather than dropping it.

## 5. Doctor reports Codex trust as recorded "for all 0" hooks

**Where:** `codex_trust` and `our_codex_hook_keys` in `another/src/doctor.rs`

**What:**
- Two parsers disagree. `our_commands` finds a wrapper command anywhere in the JSON, so the file counts as installed. `our_codex_hook_keys` only reads `hooks.<event>[].hooks[]`.
- With a command anywhere else, such as directly under the event with no group's `hooks` array, the trust check finds no hooks. No hooks missing trust counts as success: `PASS hooks:codex-trust: Codex trust recorded for all 0 of Another's hooks`.
- A person reading that would conclude holds are enforced in `codex exec`. Whether Codex runs a hook written in that shape wasn't tested here. Either way, doctor has checked nothing.

**Test:** `another/tests/doctor.rs::bug_codex_trust_passes_for_all_zero_hooks`

**Suggested fix:** when `ours` is empty, warn or fail (a config whose hooks doctor can't locate) instead of passing.

## 6. Doctor calls a hook config that isn't UTF-8 "not installed", and its fix can't run

**Where:** `check_hooks` in `another/src/doctor.rs` (`read_to_string(&path).unwrap_or_default()`)

**What:**
- Any read error gives empty text, so the file has no marker and the verdict is `WARN … hooks not installed`, with the fix `another hook install --client codex`.
- For a file that isn't UTF-8, that install then refuses to touch it and exits non-zero. The person is sent in a circle, and nothing mentions that the file is damaged.
- The check just below is written for this case ("a config that mentions the wrapper but can't be read as hooks is broken, not installed"), but an unreadable file never reaches it.
- A directory at the config path takes the same route. See `every_installed_client_is_reported_on_its_own`.

**Test:** `another/tests/doctor.rs::bug_a_hook_config_that_is_not_utf8_reads_as_not_installed`

**Suggested fix:** treat a read error other than `NotFound` as the broken-config failure.

## 7. Summary times accept a plus sign

**Where:** `parse_hhmm` in `another-core/src/summary.rs`

**What:**
- It relies on `str::parse::<u32>` for the digits, which accepts a leading `+`, and on the text being five characters long.
- `+9:00`, `09:+5` and `+1:+2` are all accepted by `another config summary_daily` and `summary schedule`, and stored as written. The first means 09:00, and `summary_schedule` then shows `+9:00` as the time.
- Low severity: the summary still goes out at the time the value parses to. It is a validation gap against the documented `HH:MM`.

**Test:** `another-core/tests/coverage_edges.rs::bug_summary_times_accept_a_plus_sign`

**Suggested fix:** require both parts to be two ASCII digits.

## Checked in the second pass and not a bug

- **Concurrent double runs behave.** With eight connections to one home released together, the daily summary goes out once, a three-use invite is redeemed exactly three times, one of the racing answers to a decision wins and its value is the one stored, and one spend release succeeds. Every loser gets a clean `forbidden` or `conflict`. Five repeat runs agreed. This shows the behaviour on this machine; it doesn't prove the absence of a race.
- **A spend lease ends at exactly its TTL.** A lease with the default hour can't be released at the one-hour mark, because the sweep has already ended it, and `release` returns `conflict`.
- **An empty period means the current one.** `""` parses as `PeriodSel::Current` on purpose.
- **The system agent can't be kicked, but it only exists once it has sent something.** Before that, kicking it returns `not_found` rather than `forbidden`. Nothing is exposed either way.
