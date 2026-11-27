"""Unit and mock-transport tests for evals/spend_ledger.py."""
from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path

from evals.spend_ledger import (
    BillingHardStopError,
    BudgetExceededError,
    HttpResponse,
    KeyConfigError,
    LedgerError,
    ProviderRequestError,
    SpendError,
    SpendLedger,
    _NoRedirectHandler,
    default_http_transport,
    load_provider_key,
    metered_chat_json,
    summarize_ledger,
)


class SpendLedgerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="another-spend-test-")
        self.root = Path(self.tmp.name)
        self.ledger_path = self.root / "spend.jsonl"

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_requires_absolute_ledger_path(self) -> None:
        with self.assertRaises(LedgerError):
            SpendLedger("relative/spend.jsonl")

    def test_rejects_retired_gpt5_mini_before_reserving(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(200, "{}", {})

        with self.assertRaises(SpendError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5-mini",
                system_prompt="sys",
                user_prompt="usr",
                api_key="mock-key",
                transport=transport,
            )
        self.assertEqual(ctx.exception.error_class, "retired_model")
        self.assertEqual(calls, 0)
        self.assertFalse(self.ledger_path.exists())

    def test_per_call_cap_and_run_cap_keep_denied_calls_out_of_ledger(self) -> None:
        ledger = SpendLedger(self.ledger_path, max_call_usd=0.002, max_run_usd=0.0025)
        calls = 0

        def ok_transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(
                200,
                json.dumps(
                    {
                        "choices": [{"message": {"content": '{"ok": true}'}}],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 50},
                    }
                ),
                {},
            )

        # Exceeds per-call cap ($0.002) via large max_output_tokens -> rejected pre-call.
        with self.assertRaises(BudgetExceededError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=2000,
                api_key="mock-key",
                transport=ok_transport,
            )
        self.assertEqual(ctx.exception.error_class, "per_call_cap_exceeded")
        self.assertEqual(calls, 0)
        self.assertFalse(self.ledger_path.exists())

        # First small call fits under $0.002 per-call and $0.003 run cap (~$0.00148).
        res = metered_chat_json(
            ledger=ledger,
            provider="openai",
            model="gpt-5.4-nano",
            system_prompt="s",
            user_prompt="u",
            max_output_tokens=500,
            api_key="mock-key",
            transport=ok_transport,
        )
        self.assertEqual(res.attempts, 1)
        self.assertEqual(calls, 1)
        rows_after_first = len(self.ledger_path.read_text(encoding="utf-8").splitlines())
        self.assertEqual(rows_after_first, 2)  # reserved + completed

        # Second call would push run total past $0.003 -> denied without touching transport/ledger.
        with self.assertRaises(BudgetExceededError) as ctx2:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=500,
                api_key="mock-key",
                transport=ok_transport,
            )
        self.assertEqual(ctx2.exception.error_class, "run_cap_exceeded")
        self.assertEqual(calls, 1)
        self.assertEqual(
            len(self.ledger_path.read_text(encoding="utf-8").splitlines()),
            rows_after_first,
        )

    def test_billing_errors_hard_stop_with_zero_retries_and_persist(self) -> None:
        billing_cases = [
            (
                "openai",
                "gpt-5.4-nano",
                "player",
                429,
                json.dumps(
                    {
                        "error": {
                            "message": "You exceeded your current quota, please check your plan and billing details.",
                            "type": "insufficient_quota",
                            "code": "insufficient_quota",
                        }
                    }
                ),
            ),
            (
                "openai",
                "gpt-5.4-nano",
                "player",
                402,
                json.dumps({"error": {"message": "Payment required"}}),
            ),
            (
                "anthropic",
                "claude-haiku-4-5-20251001",
                "judge",
                400,
                json.dumps(
                    {
                        "type": "error",
                        "error": {
                            "type": "invalid_request_error",
                            "message": "Your credit balance is too low to access the Anthropic API.",
                        },
                    }
                ),
            ),
        ]

        for idx, (provider, model, role, status, body) in enumerate(billing_cases):
            with self.subTest(provider=provider, status=status):
                lp = self.root / f"billing-{idx}.jsonl"
                ledger = SpendLedger(lp, max_call_usd=0.05, max_run_usd=0.25)
                calls = 0

                def failing_transport(*_args):
                    nonlocal calls
                    calls += 1
                    return HttpResponse(status, body, {})

                with self.assertRaises(BillingHardStopError):
                    metered_chat_json(
                        ledger=ledger,
                        provider=provider,
                        model=model,
                        system_prompt="sys",
                        user_prompt="usr",
                        max_output_tokens=200,
                        role=role,
                        api_key="secret-should-not-appear",
                        anthropic_zero_cash_confirmed=True,
                        transport=failing_transport,
                        max_transient_retries=3,
                        sleep_fn=lambda _s: None,
                    )
                # Zero retries on billing error!
                self.assertEqual(calls, 1)

                # Even a brand new SpendLedger instance reading the same file refuses future calls!
                ledger_reloaded = SpendLedger(lp, max_call_usd=0.05, max_run_usd=0.25)
                with self.assertRaises(BillingHardStopError):
                    metered_chat_json(
                        ledger=ledger_reloaded,
                        provider=provider,
                        model=model,
                        system_prompt="sys",
                        user_prompt="usr",
                        max_output_tokens=200,
                        role=role,
                        api_key="secret-should-not-appear",
                        anthropic_zero_cash_confirmed=True,
                        transport=failing_transport,
                        max_transient_retries=3,
                        sleep_fn=lambda _s: None,
                    )
                self.assertEqual(calls, 1)

                # Confirm key never leaked into ledger and receipt shows hard stop.
                raw_ledger = lp.read_text(encoding="utf-8")
                self.assertNotIn("secret-should-not-appear", raw_ledger)
                receipt = summarize_ledger(lp)
                self.assertEqual(receipt["totals"]["billing_hard_stops"], 1)
                self.assertEqual(receipt["entries"][0]["status"], "billing_hard_stop")
                for required_field in (
                    "time",
                    "product",
                    "provider",
                    "model",
                    "reserved_usd",
                    "spent_usd",
                    "status",
                    "error_class",
                ):
                    self.assertIn(required_field, receipt["entries"][0])

    def test_ambiguous_429_does_not_retry(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(429, "Too Many Requests", {})

        with self.assertRaises(ProviderRequestError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="sys",
                user_prompt="usr",
                max_output_tokens=200,
                api_key="mock-key",
                transport=transport,
                max_transient_retries=2,
                sleep_fn=lambda _s: None,
            )
        self.assertEqual(calls, 1)
        self.assertFalse(ctx.exception.retryable)

    def test_transient_non_billing_error_retries_under_single_reservation(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def flaky_transport(*_args):
            nonlocal calls
            calls += 1
            if calls == 1:
                return HttpResponse(503, '{"error":{"message":"service overloaded"}}', {})
            return HttpResponse(
                200,
                json.dumps(
                    {
                        "choices": [{"message": {"content": '{"recovered": true}'}}],
                        "usage": {"prompt_tokens": 50, "completion_tokens": 20},
                    }
                ),
                {},
            )

        res = metered_chat_json(
            ledger=ledger,
            provider="openai",
            model="gpt-5.4-nano",
            system_prompt="sys",
            user_prompt="usr",
            max_output_tokens=200,
            api_key="mock-key",
            transport=flaky_transport,
            max_transient_retries=1,
            sleep_fn=lambda _s: None,
        )
        self.assertEqual(calls, 2)
        self.assertEqual(res.attempts, 2)
        # Only ONE reservation row + ONE completed row in the ledger!
        rows = [json.loads(line) for line in self.ledger_path.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([r["status"] for r in rows], ["reserved", "completed"])
        self.assertEqual(rows[0]["reservation_id"], rows[1]["reservation_id"])

    def test_timeout_does_not_retry_to_prevent_double_billing(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def timeout_transport(*_args):
            nonlocal calls
            calls += 1
            raise TimeoutError("read timed out")

        with self.assertRaises(ProviderRequestError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="sys",
                user_prompt="usr",
                max_output_tokens=200,
                api_key="mock-key",
                transport=timeout_transport,
                max_transient_retries=3,
                sleep_fn=lambda _s: None,
            )
        self.assertEqual(calls, 1)
        self.assertFalse(ctx.exception.retryable)
        self.assertEqual(ctx.exception.error_class, "transient_timeout")
        rows = [json.loads(line) for line in self.ledger_path.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([r["status"] for r in rows], ["reserved", "transient_error"])

    def test_http_504_does_not_retry_to_prevent_double_billing(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def gw_timeout_transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(504, "Gateway Timeout", {})

        with self.assertRaises(ProviderRequestError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="sys",
                user_prompt="usr",
                max_output_tokens=200,
                api_key="mock-key",
                transport=gw_timeout_transport,
                max_transient_retries=3,
                sleep_fn=lambda _s: None,
            )
        self.assertEqual(calls, 1)
        self.assertFalse(ctx.exception.retryable)
        self.assertEqual(ctx.exception.error_class, "http_504_timeout_no_retry")

    def test_transport_refuses_redirects_and_unapproved_hosts(self) -> None:
        import urllib.error
        import urllib.request

        with self.assertRaises(ProviderRequestError) as ctx:
            default_http_transport("https://evil.example.com/v1/chat/completions", {}, "{}", 5.0)
        self.assertEqual(ctx.exception.error_class, "unapproved_endpoint")

        handler = _NoRedirectHandler()
        req = urllib.request.Request("https://api.openai.com/v1/chat/completions")
        with self.assertRaises(urllib.error.HTTPError) as ctx_redir:
            handler.redirect_request(req, None, 302, "Found", {}, "https://evil.example.com/steal")
        self.assertEqual(ctx_redir.exception.code, 302)

    def test_stuck_lock_and_corrupt_ledger_fail_closed(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        os.mkdir(f"{self.ledger_path}.lock", 0o700)
        try:
            with self.assertRaises(LedgerError) as ctx:
                ledger.reserve(
                    provider="openai",
                    model="gpt-5.4-nano",
                    body_str='{"a":1}',
                    max_output_tokens=100,
                )
            self.assertEqual(ctx.exception.error_class, "ledger_locked")
        finally:
            os.rmdir(f"{self.ledger_path}.lock")

        self.ledger_path.write_text("{not valid json\n", encoding="utf-8")
        with self.assertRaises(LedgerError) as ctx2:
            ledger.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str='{"a":1}',
                max_output_tokens=100,
            )
        self.assertEqual(ctx2.exception.error_class, "ledger_corrupt")

    def test_key_loader_enforces_mode_600_and_never_reads_anthropic_api_key(self) -> None:
        fake_home = self.root / "home"
        key_dir = fake_home / ".config" / "openai"
        key_dir.mkdir(parents=True)
        key_file = key_dir / "key"
        key_file.write_text("sk-test-openai\n", encoding="utf-8")

        # Mode 0644 must fail closed.
        os.chmod(key_file, 0o644)
        with self.assertRaises(KeyConfigError) as ctx:
            load_provider_key("openai", env={}, home_dir=fake_home)
        self.assertEqual(ctx.exception.error_class, "key_file_bad_mode")

        # Mode 0600 succeeds.
        os.chmod(key_file, 0o600)
        self.assertEqual(load_provider_key("openai", env={}, home_dir=fake_home), "sk-test-openai")

        # Symlink key file fails closed.
        symlink_key = self.root / "judge-link"
        os.symlink(key_file, symlink_key)
        with self.assertRaises(KeyConfigError):
            load_provider_key(
                "anthropic",
                env={"ANTHROPIC_JUDGE_KEY_FILE": str(symlink_key)},
                home_dir=fake_home,
                role="judge",
                anthropic_zero_cash_confirmed=True,
            )

        # ANTHROPIC_API_KEY must be ignored even when set!
        with self.assertRaises(KeyConfigError) as ctx_anth:
            load_provider_key(
                "anthropic",
                env={"ANTHROPIC_API_KEY": "sk-ant-must-be-ignored"},
                home_dir=fake_home,
                role="judge",
                anthropic_zero_cash_confirmed=True,
            )
        self.assertEqual(ctx_anth.exception.error_class, "key_missing")

        # Anthropic without zero-cash confirmation fails closed.
        with self.assertRaises(KeyConfigError) as ctx_zc:
            load_provider_key(
                "anthropic",
                env={"ANTHROPIC_JUDGE_API_KEY": "sk-ant-judge-only"},
                home_dir=fake_home,
                role="judge",
                anthropic_zero_cash_confirmed=False,
            )
        self.assertEqual(ctx_zc.exception.error_class, "anthropic_zero_cash_unconfirmed")

        # Anthropic in non-judge role fails closed.
        with self.assertRaises(KeyConfigError) as ctx_role:
            load_provider_key(
                "anthropic",
                env={"ANTHROPIC_JUDGE_API_KEY": "sk-ant-judge-only"},
                home_dir=fake_home,
                role="player",
                anthropic_zero_cash_confirmed=True,
            )
        self.assertEqual(ctx_role.exception.error_class, "anthropic_judge_only")

        # Anthropic judge with mode-600 ANTHROPIC_JUDGE_KEY_FILE and zero-cash confirmed succeeds.
        judge_file = self.root / "anthropic-judge.key"
        judge_file.write_text("sk-ant-judge-promo\n", encoding="utf-8")
        os.chmod(judge_file, 0o600)
        loaded = load_provider_key(
            "anthropic",
            env={"ANTHROPIC_JUDGE_KEY_FILE": str(judge_file)},
            home_dir=fake_home,
            role="judge",
            anthropic_zero_cash_confirmed=True,
        )
        self.assertEqual(loaded, "sk-ant-judge-promo")


if __name__ == "__main__":
    unittest.main()
