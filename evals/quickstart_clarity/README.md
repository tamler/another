# Quickstart Clarity Eval (M10)

An opt-in evaluation harness in which an LLM plays a first-time stranger trying to install Another and complete its core workflow using only public documentation (`README.md`, `site/docs/install/index.html`, `site/docs/getting-started/index.html`, and `docs/VERIFY.md`).

This eval is kept out of default CI. It runs locally by explicit command or in GitHub Actions via `workflow_dispatch` (`.github/workflows/eval-quickstart-clarity.yml`).

## What the stranger can and cannot see

`evals/quickstart_clarity/bundle.py` enforces an explicit public-doc allowlist:

- Default corpus: `README.md`, `site/docs/install/index.html` (`<main>` text only), `site/docs/getting-started/index.html` (`<main>` text only), and `docs/VERIFY.md`.
- Optional extra public docs (`--extra-doc`): only explicitly allowlisted public user guides (`docs/HOOKS.md`, `docs/HOLDS.md`, `docs/QUEUE.md`, `docs/DOCTOR.md`, `docs/DAEMON.md`, `docs/NEEDS.md`, `docs/BOARD.md`, `docs/REQUESTS.md`, `docs/ADDRESSING.md`, `docs/GLOSSARY.md`, `examples/README.md`, and public `site/docs/` pages).
- Rejected with `IsolationViolationError`: lane files (`.lanes/*`, `CHARTER.md`, `SPEND-RULES.md`, `STATUS.md`, `DONE.md`), internal engineering docs (`AGENTS.md`, `CLAUDE.md`, `docs/DECISIONS.md`, `docs/V0.1.0-SCOPE.md`, `docs/SPEC.md`), symlinks, dotfiles, and any path escaping the repo root.

## Running offline fixture tests (zero spend)

Run the full unit and fixture suite (no network calls, no API keys required):

```bash
python3 -m unittest discover -s evals/tests -v
```

Run the harness in default `fixture` mode (always uses an isolated temporary fixture ledger, ignores `ANOTHER_EVAL_LEDGER`, refuses `--ledger`, and excludes simulated usage from actual `totals.spent_usd`):

```bash
python3 evals/quickstart_clarity/runner.py --mode fixture --judge anthropic
```

## Running live opt-in evals (after merge and key setup)

Live paid API calls are disabled by default and fail closed unless all of the following are provided:

1. `--mode live`, `ANOTHER_EVAL_ALLOW_LIVE=1`, AND explicit credit-only / no-auto-recharge confirmation (`--confirm-no-auto-recharge` or `ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED=1`).
2. An explicit, **already-provisioned** absolute durable spend ledger path via `--ledger /abs/path/to/spend.jsonl` or `ANOTHER_EVAL_LEDGER=/abs/path/to/spend.jsonl`. Provision once before the first live run with `python3 evals/quickstart_clarity/runner.py --init-ledger /abs/path/to/spend.jsonl`. Live mode fails closed if the ledger file is missing rather than creating an empty ledger. On GitHub Actions, hosted runners and Actions caches are ephemeral and cannot guarantee billing-stop continuity, so `mode: live` fails closed unless a persistent runner provides an authoritative pre-existing file via `vars.ANOTHER_EVAL_DURABLE_LEDGER` outside runner temp directories.
3. Per-call (`--max-call-usd`, default `$0.10`) and run (`--max-run-usd`, default `$0.25`) reservation caps.
4. Provider keys on an authorized host or CI secret (never copied onto the `agents` server, never printed; validated against whitespace, newlines, and control characters before header construction):
   - OpenAI (default player `gpt-5.4-nano`; `gpt-5-mini` is explicitly rejected): `OPENAI_API_KEY` or a mode-`0600` file at `$HOME/.config/openai/key`.
   - Optional Anthropic second-family judge (`--judge anthropic`): `ANTHROPIC_JUDGE_KEY_FILE` (absolute path to a mode-`0600` file) or `ANTHROPIC_JUDGE_API_KEY`. `ANTHROPIC_API_KEY` is intentionally never read. Before any live Anthropic judge call, confirm `$0` cash balance and auto-recharge disabled by passing `--confirm-anthropic-zero-cash` (or `ANOTHER_EVAL_ANTHROPIC_ZERO_CASH_CONFIRMED=1`).

Example live run (with optional sandbox CLI verification against an installed binary in a temporary `ANOTHER_HOME`):

```bash
python3 evals/quickstart_clarity/runner.py --init-ledger /var/lib/another-eval/spend.jsonl

ANOTHER_EVAL_ALLOW_LIVE=1 \
ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED=1 \
ANOTHER_EVAL_LEDGER=/var/lib/another-eval/spend.jsonl \
python3 evals/quickstart_clarity/runner.py \
  --mode live \
  --player-model gpt-5.4-nano \
  --judge anthropic \
  --confirm-anthropic-zero-cash \
  --verify-bin "$(command -v another)" \
  --output /tmp/clarity-report.json \
  --receipt-out /tmp/clarity-receipt.json
```

## Spend ledger and Fleshapron receipt

`evals/spend_ledger.py` implements a shared reserve-then-spend ledger (reusable by sibling eval harnesses) modeled on `compoundinterest.ing` `analyst-budget.server.ts`:

- Durable JSONL file with an exclusive `<ledger>.lock` directory (crashed locks fail closed on reserve for operator reconciliation) and `fsync` on every append.
- Conservative pre-call reservations (never refunded on crash or timeout).
- Hard stop (`BillingHardStopError`) on any HTTP 402, `insufficient_quota`, credit-balance, or billing error with zero retries. Before acquiring the JSONL lock for the outcome write, a lock-free atomic sidecar sentinel (`<ledger>.<provider>.hard_stop`) is written and fsynced, and outcome lock acquisition retries over a bounded window so sibling contention or a JSONL append failure never loses a billing stop for future processes.
- Ambiguous HTTP 429, HTTP 504, and timeout responses also fail closed without retrying.
- Transient non-billing errors (such as HTTP 503 or explicit non-billing `rate_limit_exceeded`) retry at most once under the existing unchanged reservation.
- Every row and `--receipt-out` entry includes the weekly Fleshapron receipt fields: `time`, `product`, `provider`, `model`, `reserved_usd`, `spent_usd`, `status`, and `error_class`. Actual live spend is tracked in `totals`; simulated fixture runs are excluded from `totals` and tracked separately in `simulated_totals`.
