"""Unit and mock-transport tests for evals/spend_ledger.py."""
from __future__ import annotations

import json
import os
import tempfile
import threading
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
    init_ledger,
    load_provider_key,
    metered_chat_json,
    summarize_ledger,
    validate_api_key,
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
        ledger = SpendLedger(self.ledger_path, reserve_lock_wait_s=0.0)
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
            # Stuck lock must never be auto-deleted.
            self.assertTrue(Path(f"{self.ledger_path}.lock").exists())
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

    def test_require_existing_ledger_fails_closed_until_provisioned(self) -> None:
        missing = self.root / "must-exist.jsonl"
        ledger = SpendLedger(missing, require_existing=True)
        calls = 0

        def transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(200, "{}", {})

        with self.assertRaises(LedgerError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=100,
                api_key="sk-test-valid",
                transport=transport,
            )
        self.assertEqual(ctx.exception.error_class, "ledger_missing")
        self.assertEqual(calls, 0)
        self.assertFalse(missing.exists())

        # After explicit initialization, reservation succeeds.
        init_ledger(missing)
        self.assertTrue(missing.exists())
        res = ledger.reserve(
            provider="openai",
            model="gpt-5.4-nano",
            body_str='{"ok":1}',
            max_output_tokens=100,
        )
        self.assertTrue(res.reservation_id)

    def test_billing_hard_stop_survives_sibling_lock_contention_and_append_failure(self) -> None:
        # Case 1: Brief sibling lock contention during outcome write resolves via bounded retry.
        lp1 = self.root / "contended.jsonl"
        lock1 = Path(f"{lp1}.lock")
        sleep_calls = 0

        def release_lock_on_second_sleep(_delay: float) -> None:
            nonlocal sleep_calls
            sleep_calls += 1
            if sleep_calls == 2 and lock1.exists():
                os.rmdir(lock1)

        ledger1 = SpendLedger(lp1, outcome_lock_wait_s=1.0, lock_sleep_fn=release_lock_on_second_sleep)

        def transport_with_brief_lock(*_args):
            # Simulate sibling acquiring lock while our HTTP request is in flight.
            os.mkdir(lock1, 0o700)
            return HttpResponse(402, '{"error":{"message":"Payment required"}}', {})

        with self.assertRaises(BillingHardStopError):
            metered_chat_json(
                ledger=ledger1,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=100,
                api_key="sk-test-valid",
                transport=transport_with_brief_lock,
            )
        self.assertGreaterEqual(sleep_calls, 2)
        rows1 = [json.loads(line) for line in lp1.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([r["status"] for r in rows1], ["reserved", "billing_hard_stop"])
        self.assertTrue(ledger1.hard_stop_sentinel_path("openai").exists())

        # Case 2: Lock remains stuck permanently during billing outcome write.
        lp2 = self.root / "stuck-outcome.jsonl"
        lock2 = Path(f"{lp2}.lock")
        ledger2 = SpendLedger(lp2, outcome_lock_wait_s=0.0, lock_sleep_fn=lambda _d: None)

        def transport_with_stuck_lock(*_args):
            os.mkdir(lock2, 0o700)
            return HttpResponse(402, '{"error":{"message":"Payment required"}}', {})

        with self.assertRaises(BillingHardStopError):
            metered_chat_json(
                ledger=ledger2,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=100,
                api_key="sk-test-valid",
                transport=transport_with_stuck_lock,
            )
        # Even though JSONL only has the unresolved reservation, durable sentinel exists.
        self.assertTrue(ledger2.hard_stop_sentinel_path("openai").exists())
        os.rmdir(lock2)

        # Fresh process opening the same ledger AFTER the lock clears must STILL refuse spending!
        fresh = SpendLedger(lp2)
        fresh_calls = 0

        def should_not_run(*_args):
            nonlocal fresh_calls
            fresh_calls += 1
            return HttpResponse(200, "{}", {})

        with self.assertRaises(BillingHardStopError):
            metered_chat_json(
                ledger=fresh,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="s",
                user_prompt="u",
                max_output_tokens=100,
                api_key="sk-test-valid",
                transport=should_not_run,
            )
        self.assertEqual(fresh_calls, 0)
        receipt = summarize_ledger(lp2)
        self.assertEqual(receipt["totals"]["billing_hard_stops"], 1)
        self.assertEqual(receipt["hard_stop_sentinels"], 1)

        # Case 3: JSONL append itself fails (e.g. read-only ledger) during billing outcome write.
        lp3 = self.root / "readonly-outcome.jsonl"
        ledger3 = SpendLedger(lp3)

        def transport_makes_ledger_readonly(*_args):
            os.chmod(lp3, 0o400)
            return HttpResponse(402, '{"error":{"message":"Payment required"}}', {})

        try:
            with self.assertRaises(BillingHardStopError):
                metered_chat_json(
                    ledger=ledger3,
                    provider="openai",
                    model="gpt-5.4-nano",
                    system_prompt="s",
                    user_prompt="u",
                    max_output_tokens=100,
                    api_key="sk-test-valid",
                    transport=transport_makes_ledger_readonly,
                )
            self.assertTrue(ledger3.hard_stop_sentinel_path("openai").exists())
            self.assertFalse(Path(f"{lp3}.lock").exists())
        finally:
            os.chmod(lp3, 0o600)

        # Fresh process after restoring write perms still refuses spending due to durable sentinel.
        fresh3 = SpendLedger(lp3)
        with self.assertRaises(BillingHardStopError):
            fresh3.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str='{"retry":1}',
                max_output_tokens=100,
            )

    def test_malformed_keys_rejected_without_leaking_secret_or_calling_transport(self) -> None:
        secret = "SUPERSECRET_KEY_TOKEN_9942"
        bad_keys = [
            f"sk-{secret}\nX-Injected: evil",
            f"sk-{secret}\r\nX-Injected: evil",
            f"sk-{secret} extra",
            f"sk-{secret}\x00tail",
            f"sk-{secret}\u00e9",
        ]
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(200, "{}", {})

        for bad_key in bad_keys:
            with self.subTest(bad_key=repr(bad_key)):
                with self.assertRaises(KeyConfigError) as ctx_val:
                    validate_api_key(bad_key, "openai")
                self.assertEqual(ctx_val.exception.error_class, "key_invalid_format")
                self.assertNotIn(secret, str(ctx_val.exception))

                # Direct api_key argument to metered_chat_json must also reject pre-header.
                with self.assertRaises(KeyConfigError) as ctx_call:
                    metered_chat_json(
                        ledger=ledger,
                        provider="openai",
                        model="gpt-5.4-nano",
                        system_prompt="s",
                        user_prompt="u",
                        max_output_tokens=100,
                        api_key=bad_key,
                        transport=transport,
                    )
                self.assertEqual(ctx_call.exception.error_class, "key_invalid_format")
                self.assertNotIn(secret, str(ctx_call.exception))

                # Env var loader must also reject without echoing secret.
                with self.assertRaises(KeyConfigError) as ctx_env:
                    load_provider_key("openai", env={"OPENAI_API_KEY": bad_key}, home_dir=self.root)
                self.assertEqual(ctx_env.exception.error_class, "key_invalid_format")
                self.assertNotIn(secret, str(ctx_env.exception))

        # Mode-600 key file with interior newline must also reject without leaking secret.
        fake_home = self.root / "badhome"
        key_file = fake_home / ".config" / "openai" / "key"
        key_file.parent.mkdir(parents=True)
        key_file.write_text(f"sk-{secret}\nsecond-line\n", encoding="utf-8")
        os.chmod(key_file, 0o600)
        with self.assertRaises(KeyConfigError) as ctx_file:
            load_provider_key("openai", env={}, home_dir=fake_home)
        self.assertEqual(ctx_file.exception.error_class, "key_invalid_format")
        self.assertNotIn(secret, str(ctx_file.exception))

        self.assertEqual(calls, 0)
        self.assertFalse(self.ledger_path.exists())

    def test_fixture_and_live_ledgers_cannot_mix_and_receipt_excludes_simulations(self) -> None:
        live_path = self.root / "live.jsonl"
        live_ledger = SpendLedger(live_path, simulated=False)
        res = live_ledger.reserve(
            provider="openai",
            model="gpt-5.4-nano",
            body_str='{"live":1}',
            max_output_tokens=100,
        )
        live_ledger.record_outcome(res, status="completed", spent_micros=50)
        before_bytes = live_path.read_bytes()

        # Fixture ledger attempting to write into existing live ledger fails closed!
        fixture_ledger = SpendLedger(live_path, simulated=True)
        with self.assertRaises(LedgerError) as ctx:
            fixture_ledger.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str='{"sim":1}',
                max_output_tokens=100,
            )
        self.assertEqual(ctx.exception.error_class, "fixture_live_ledger_collision")
        self.assertEqual(live_path.read_bytes(), before_bytes)

        # Standalone fixture ledger excludes simulated spend from actual totals!
        sim_path = self.root / "sim.jsonl"
        sim_ledger = SpendLedger(sim_path, simulated=True)
        sim_res = sim_ledger.reserve(
            provider="openai",
            model="gpt-5.4-nano",
            body_str='{"sim":1}',
            max_output_tokens=100,
        )
        sim_ledger.record_outcome(sim_res, status="completed", spent_micros=500)
        receipt = summarize_ledger(sim_path)
        self.assertEqual(receipt["totals"]["spent_usd"], 0.0)
        self.assertEqual(receipt["totals"]["reserved_usd"], 0.0)
        self.assertEqual(receipt["totals"]["reservations"], 0)
        self.assertGreater(receipt["simulated_totals"]["spent_usd"], 0.0)
        self.assertEqual(receipt["simulated_totals"]["reservations"], 1)

    def test_http_500_does_not_retry_to_prevent_double_billing(self) -> None:
        ledger = SpendLedger(self.ledger_path)
        calls = 0

        def server_err_transport(*_args):
            nonlocal calls
            calls += 1
            return HttpResponse(500, '{"error":{"message":"internal server error"}}', {})

        with self.assertRaises(ProviderRequestError) as ctx:
            metered_chat_json(
                ledger=ledger,
                provider="openai",
                model="gpt-5.4-nano",
                system_prompt="sys",
                user_prompt="usr",
                max_output_tokens=200,
                api_key="mock-key",
                transport=server_err_transport,
                max_transient_retries=3,
                sleep_fn=lambda _s: None,
            )
        self.assertEqual(calls, 1)
        self.assertFalse(ctx.exception.retryable)
        self.assertEqual(ctx.exception.error_class, "http_500_no_retry")

    def test_concurrent_lanes_never_drop_completed_or_hard_stop_rows(self) -> None:
        shared_path = self.root / "concurrent-shared.jsonl"
        init_ledger(shared_path)

        # Pre-reserve 6 calls (5 normal completions + 1 billing hard stop), then write all
        # outcomes simultaneously from 6 threads to exercise real lock contention.
        lanes: list[tuple[SpendLedger, object, str]] = []
        for idx in range(6):
            lgr = SpendLedger(shared_path, reserve_lock_wait_s=2.0, outcome_lock_wait_s=5.0)
            res = lgr.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str=f'{{"worker":{idx}}}',
                max_output_tokens=100,
            )
            outcome = "billing_hard_stop" if idx == 5 else "completed"
            lanes.append((lgr, res, outcome))

        barrier = threading.Barrier(len(lanes))
        errors: list[Exception] = []

        def worker(lgr: SpendLedger, res, outcome: str) -> None:
            try:
                barrier.wait(timeout=5.0)
                if outcome == "billing_hard_stop":
                    lgr.record_outcome(
                        res,
                        status="billing_hard_stop",
                        spent_micros=0,
                        error_class="billing_payment_required",
                    )
                else:
                    lgr.record_outcome(res, status="completed", spent_micros=50)
            except Exception as exc:  # pragma: no cover - captured for assertion
                errors.append(exc)

        threads = [threading.Thread(target=worker, args=item) for item in lanes]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=10.0)
            self.assertFalse(t.is_alive())

        self.assertEqual(errors, [], f"Concurrent outcome writes failed: {errors}")
        self.assertFalse(Path(f"{shared_path}.lock").exists())

        receipt = summarize_ledger(shared_path)
        self.assertEqual(receipt["totals"]["reservations"], 6)
        self.assertEqual(receipt["totals"]["completed"], 5)
        self.assertEqual(receipt["totals"]["billing_hard_stops"], 1)
        self.assertEqual(receipt["hard_stop_sentinels"], 1)

        # Fresh lane must see the persisted billing hard stop and refuse new calls.
        fresh = SpendLedger(shared_path)
        with self.assertRaises(BillingHardStopError):
            fresh.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str='{"after":true}',
                max_output_tokens=100,
            )

    def test_max_ledger_usd_enforces_cumulative_cap_across_runs(self) -> None:
        shared_path = self.root / "capped-ledger.jsonl"
        init_ledger(shared_path)
        run1 = SpendLedger(shared_path, max_call_usd=0.05, max_run_usd=0.05, max_ledger_usd=0.002)
        res1 = run1.reserve(
            provider="openai",
            model="gpt-5.4-nano",
            body_str='{"first":true}',
            max_output_tokens=200,
        )
        run1.record_outcome(res1, status="completed", spent_micros=50)

        # Separate SpendLedger instance with same max_ledger_usd must reject second reservation.
        run2 = SpendLedger(shared_path, max_call_usd=0.05, max_run_usd=0.05, max_ledger_usd=0.002)
        with self.assertRaises(BudgetExceededError) as ctx:
            run2.reserve(
                provider="openai",
                model="gpt-5.4-nano",
                body_str='{"second":true}',
                max_output_tokens=200,
            )
        self.assertEqual(ctx.exception.error_class, "ledger_cap_exceeded")


if __name__ == "__main__":
    unittest.main()
