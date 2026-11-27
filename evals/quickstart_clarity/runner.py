#!/usr/bin/env python3
"""Docs and quickstart clarity eval runner (stranger walkthrough).

Supports M10 (a README and quickstart a stranger can follow).
Default mode is `fixture` (zero network, zero spend).
Live paid calls require BOTH `--mode live` and `ANOTHER_EVAL_ALLOW_LIVE=1`,
an explicit absolute spend ledger path, per-call and run caps, and fail-closed
billing hard stops.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Any

# Allow invoking as `python3 evals/quickstart_clarity/runner.py` directly.
REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from evals.quickstart_clarity.bundle import (
    DEFAULT_PUBLIC_DOCS,
    StrangerBundle,
    build_judge_prompts,
    build_stranger_bundle,
    build_stranger_prompts,
    parse_and_validate_judge_output,
    parse_and_validate_stranger_output,
)
from evals.quickstart_clarity.sandbox import verify_commands_in_sandbox
from evals.spend_ledger import (
    DEFAULT_ANTHROPIC_JUDGE_MODEL,
    DEFAULT_OPENAI_MODEL,
    DEFAULT_PER_CALL_CAP_USD,
    DEFAULT_RUN_CAP_USD,
    HttpResponse,
    SpendError,
    SpendLedger,
    TransportFn,
    default_http_transport,
    init_ledger,
    metered_chat_json,
    summarize_ledger,
    utc_now_iso,
)

DEFAULT_FIXTURE_PATH = Path(__file__).resolve().parent / "fixtures" / "recorded_run.json"


def _make_fixture_transport(fixture_data: dict[str, Any]) -> TransportFn:
    """Build a deterministic local transport from recorded fixture responses."""

    def _transport(url: str, headers: dict[str, str], body: str, timeout_s: float) -> HttpResponse:
        del headers, timeout_s
        if "api.openai.com" in url:
            payload = json.loads(body)
            sys_msg = ""
            for msg in payload.get("messages", []):
                if isinstance(msg, dict) and msg.get("role") == "system":
                    sys_msg = str(msg.get("content", ""))
            if "second-family reviewer" in sys_msg:
                fixture_resp = fixture_data["openai_judge_response"]
            else:
                fixture_resp = fixture_data["stranger_response"]
            return HttpResponse(status=200, body=json.dumps(fixture_resp), headers={})
        if "api.anthropic.com" in url:
            fixture_resp = fixture_data["anthropic_judge_response"]
            return HttpResponse(status=200, body=json.dumps(fixture_resp), headers={})
        return HttpResponse(status=404, body='{"error":{"message":"unknown mock url"}}', headers={})

    return _transport


def run_eval(
    *,
    repo_root: Path,
    ledger_path: Path,
    mode: str = "fixture",
    fixture_path: Path = DEFAULT_FIXTURE_PATH,
    player_model: str = DEFAULT_OPENAI_MODEL,
    judge_provider: str = "none",
    judge_model: str | None = None,
    max_call_usd: float = DEFAULT_PER_CALL_CAP_USD,
    max_run_usd: float = DEFAULT_RUN_CAP_USD,
    extra_docs: tuple[str, ...] = (),
    verify_bin: Path | None = None,
    no_auto_recharge_confirmed: bool = False,
    anthropic_zero_cash_confirmed: bool = False,
    transport: TransportFn | None = None,
    env: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Execute the stranger quickstart clarity eval and return the structured report."""
    source_env = os.environ if env is None else env
    if mode not in ("fixture", "live"):
        raise ValueError(f"Unsupported mode {mode!r}; expected 'fixture' or 'live'")
    if judge_provider not in ("none", "openai", "anthropic"):
        raise ValueError(f"Unsupported judge_provider {judge_provider!r}")

    if mode == "live":
        if source_env.get("ANOTHER_EVAL_ALLOW_LIVE", "").strip() != "1":
            raise RuntimeError(
                "Refusing live paid API call without ANOTHER_EVAL_ALLOW_LIVE=1 (fail closed)"
            )
        credit_only_ok = no_auto_recharge_confirmed or (
            source_env.get("ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED", "").strip() == "1"
        )
        if not credit_only_ok:
            raise RuntimeError(
                "Refusing live paid API call without confirming credit-only billing / auto-recharge disabled "
                "(--confirm-no-auto-recharge or ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED=1)"
            )

    bundle: StrangerBundle = build_stranger_bundle(
        repo_root,
        doc_paths=DEFAULT_PUBLIC_DOCS,
        extra_docs=extra_docs,
    )
    stranger_sys, stranger_user = build_stranger_prompts(bundle)

    ledger = SpendLedger(
        ledger_path,
        product="another",
        eval_name="quickstart-clarity",
        max_call_usd=max_call_usd,
        max_run_usd=max_run_usd,
        simulated=(mode == "fixture"),
        require_existing=(mode == "live"),
    )

    if mode == "fixture":
        fixture_data = json.loads(fixture_path.read_text(encoding="utf-8"))
        active_transport = transport or _make_fixture_transport(fixture_data)
        player_key: str | None = "fixture-openai-key"
        judge_key: str | None = "fixture-judge-key"
    else:
        active_transport = transport or default_http_transport
        player_key = None
        judge_key = None

    stranger_call = metered_chat_json(
        ledger=ledger,
        provider="openai",
        model=player_model,
        system_prompt=stranger_sys,
        user_prompt=stranger_user,
        max_output_tokens=3000,
        role="player",
        api_key=player_key,
        env=source_env,
        transport=active_transport,
    )
    stranger_parsed = parse_and_validate_stranger_output(stranger_call.text)

    sandbox_results: list[dict[str, Any]] | None = None
    if verify_bin is not None:
        sandbox_results = verify_commands_in_sandbox(
            verify_bin,
            stranger_parsed["proposed_commands"],
        )

    judge_report: dict[str, Any] | None = None
    judge_call_meta: dict[str, Any] | None = None
    if judge_provider != "none":
        resolved_judge_model = judge_model or (
            DEFAULT_ANTHROPIC_JUDGE_MODEL if judge_provider == "anthropic" else DEFAULT_OPENAI_MODEL
        )
        judge_sys, judge_user = build_judge_prompts(bundle, stranger_parsed, sandbox_results)
        judge_call = metered_chat_json(
            ledger=ledger,
            provider=judge_provider,
            model=resolved_judge_model,
            system_prompt=judge_sys,
            user_prompt=judge_user,
            max_output_tokens=2000,
            role="judge",
            api_key=judge_key,
            env=source_env,
            anthropic_zero_cash_confirmed=anthropic_zero_cash_confirmed or (mode == "fixture"),
            transport=active_transport,
        )
        judge_report = parse_and_validate_judge_output(judge_call.text)
        judge_call_meta = {
            "provider": judge_call.provider,
            "model": judge_call.model,
            "input_tokens": judge_call.input_tokens,
            "output_tokens": judge_call.output_tokens,
            "reserved_usd": judge_call.reserved_usd,
            "spent_usd": judge_call.spent_usd,
            "attempts": judge_call.attempts,
        }

    receipt = summarize_ledger(ledger_path)
    return {
        "eval": "quickstart-clarity",
        "scope_id": "M10",
        "mode": mode,
        "generated_at": utc_now_iso(),
        "public_docs_manifest": bundle.manifest(),
        "stranger_call": {
            "provider": stranger_call.provider,
            "model": stranger_call.model,
            "input_tokens": stranger_call.input_tokens,
            "output_tokens": stranger_call.output_tokens,
            "reserved_usd": stranger_call.reserved_usd,
            "spent_usd": stranger_call.spent_usd,
            "attempts": stranger_call.attempts,
        },
        "stranger_result": stranger_parsed,
        "sandbox_verification": sandbox_results,
        "judge_call": judge_call_meta,
        "judge_result": judge_report,
        "spend_receipt": receipt,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the Another docs/quickstart clarity eval (model plays a stranger)."
    )
    parser.add_argument(
        "--mode",
        choices=("fixture", "live"),
        default="fixture",
        help="Run against recorded fixtures (default) or live APIs (requires ANOTHER_EVAL_ALLOW_LIVE=1).",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Repository checkout root containing public docs.",
    )
    parser.add_argument(
        "--init-ledger",
        type=Path,
        default=None,
        help="Explicitly provision a durable mode-0600 spend ledger file at an absolute path and exit.",
    )
    parser.add_argument(
        "--ledger",
        type=Path,
        default=None,
        help="Absolute path to an existing durable JSONL spend ledger for live mode (or set ANOTHER_EVAL_LEDGER).",
    )
    parser.add_argument(
        "--fixture",
        type=Path,
        default=DEFAULT_FIXTURE_PATH,
        help="Recorded fixture JSON path for --mode fixture.",
    )
    parser.add_argument(
        "--player-model",
        default=DEFAULT_OPENAI_MODEL,
        help=f"OpenAI model for the stranger walkthrough (default: {DEFAULT_OPENAI_MODEL}).",
    )
    parser.add_argument(
        "--judge",
        choices=("none", "openai", "anthropic"),
        default="none",
        help="Optional second-pass judge provider (default: none).",
    )
    parser.add_argument(
        "--judge-model",
        default=None,
        help="Optional explicit judge model from MODEL_CATALOG.",
    )
    parser.add_argument(
        "--confirm-no-auto-recharge",
        action="store_true",
        help="Confirm credit-only billing with auto-recharge disabled before any live call.",
    )
    parser.add_argument(
        "--confirm-anthropic-zero-cash",
        action="store_true",
        help="Confirm Anthropic account has $0 cash and auto-recharge disabled before live judge calls.",
    )
    parser.add_argument(
        "--max-call-usd",
        type=float,
        default=DEFAULT_PER_CALL_CAP_USD,
        help=f"Per-call spend reservation cap in USD (default: {DEFAULT_PER_CALL_CAP_USD}).",
    )
    parser.add_argument(
        "--max-run-usd",
        type=float,
        default=DEFAULT_RUN_CAP_USD,
        help=f"Total run spend reservation cap in USD (default: {DEFAULT_RUN_CAP_USD}).",
    )
    parser.add_argument(
        "--extra-doc",
        action="append",
        default=[],
        help="Additional allowlisted public doc relative path (repeatable).",
    )
    parser.add_argument(
        "--verify-bin",
        type=Path,
        default=None,
        help="Optional path to an installed `another` binary to test commands in a temp ANOTHER_HOME.",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Optional file path to write the full JSON eval report.",
    )
    parser.add_argument(
        "--receipt-out",
        type=Path,
        default=None,
        help="Optional file path to write the Fleshapron spend receipt JSON.",
    )
    args = parser.parse_args(argv)

    if args.init_ledger is not None:
        try:
            provisioned = init_ledger(args.init_ledger)
        except SpendError as exc:
            print(f"error ({exc.error_class}): {exc}", file=sys.stderr)
            return 1
        print(f"initialized durable spend ledger: {provisioned}")
        return 0

    tmp_ledger_dir: tempfile.TemporaryDirectory[str] | None = None
    try:
        if args.mode == "fixture":
            if args.ledger is not None:
                print(
                    "error: --ledger cannot be used with --mode fixture; fixture runs always use "
                    "an isolated temporary ledger so simulated spend never touches a live ledger",
                    file=sys.stderr,
                )
                return 2
            # Always ignore ANOTHER_EVAL_LEDGER in fixture mode so live ledgers stay untouched.
            tmp_ledger_dir = tempfile.TemporaryDirectory(prefix="another-eval-fixture-ledger-")
            ledger_path = Path(tmp_ledger_dir.name) / "fixture-spend-ledger.jsonl"
        else:
            if args.ledger is not None:
                ledger_path = args.ledger
            elif os.environ.get("ANOTHER_EVAL_LEDGER", "").strip():
                ledger_path = Path(os.environ["ANOTHER_EVAL_LEDGER"].strip())
            else:
                print(
                    "error: live mode requires an explicit absolute --ledger or ANOTHER_EVAL_LEDGER path",
                    file=sys.stderr,
                )
                return 2

        report = run_eval(
            repo_root=args.repo_root,
            ledger_path=ledger_path,
            mode=args.mode,
            fixture_path=args.fixture,
            player_model=args.player_model,
            judge_provider=args.judge,
            judge_model=args.judge_model,
            max_call_usd=args.max_call_usd,
            max_run_usd=args.max_run_usd,
            extra_docs=tuple(args.extra_doc),
            verify_bin=args.verify_bin,
            no_auto_recharge_confirmed=args.confirm_no_auto_recharge,
            anthropic_zero_cash_confirmed=args.confirm_anthropic_zero_cash,
        )

        if args.output is not None:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        if args.receipt_out is not None:
            args.receipt_out.parent.mkdir(parents=True, exist_ok=True)
            args.receipt_out.write_text(
                json.dumps(report["spend_receipt"], indent=2) + "\n", encoding="utf-8"
            )

        summary = report["stranger_result"]["summary"]
        findings = report["stranger_result"]["findings"]
        totals = report["spend_receipt"]["totals"]
        sim_totals = report["spend_receipt"]["simulated_totals"]
        print(
            f"quickstart-clarity ({args.mode}): findings={len(findings)}, "
            f"clarity_score={summary.get('overall_clarity_score_1_to_5')}/5, "
            f"spent_usd=${totals['spent_usd']:.6f}, "
            f"simulated_usd=${sim_totals['spent_usd']:.6f}"
        )
        if report["judge_result"] is not None:
            print(
                f"judge ({args.judge}): verdict={report['judge_result']['verdict']}, "
                f"confirmed={len(report['judge_result']['confirmed_findings'])}, "
                f"rejected={len(report['judge_result']['rejected_findings'])}"
            )
        return 0
    except SpendError as exc:
        # Print sanitized error without a traceback so malformed keys can never leak to stderr.
        print(f"error ({exc.error_class}): {exc}", file=sys.stderr)
        return 1
    except (RuntimeError, ValueError, FileNotFoundError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    finally:
        if tmp_ledger_dir is not None:
            tmp_ledger_dir.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
