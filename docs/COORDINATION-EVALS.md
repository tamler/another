# Coordination scenarios (Q43, M10)

This opt-in quality tool runs four scripted multi-agent scenarios against temporary
Another stores. It is not a v0.1.0 release requirement and does not run in default CI.
It tests queue claim races, overlapping exclusive path holds, claim release on the
owning session end or the grace rule, and admission racing a claim.

Use Python 3.11 or newer, stable Rust, and macOS or Linux. From the checkout:

```sh
python3 -m unittest discover -s scripts -p test_coordination_eval.py -v
ANOTHER_RUN_COORDINATION_EVALS=1 python3 scripts/coordination_eval.py --second-family
```

Or select **Actions / coordination scenarios / Run workflow** in GitHub. The
`workflow_dispatch` workflow uses fixtures only, requests no secrets, and has no
push or pull-request trigger. No key or live-call workflow input exists.

The wrapper runs each exact ignored Rust test separately. Exit zero, the exact
test name, and a one-test success summary are all required. A failed scenario
always makes the report fail, regardless of the judge fixture. To run just the
Rust scenarios without judging:

```sh
cargo test -p another-core --test coordination_scenarios -- --ignored
```

Actors within a scenario share one temporary home. The `next` race includes
separate processes; other races use independent SQLite connections. Scripted
assertions provide the score. The JSON judge responses are synthetic fixtures,
not recorded provider calls or evidence of model quality. The default judge
profile is OpenAI's small-task `gpt-6-luna`; `--second-family` adds the Anthropic
`claude-haiku-5-5` fixture. No model controls the actors in this version.
The current profiles follow the public [OpenAI model guide](https://developers.openai.com/api/docs/models/gpt-6-luna)
and [Claude Haiku 5.5 overview](https://platform.claude.com/docs/en/models/haiku-5-5/overview).
Rechecked 27 Nov 2026, 5:52 PM Asia/Manila: `gpt-6-luna` and `claude-haiku-5-5` are
still the public API ids. GPT-5-mini is not used. Recheck both ids before any live
transport is enabled.

Each run gets a recorded temporary root through `ANOTHER_EVAL_ROOT`, `ANOTHER_HOME`
and `TMPDIR`. Successful completed runs remove it. Failed runs retain their logs
and print the exact retained root to stderr immediately and in the final report.
A timeout sends SIGTERM to only
the process group started for that scenario and waits up to five seconds. Timeout
reports retain their exact `retained_root` for diagnosis, because reaping cargo
alone does not prove every descendant has exited. Do not remove a retained root
until its processes have exited. No broad process termination or SIGKILL is used.
The wrapper prints only fixed scenario summaries and ledger rows, not raw logs.
Compilation and assertion logs live in that root until completed-run cleanup.

## Spending controls and live gate

`--live` unconditionally refuses before opening a ledger or reading credentials.
There is no HTTP client or credential loader in this PR. Live transport enablement
is a separate reviewed change, only after these controls are merged and tested.
This is intentionally an offline harness with tested spend plumbing, not a usable
paid-provider integration. No API credit was spent building or testing it.

The ledger accepts an absolute mode-600 SQLite file created once with
`Ledger.initialize(path, credit_micro_usd, "prepaid-no-auto-recharge")`. Opening a
missing file fails; initialization refuses existing files. There is no top-up,
refund, reset, or automatic recovery command. Every reserve uses `BEGIN IMMEDIATE`,
checks the persistent hard stop and remaining credit inside the transaction, and
commits with `synchronous=FULL` before calling the injected fixture transport.
The call reserves a fixed mock amount within the persisted per-call cap. This
amount is not a price estimate and must never fund a live call. Concurrent
reservations cannot exceed the allocated credit. Every error after reservation
stops later calls, including across process restarts. Unresolved reservations also
block dispatch when a billing-stop write fails. There are no retries or provider fallbacks.

Only one call may have an outstanding reservation in a ledger. A crash before
settlement blocks later calls permanently; there is no automatic recovery path.

Crash or uncertain outcomes retain the full reservation. Actual spend remains
`null`, rather than a fabricated amount. The receipt includes UTC time, product,
provider, model, reserved micro-USD, spent micro-USD, status and fixed error class.
Divide micro-USD by 1,000,000 for weekly USD receipts. Fixture receipts are not real
spend. Normal harness runs use a disposable mock ledger and print its receipts.

Before a future live run, the owner must allocate only available prepaid credit,
confirm no auto-recharge or cash billing (Anthropic must have zero cash), and
verify current model pricing against bounded input and output. A local ledger
cannot detect other products spending the same account credit; use a dedicated
product allocation. Never assume an API key itself enforces prepaid-only billing.
Future credential paths, not read by this implementation:

- OpenAI: `OPENAI_API_KEY` or `$HOME/.config/openai/key`.
- Anthropic judge only: `ANTHROPIC_JUDGE_KEY_FILE` (mode 600) or
  `ANTHROPIC_JUDGE_API_KEY`. Never read `ANTHROPIC_API_KEY` in eval or worker code.

Never put credentials in fixtures, prompts, command arguments or reports. The
Anthropic judge path is for second-family evaluation only, never build work.
