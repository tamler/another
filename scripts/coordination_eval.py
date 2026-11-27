#!/usr/bin/env python3
"""Opt-in, offline coordination scenarios and fail-closed judge accounting."""

import argparse
import contextlib
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile


MODELS = {"openai": "gpt-6-luna", "anthropic": "claude-haiku-5-5"}
SCENARIOS = (
    "two_agents_racing_next_get_exactly_one_claim",
    "overlapping_exclusive_holds_have_exactly_one_winner",
    "claim_release_requires_owning_session_end_or_silent_grace",
    "admission_racing_next_never_claims_before_admission",
)
ROOT = Path(__file__).resolve().parents[1]
FIXTURES = Path(__file__).parent / "fixtures" / "coordination_judges.json"
PRODUCT = "another-coordination-evals"
CHILD_ENV = (
    "PATH", "HOME", "USERPROFILE", "RUSTUP_HOME", "CARGO_HOME",
    "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "SDKROOT", "DEVELOPER_DIR",
    "LIBRARY_PATH", "CPATH", "SYSTEMROOT",
)


class Refused(Exception):
    """A safe, fixed error message; never includes provider response text."""


def positive(value):
    if type(value) is not int or value <= 0:
        raise Refused("amount must be a positive integer in micro-USD")
    return value


class Ledger:
    """No refunds, top-ups, resets, or automatic creation on open."""

    @staticmethod
    def initialize(path, credit, attestation, cap=1000):
        path = Path(path)
        positive(credit)
        positive(cap)
        if not path.is_absolute() or attestation != "prepaid-no-auto-recharge":
            raise Refused("absolute ledger and prepaid attestation required")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.close(fd)
        # A failed initialization leaves a file that cannot be reinitialized.
        with contextlib.closing(sqlite3.connect(path)) as db:
            with db:
                db.execute("PRAGMA synchronous=FULL")
                db.executescript("""
                    CREATE TABLE budget(credit INTEGER NOT NULL, stopped INTEGER NOT NULL,
                                        attestation TEXT NOT NULL, cap INTEGER NOT NULL);
                CREATE TABLE calls(id INTEGER PRIMARY KEY, time TEXT NOT NULL,
                    product TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
                    reserved_micro_usd INTEGER NOT NULL, spent_micro_usd INTEGER,
                    status TEXT NOT NULL, error_class TEXT);
                """)
                db.execute("INSERT INTO budget VALUES (?,0,?,?)", (credit, attestation, cap))

    def __init__(self, path):
        self.path = Path(path)
        if not self.path.is_absolute() or self.path.is_symlink():
            raise Refused("ledger must be an absolute regular file")
        if not self.path.is_file() or self.path.stat().st_mode & 0o077:
            raise Refused("ledger missing or not private")

    @contextlib.contextmanager
    def connect(self):
        db = sqlite3.connect(self.path.as_uri() + "?mode=rw", uri=True, timeout=5)
        try:
            db.execute("PRAGMA busy_timeout=5000")
            db.execute("PRAGMA synchronous=FULL")
            yield db
        finally:
            db.close()

    def reserve(self, provider, model, amount, cap):
        positive(amount)
        positive(cap)
        if amount > cap:
            raise Refused("per-call cap exceeded")
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            credit, stopped, _, ledger_cap = db.execute("SELECT * FROM budget").fetchone()
            if amount > ledger_cap:
                raise Refused("ledger per-call cap exceeded")
            used = db.execute("SELECT COALESCE(SUM(reserved_micro_usd),0) FROM calls").fetchone()[0]
            if stopped:
                raise Refused("ledger stopped")
            unresolved = db.execute(
                "SELECT 1 FROM calls WHERE status='reserved' LIMIT 1"
            ).fetchone()
            if unresolved:
                raise Refused("unresolved reservation blocks dispatch")
            if amount > credit - used:
                raise Refused("insufficient ledger credit")
            cursor = db.execute(
                "INSERT INTO calls(time,product,provider,model,reserved_micro_usd,status) "
                "VALUES (?,?,?,?,?,'reserved')",
                (datetime.datetime.now(datetime.timezone.utc).isoformat(), PRODUCT,
                 provider, model, amount),
            )
            db.commit()
            return cursor.lastrowid

    def finish(self, call, status, error_class=None):
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            if error_class:
                db.execute("UPDATE budget SET stopped=1")
            # Unknown actual spend remains NULL. Reserved funds are never returned.
            db.execute("UPDATE calls SET status=?,error_class=? WHERE id=? AND status='reserved'",
                       (status, error_class, call))
            db.commit()

    def receipts(self):
        with self.connect() as db:
            db.row_factory = sqlite3.Row
            return [dict(row) for row in db.execute("SELECT * FROM calls ORDER BY id")]


def live_judge(*_args, **_kwargs):
    # Deliberately no key reads or transport. Enable only in a separate reviewed change.
    raise Refused("live calls disabled pending merged and tested spend controls")


def judge_prompt(scenarios):
    """JSON prompt for the judge, without local retained_root paths."""
    cleaned = [{key: value for key, value in row.items() if key != "retained_root"}
               for row in scenarios]
    return json.dumps(cleaned)


