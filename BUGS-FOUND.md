# Bugs found by the v0.1.0 coverage pass

Found while adding edge-case tests for the ten lowest-covered public functions (see the PR for the `cargo llvm-cov` numbers). This PR **records these and does not fix them**. Each test that shows one asserts today's behaviour and points here, so whoever fixes a bug updates its test in the same change.

## Status (2026-11-05)

| Bug | Status |
|---|---|
| 1, 2, 6, 7 | Fixed in draft PR #25, not yet on the base. The tests here no longer depend on either side of those fixes, and #25 has its own tests for the fixed behaviour. |
| 4, 5 | Fixed on the base by `66100cf`. PR #25 also covers the remaining case of #4 described in its section. |
| 3, 8, 9, 10, 12 | Open. |
| 11 | Fixed on the federation authority branch, in a draft PR. Rows and events already rewritten are not repaired. |

## 1. A daily summary to an unknown person is accepted, then fails on every sweep

**Status:** fixed in draft PR #25.

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

**Test:** `another-core/tests/coverage_edges.rs::a_summary_to_an_unknown_person_is_never_recorded_as_sent` (holds with and without PR #25)

**Suggested fix:** resolve the agent when `summary_to` is set, and refuse an unknown one. Optionally, also mark the day as attempted, so a later failure (the agent was removed) logs once a day instead of on every sweep.

## 2. The session digest can hide urgent mail behind newer, ordinary mail

**Status:** fixed in draft PR #25.

**Where:** `another/src/hooks.rs::session_digest`

**What:**
- The session-start digest lists the five newest unread messages (`InboxQuery { limit: 5 }`), whatever their importance.
- Once five newer normal messages arrive, an urgent one drops out of the list. The header still says `(1 urgent)`, or `(2 urgent)` when counting high, but the agent can't see which message it is or who sent it.
- The digest is Another's delivery guarantee for agents that don't poll. Urgent mail is exactly what it must not drop.

**Test:** PR #25's `the_session_digest_lists_urgent_mail_before_newer_ordinary_mail`. This branch no longer asserts which five are listed.

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

**Fixed in `66100cf` on `claude/v0.1.0-readiness`.** The tests below now assert the correct result. What follows describes the bug as found.

**Remaining case (low):** a config with one recognised, healthy wrapper command and a second command that wraps a missing one, such as `/bin/sh '/gone/another-hook.sh' x`, still passes, because only recognised commands are checked. Checked with the real binary on `7af29ef`. Another's installer never writes this shape.

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
- `a_missing_wrapper_under_a_path_with_an_apostrophe_fails`
- `a_wrapper_that_is_not_the_first_word_fails`

**Suggested fix:** undo `sh_quote`'s escaping when reading the word. For a command where the wrapper can't be located, report a failure or warning rather than dropping it.

## 5. Doctor reports Codex trust as recorded "for all 0" hooks

**Fixed in `66100cf` on `claude/v0.1.0-readiness`.** A command in that shape now fails the hook check, so the trust check doesn't run. What follows describes the bug as found.

**Where:** `codex_trust` and `our_codex_hook_keys` in `another/src/doctor.rs`

**What:**
- Two parsers disagree. `our_commands` finds a wrapper command anywhere in the JSON, so the file counts as installed. `our_codex_hook_keys` only reads `hooks.<event>[].hooks[]`.
- With a command anywhere else, such as directly under the event with no group's `hooks` array, the trust check finds no hooks. No hooks missing trust counts as success: `PASS hooks:codex-trust: Codex trust recorded for all 0 of Another's hooks`.
- A person reading that would conclude holds are enforced in `codex exec`. Whether Codex runs a hook written in that shape wasn't tested here. Either way, doctor has checked nothing.

**Test:** `another/tests/doctor.rs::a_hook_outside_a_group_fails_and_gets_no_trust_verdict`

**Suggested fix:** when `ours` is empty, warn or fail (a config whose hooks doctor can't locate) instead of passing.

## 6. Doctor calls a hook config that isn't UTF-8 "not installed", and its fix can't run

**Where:** `check_hooks` in `another/src/doctor.rs` (`read_to_string(&path).unwrap_or_default()`)

**What:**
- Any read error gives empty text, so the file has no marker and the verdict is `WARN … hooks not installed`, with the fix `another hook install --client codex`.
- For a file that isn't UTF-8, that install then refuses to touch it and exits non-zero. The person is sent in a circle, and nothing mentions that the file is damaged.
- The check just below is written for this case ("a config that mentions the wrapper but can't be read as hooks is broken, not installed"), but an unreadable file never reaches it.
- A directory at the config path takes the same route. See `every_installed_client_is_reported_on_its_own`.
- So does a config the user can't read (mode 000), checked with the real binary on `7af29ef`: `WARN … hooks not installed`.

**Test:** `another/tests/doctor.rs::a_hook_config_that_is_not_utf8_never_passes_and_the_installer_leaves_it_alone`

**Suggested fix:** treat a read error other than `NotFound` as the broken-config failure.

## 7. Summary times accept a plus sign

**Where:** `parse_hhmm` in `another-core/src/summary.rs`

**What:**
- It relies on `str::parse::<u32>` for the digits, which accepts a leading `+`, and on the text being five characters long.
- `+9:00`, `09:+5` and `+1:+2` are all accepted by `another config summary_daily` and `summary schedule`, and stored as written. The first means 09:00, and `summary_schedule` then shows `+9:00` as the time.
- Low severity: the summary still goes out at the time the value parses to. It is a validation gap against the documented `HH:MM`.

**Test:** `another-core/tests/coverage_edges.rs::a_summary_time_with_a_plus_sign_is_refused_or_kept_as_written`

**Suggested fix:** require both parts to be two ASCII digits.

## 8. `hook install` writes a relative wrapper path for a relative Another home

**Where:** the hook command built in `another/src/hooks.rs` (`HookEnv`, `sh_quote`)

**What:**
- With `ANOTHER_HOME=relative-home`, the install writes `'relative-home/hooks/another-hook.sh' session-start …`. A client runs that from its own working directory, where the path doesn't resolve.
- Doctor reports this as `FAIL … mentions another-hook.sh but no hook command runs it`. Failing is right, but the message doesn't name the cause, and the reinstall it suggests writes the same relative path.
- Low severity: it needs a relative `ANOTHER_HOME`.

**Test:** `another/tests/doctor.rs::bug_hook_install_writes_a_relative_wrapper_for_a_relative_home`

**Suggested fix:** make the home absolute when building `HookEnv`, or refuse a relative one.

## 9. A refused late answer rolls back the sweep that expired the decision

**Where:** `Store::answer_decision` in `another-core/src/decisions.rs`

**What:**
- The answer runs `sweep_tx` inside its transaction, and a `conflict` return rolls that back. So the default, its event and its mail aren't committed by the call that reported the decision as closed.
- `cancel_decision` had the same shape, and `afa366a` fixed it there with a nested `Ok(Err(..))`.
- Low severity: nothing is lost. The next sweeping read applied the default exactly once in the test, but without a running server that may be some time later.

**Reproduce:** with a `choose_default` decision past its deadline, `answer_decision` returns `conflict`, and `events` shows no `decision.expired` until something sweeps.

**Test:** none on this branch. Checked with a throwaway test on `7af29ef`.

**Suggested fix:** use the same nested result as `cancel_decision`.

## 10. Spend on an account archived during the window leaves the daily summary

**Where:** `Store::daily_summary` in `another-core/src/summary.rs`

**What:**
- The summary's spend lines come from `spend_report`, which skips archived accounts, so money recorded today on an account archived today has no line and isn't flagged.
- This predates `b4e6d8c`, which correctly stopped that spend being attributed to a same-named replacement.
- Low severity: the entries remain in the ledger.

**Test:** none on this branch. Checked with a throwaway test on `7af29ef`.

**Suggested fix:** add a line for window spend on archived accounts, labelled as archived.

## Checked in the second pass and not a bug

- **Concurrent double runs behave.** With eight connections to one home released together, the daily summary goes out once, a three-use invite is redeemed exactly three times, one of the racing answers to a decision wins and its value is the one stored, and one spend release succeeds. Every loser gets a clean `forbidden` or `conflict`. Five repeat runs agreed. This shows the behaviour on this machine; it doesn't prove the absence of a race.
- **A spend lease ends at exactly its TTL.** A lease with the default hour can't be released at the one-hour mark, because the sweep has already ended it, and `release` returns `conflict`.
- **An empty period means the current one.** `""` parses as `PeriodSel::Current` on purpose.
- **The system agent can't be kicked, but it only exists once it has sent something.** Before that, kicking it returns `not_found` rather than `forbidden`. Nothing is exposed either way.

# Bugs found by the third coverage pass

Same rules: recorded here, not fixed, and each test asserts today's behaviour so the fix updates it.

## 11. A paired spoke can rewrite what belongs to the hub

**Fixed on the federation authority branch.** The table below records the historical
reproductions at `faf0edc`. The authority and HTTP regression tests now assert refusal
and unchanged state. Incoming events are validated against their signed origin before
entering the relay log; SQL guards supplement those checks. Borrow permission uses the
stored resource owner, and only that owner can adopt, stop or transfer its resource.
A spoke cannot push a borrow onto a lapsed resource or renew an expired borrower lease:
the hub refuses both before storing them, and receivers follow what the hub admitted.
Report and answer carriers are read or marked read only for the correct origin and sender.
Decision mirrors cannot borrow another node's asker, project, lane or question mail.
This prevents new violations; it does not repair previously rewritten rows or remove
forged events from an existing log.

**Where:** `project_foreign`, `foreign_lease` and `spoke_may_publish` in `another-core/src/federation.rs`; `apply_proposal_tx` in `another-core/src/lanes.rs`

**What:** `ingest` checks that an event is signed by the node it names, and that a spoke only pushes its own. It doesn't check that the signing node owns what the event describes, though many branches of `project_foreign` update rows by an id taken from the payload. `docs/FEDERATION.md` promises the opposite: each node is authoritative for its own agents, holds and resources, and events from other nodes "never rewrite another node's ownership".

**Who can do this:** a node holding a key the hub has paired. An outsider can't: unsigned, wrongly signed and unpaired events are refused, and `federation_authority.rs` tests that too. So this breaks the boundary between paired nodes, for instance a compromised or careless runner against the hub. Agent, lease and hold ids reach spokes through normal sync, so a spoke has what it needs.

Each of these was shown on a hub by ingesting properly signed events:

| Event | Effect on the hub | Missing check |
|---|---|---|
| `agent.registered` with a hub agent's id | Rewrites its program and description, and its address reads `alice@@evil` | `ON CONFLICT(agent_id)` updates whichever node owns the row |
| `message.sent` with a hub agent as sender, from a spoke with `send_mail` | The recipient's inbox shows mail from `alice@shop` that alice never sent | Only that the sender exists |
| `lane.report_proposed`, with such a mail as its carrier | A report on the hub's lane, filed as its lead | `apply_proposal_tx` doesn't check the agent belongs to the signing node. `apply_relayed_tx` does this for decisions. |
| `session.ended` or `session.started` | Marks a hub agent offline or active | No node check |
| `delivery.acked` or `delivery.read` | Marks a hub agent's mail as read, and with `delivery.acked` also acknowledged | No check that the agent belongs to the signing node |
| `resource.stopped` or `resource.reaped` | Ends every lease on a hub resource, though the resource itself is protected | The first `UPDATE` checks the node, and the second, on leases, doesn't |
| `resource.transferred` | Promotes a borrower on a hub resource to owner | No node check |
| `hold.updated` or `hold.acquired` with a hub hold's id | Rewrites its expiry, so the next sweep ends it | `ON CONFLICT(hold_id)` updates whichever node owns the row |
| `resource.borrowed` without `resource_node` | Takes a lease with no `borrow_resource` permission | The check reads `resource_node` from the payload the spoke wrote |
| `resource.adopted`, or `resource.registered` with a hub resource's id | With no permissions, takes ownership of a lapsed hub resource, even an exclusive one, and resets the clock reaping goes by | `foreign_lease` checks only that the agent and resource exist, and its `lapsed_at` update has no node check. Found by the Grok review. |
| `node.renamed` | The spoke's own entry takes the hub's name, or one with spaces and slashes, both of which pairing refuses. Lookups by name then see the clash. | No `validate_node_name` or clash check |

The tests push the first event named in each row. `session.started`, `delivery.read`, `hold.acquired` and `resource.reaped` share the same statement as the tested event, and were checked by reading the code.

**Holding correctly:** `hold.released`, `hold.renewed`, `hold.expired`, `resource.released`, `resource.renewed`, `resource.lease_expired`, `lane.closed`, `lane.status_changed` and `lane.updated` all check the node, and a spoke can't affect the hub's objects with them. `a_spoke_cannot_end_the_hubs_holds_leases_lanes_or_resources` tests this.

**Over the wire:** two of these were also run through a real hub's `POST /fed/push` with a properly signed request. The route refuses an unsigned request and accepts the signed one. Forged mail then reaches a second, ordinary spoke when it syncs, and its inbox shows it from `alice@shop@hub`, so a spoke that did nothing wrong can't tell. The other rows went through `Store::ingest`, which is what the route calls.

**Tests:** the authority rejection, ID-collision, relay, lapse-clock and carrier tests in
`another-core/tests/federation_authority.rs`, plus the real-loopback rejection, mixed-push
and sync-cursor tests in `another/tests/federation_route.rs`. Existing legitimate mail,
receipt, borrow, promotion, lane-proposal and decision flows remain positive controls.

**Suggested fix:** in `project_foreign`, compare the row's `node_id` with the event's origin before updating it, as `hold.released` already does. Add that condition to the two `ON CONFLICT` updates and the second `resource.stopped` update. Require the sender of `message.sent` and the agent in `session.*`, `delivery.*` and `lane.report_proposed` to belong to the origin. For `resource.borrowed`, look up the resource's node instead of trusting the payload. In `foreign_lease`, refuse a resource the origin doesn't own unless it's a permitted borrow. Validate names in `node.renamed`.

## 12. `spend_account_upsert` ignores fields it can't change

**Where:** `spend_account_upsert` in `another/src/mcp_spend.rs`

**What:** on an existing account, `kind`, `unit`, `period` and `project` are dropped silently, and the call returns success. An agent asking to change `unit` to `credits` gets back an account still in USD, with nothing saying its request was ignored.

**Test:** `another/tests/mcp_tools.rs::bug_upserting_an_account_ignores_fields_it_cannot_change`

**Suggested fix:** refuse a value that differs from the account's, naming the field.

## Checked in the third pass and not a bug

- **`resource_find` shows a token every project's resources.** This is deliberate and documented (`docs/REMOTE.md`, D38).
- **A spoke's holds don't appear in the hub's `holds()`.** Holds are node-local, and they surface only as warnings on acquires in a project of the same name.