def judge(ledger, provider, model, prompt, transport, reserve=1000, cap=1000):
    if MODELS.get(provider) != model:
        raise Refused("unknown judge model")
    if not isinstance(prompt, str) or len(prompt.encode("utf-8")) > 16_384:
        raise Refused("judge prompt too large")
    if not callable(transport):
        raise Refused("only injected offline transports are supported")
    call = ledger.reserve(provider, model, reserve, cap)
    settled = False
    try:
        # Exactly one attempt, after durable reservation. No retry or fallback.
        response = transport(prompt)
        if not isinstance(response, dict):
            raise Refused("invalid judge response")
        if "error" in response:
            error = response["error"]
            code = error.get("type") if isinstance(error, dict) else None
            category = "insufficient_credit" if code == "insufficient_credit" else "billing_or_provider_error"
            ledger.finish(call, "stopped", category)
            settled = True
            raise Refused(category)
        if set(response) != {"verdict"} or type(response["verdict"]) is not bool:
            raise Refused("invalid judge response")
        ledger.finish(call, "fixture-completed")
        settled = True
        return response["verdict"]
    except Refused:
        if not settled:
            ledger.finish(call, "stopped", "judge_failure")
        raise
    except Exception:
        ledger.finish(call, "stopped", "uncertain_provider_failure")
        raise Refused("judge failed; reservation retained and ledger stopped") from None


def score(scenarios, verdicts):
    return bool(scenarios) and all(row["passed"] for row in scenarios) and all(verdicts)


def remove_run_root(target, recorded):
    if Path(target) != Path(recorded) or not Path(recorded).name.startswith("another-eval-"):
        raise Refused("cleanup target is not the recorded run root")
    shutil.rmtree(recorded)


def terminate_owned(process, grace=5):
    # Signal while the owned group leader is still unreaped, before PID reuse.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=grace)
        return "timeout"
    except subprocess.TimeoutExpired:
        return "timeout-unterminated"


def run_scenario(name, timeout=600):
    run_root = Path(tempfile.mkdtemp(prefix="another-eval-"))
    env = {key: os.environ[key] for key in CHILD_ENV if key in os.environ}
    env.update(ANOTHER_HOME=str(run_root / "home"), ANOTHER_EVAL_ROOT=str(run_root),
               TMPDIR=str(run_root), CARGO_TERM_COLOR="never")
    command = ["cargo", "test", "-p", "another-core", "--test", "coordination_scenarios",
               name, "--", "--ignored", "--exact", "--nocapture"]
    # Score only a bounded final tail; preserve the complete log for failed runs.
    timed_out = False
    completed_normally = False
    process = None
    passed = False
    try:
        with (run_root / "output.log").open("w+b") as output:
            process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                process.wait(timeout=timeout)
                status = "completed"
                completed_normally = True
            except subprocess.TimeoutExpired:
                timed_out = True
                status = terminate_owned(process)
            output.seek(0, os.SEEK_END)
            output.seek(max(0, output.tell() - 65_536))
            log = output.read(65_536).decode("utf-8", errors="replace")
        passed = (not timed_out and process.returncode == 0
                  and re.search(r"^test result: ok\. 1 passed; 0 failed;", log, re.M) is not None
                  and f"test {name} ... ok" in log)
        result = {"scenario": name, "passed": passed, "status": status,
                  "exit": process.returncode}
        if not passed:
            result["retained_root"] = str(run_root)
        return result
    except Exception:
        if process is not None and not completed_normally and not timed_out:
            terminate_owned(process)
        return {"scenario": name, "passed": False, "status": "launch-or-run-error",
                "exit": process.returncode if process else None, "retained_root": str(run_root)}
    except BaseException:
        if process is not None and not completed_normally and not timed_out:
            terminate_owned(process)
        print(json.dumps({"scenario": name, "passed": False, "status": "interrupted",
                          "retained_root": str(run_root)}), file=sys.stderr)
        raise
    finally:
        if completed_normally and not timed_out and passed:
            try:
                remove_run_root(run_root, run_root)
            except OSError:
                result.update(passed=False, status="cleanup-error", retained_root=str(run_root))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--second-family", action="store_true")
    args = parser.parse_args()
    if args.live:
        live_judge()
    if os.environ.get("ANOTHER_RUN_COORDINATION_EVALS") != "1":
        raise Refused("set ANOTHER_RUN_COORDINATION_EVALS=1 to run")
    fixtures = json.loads(FIXTURES.read_text())
    scenarios = []
    for name in SCENARIOS:
        row = run_scenario(name)
        scenarios.append(row)
        if "retained_root" in row:
            print(json.dumps(row), file=sys.stderr)
    with tempfile.TemporaryDirectory(prefix="another-eval-judge-") as tmp:
        path = Path(tmp) / "ledger.sqlite"
        Ledger.initialize(path, 2000, "prepaid-no-auto-recharge")
        ledger = Ledger(path)
        providers = ["openai", "anthropic"] if args.second_family else ["openai"]
        verdicts = []
        judge_error = False
        for provider in providers:
            try:
                verdicts.append(judge(ledger, provider, MODELS[provider], judge_prompt(scenarios),
                                      lambda _, p=provider: fixtures[p]))
            except (Refused, OSError, sqlite3.Error, ValueError):
                judge_error = True
                break
        report = {"mode": "offline-fixtures", "scenarios": scenarios,
                  "judge_verdicts": verdicts, "receipts": ledger.receipts(),
                  "judge_error": judge_error,
                  "passed": not judge_error and score(scenarios, verdicts)}
        print(json.dumps(report, indent=2))
        return 0 if report["passed"] else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refused, OSError, sqlite3.Error, ValueError):
        print("coordination eval refused or failed; no live call was made")
        raise SystemExit(2) from None
