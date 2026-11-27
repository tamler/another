import ast
import importlib.util
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import threading
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).with_name("coordination_eval.py")
SPEC = importlib.util.spec_from_file_location("coordination_eval", MODULE_PATH)
evals = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evals)


class LedgerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "ledger.sqlite"
        evals.Ledger.initialize(self.path, 1000, "prepaid-no-auto-recharge")
        self.ledger = evals.Ledger(self.path)

    def test_reservation_is_committed_before_dispatch(self):
        observed = []

        def transport(_prompt):
            other = evals.Ledger(self.path)
            observed.extend(other.receipts())
            return {"verdict": True}

        self.assertTrue(evals.judge(self.ledger, "openai", evals.MODELS["openai"],
                                    "fixture", transport))
        self.assertEqual(observed[0]["status"], "reserved")
        self.assertEqual(observed[0]["spent_micro_usd"], None)

    def test_per_call_cap_refuses_before_dispatch(self):
        calls = []
        with self.assertRaises(evals.Refused):
            evals.judge(self.ledger, "openai", evals.MODELS["openai"], "fixture",
                        lambda _: calls.append(1), reserve=11, cap=10)
        self.assertEqual(calls, [])
        self.assertEqual(self.ledger.receipts(), [])

    def test_insufficient_credit_refuses_before_dispatch(self):
        path = Path(self.temp.name) / "small-credit.sqlite"
        evals.Ledger.initialize(path, 500, "prepaid-no-auto-recharge")
        ledger = evals.Ledger(path)
        calls = []
        with self.assertRaises(evals.Refused):
            evals.judge(ledger, "openai", evals.MODELS["openai"], "fixture",
                        lambda _: calls.append(1), reserve=501, cap=1000)
        self.assertEqual(calls, [])
        self.assertEqual(ledger.receipts(), [])

    def test_persisted_cap_cannot_be_raised_by_caller(self):
        with self.assertRaisesRegex(evals.Refused, "ledger per-call cap"):
            self.ledger.reserve("openai", evals.MODELS["openai"], 1001, 2000)
        self.assertEqual(self.ledger.receipts(), [])

    def test_billing_fixtures_stop_ledger_without_retry(self):
        fixtures = __import__("json").loads(
            (MODULE_PATH.parent / "fixtures" / "coordination_judges.json").read_text())
        for fixture_name in ("insufficient_credit", "billing_error"):
            path = Path(self.temp.name) / f"{fixture_name}.sqlite"
            evals.Ledger.initialize(path, 1000, "prepaid-no-auto-recharge")
            ledger = evals.Ledger(path)
            calls = []

            def transport(_prompt):
                calls.append(1)
                return fixtures[fixture_name]

            with self.assertRaises(evals.Refused):
                evals.judge(ledger, "openai", evals.MODELS["openai"], "fixture", transport)
            self.assertEqual(calls, [1])
            reopened = evals.Ledger(path)
            with self.assertRaisesRegex(evals.Refused, "stopped"):
                reopened.reserve("openai", evals.MODELS["openai"], 1, 1000)
            receipt = reopened.receipts()[0]
            self.assertEqual(receipt["reserved_micro_usd"], 1000)
            self.assertIsNone(receipt["spent_micro_usd"])
            expected = ("insufficient_credit" if fixture_name == "insufficient_credit"
                        else "billing_or_provider_error")
            self.assertEqual(receipt["error_class"], expected)

    def test_billing_error_finishes_the_ledger_once(self):
        calls = []
        real_finish = self.ledger.finish

        def counted_finish(*args, **kwargs):
            calls.append(1)
            return real_finish(*args, **kwargs)

        self.ledger.finish = counted_finish
        with self.assertRaises(evals.Refused):
            evals.judge(
                self.ledger, "openai", evals.MODELS["openai"], "fixture",
                lambda _: {"error": {"type": "insufficient_credit"}},
            )
        self.assertEqual(len(calls), 1)
        receipt = self.ledger.receipts()[0]
        self.assertEqual(receipt["status"], "stopped")
        self.assertEqual(receipt["error_class"], "insufficient_credit")

    def test_judge_prompt_strips_retained_root(self):
        prompt = evals.judge_prompt([
            {"scenario": "s", "passed": False, "retained_root": "/tmp/another-eval-secret"},
        ])
        self.assertNotIn("retained_root", prompt)
        self.assertNotIn("/tmp/another-eval-secret", prompt)
        self.assertIn('"scenario": "s"', prompt)

    def test_failed_billing_stop_persistence_leaves_unresolved_call_and_blocks_retry(self):
        calls = []
        ledger = evals.Ledger(self.path)
        real_finish = ledger.finish

        def failed_finish(*args, **kwargs):
            raise sqlite3.OperationalError("disk I/O error")

        ledger.finish = failed_finish
        with self.assertRaises(sqlite3.OperationalError):
            evals.judge(
                ledger, "openai", evals.MODELS["openai"], "fixture",
                lambda _: calls.append(1) or {"error": {"type": "billing_error"}},
            )
        ledger.finish = real_finish
        self.assertEqual(calls, [1])

        reopened = evals.Ledger(self.path)
        with self.assertRaisesRegex(evals.Refused, "unresolved reservation"):
            evals.judge(reopened, "openai", evals.MODELS["openai"], "fixture",
                        lambda _: calls.append(2) or {"verdict": True})
        self.assertEqual(calls, [1])
        self.assertEqual(reopened.receipts()[0]["status"], "reserved")

    def test_malformed_and_exception_stop_without_leaking_raw_errors(self):
        for transport in (lambda _: {"verdict": "pass"},
                          lambda _: (_ for _ in ()).throw(RuntimeError("secret-key-123"))):
            path = Path(self.temp.name) / f"case-{len(list(Path(self.temp.name).glob('case-*')))}.sqlite"
            evals.Ledger.initialize(path, 1000, "prepaid-no-auto-recharge")
            ledger = evals.Ledger(path)
            with self.assertRaises(evals.Refused) as raised:
                evals.judge(ledger, "openai", evals.MODELS["openai"], "fixture", transport)
            self.assertNotIn("secret-key-123", str(raised.exception))
            self.assertNotIn("secret-key-123", repr(ledger.receipts()))
            self.assertEqual(ledger.receipts()[0]["reserved_micro_usd"], 1000)
            self.assertIsNone(ledger.receipts()[0]["spent_micro_usd"])

    def test_concurrent_reservations_allow_only_one_unsettled_call(self):
        outcomes = []
        refusals = []
        barrier = threading.Barrier(3)

        def reserve():
            barrier.wait(timeout=2)
            try:
                outcomes.append(evals.Ledger(self.path).reserve("openai", "m", 600, 600))
            except evals.Refused as error:
                outcomes.append(None)
                refusals.append(str(error))

        threads = [threading.Thread(target=reserve) for _ in range(2)]
        for thread in threads:
            thread.start()
        barrier.wait(timeout=2)
        for thread in threads:
            thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
        self.assertEqual(sum(item is not None for item in outcomes), 1)
        self.assertEqual(refusals, ["unresolved reservation blocks dispatch"])
        self.assertEqual(sum(row["reserved_micro_usd"] for row in self.ledger.receipts()), 600)

    def test_completed_reservations_still_consume_credit(self):
        call = self.ledger.reserve("openai", evals.MODELS["openai"], 600, 1000)
        self.ledger.finish(call, "fixture-completed")
        with self.assertRaisesRegex(evals.Refused, "insufficient ledger credit"):
            evals.Ledger(self.path).reserve("openai", evals.MODELS["openai"], 600, 1000)

    def test_missing_open_does_not_create_and_existing_init_refuses(self):
        missing = Path(self.temp.name) / "missing.sqlite"
        with self.assertRaises(evals.Refused):
            evals.Ledger(missing)
        self.assertFalse(missing.exists())
        with self.assertRaises(FileExistsError):
            evals.Ledger.initialize(self.path, 2000, "prepaid-no-auto-recharge")
        self.assertEqual(self.ledger.receipts(), [])


class JudgeBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "ledger.sqlite"
        evals.Ledger.initialize(self.path, 1000, "prepaid-no-auto-recharge")
        self.ledger = evals.Ledger(self.path)

    def test_live_judge_refuses_without_environment_or_file_reads(self):
        with mock.patch.dict(os.environ, {"OPENAI_API_KEY": "sentinel"}, clear=True), \
             mock.patch.object(Path, "read_text", side_effect=AssertionError("file read")), \
             mock.patch.object(evals.Ledger, "__init__", side_effect=AssertionError("ledger read")):
            with self.assertRaises(evals.Refused):
                evals.live_judge()

    def test_no_network_or_key_imports_in_module(self):
        tree = ast.parse(MODULE_PATH.read_text())
        forbidden = {"urllib", "http", "http.client", "ssl", "socket", "requests", "httpx"}
        imports = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imports.update(alias.name for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.module:
                imports.add(node.module)
        self.assertTrue(imports.isdisjoint(forbidden), imports & forbidden)
        source = MODULE_PATH.read_text()
        self.assertNotIn("OPENAI_API_KEY", source)
        self.assertNotIn("ANTHROPIC_API_KEY", source)
        self.assertNotIn("ANTHROPIC_JUDGE_API_KEY", source)

    def test_url_transport_is_rejected_before_reservation(self):
        with self.assertRaises(evals.Refused):
            evals.judge(self.ledger, "openai", evals.MODELS["openai"], "fixture",
                        "https://example.invalid")
        self.assertEqual(self.ledger.receipts(), [])

    def test_unknown_model_and_oversized_prompt_refuse_before_reservation(self):
        for model, prompt in (("gpt-5-mini", "ok"), (evals.MODELS["openai"], "x" * 16_385)):
            with self.assertRaises(evals.Refused):
                evals.judge(self.ledger, "openai", model, prompt, lambda _: {"verdict": True})
        self.assertEqual(self.ledger.receipts(), [])

    def test_judge_cannot_override_scenario_failure(self):
        self.assertFalse(evals.score([{"passed": False}], [True, True]))
        self.assertTrue(evals.score([{"passed": True}], [True]))

    def test_cleanup_refuses_outside_recorded_root(self):
        outside = Path(self.temp.name) / "important"
        outside.mkdir()
        (outside / "keep").write_text("preserve")
        with self.assertRaises(evals.Refused):
            evals.remove_run_root(outside, Path(self.temp.name) / "another-eval-owned")
        self.assertTrue((outside / "keep").exists())

    def test_timeout_termination_is_bounded_and_reports_survivor(self):
        class FakeProcess:
            pid = 987654
            returncode = None

            def wait(self, timeout):
                self.asserted_timeout = timeout
                raise subprocess.TimeoutExpired("fake", timeout)

        process = FakeProcess()
        with mock.patch.object(evals.os, "killpg") as killpg:
            self.assertEqual(evals.terminate_owned(process, grace=0.01), "timeout-unterminated")
        killpg.assert_called_once_with(process.pid, signal.SIGTERM)
        self.assertEqual(process.asserted_timeout, 0.01)

    def test_interrupted_scenario_retains_root_and_stops_owned_group(self):
        run_root = Path(self.temp.name) / "another-eval-interrupted"
        run_root.mkdir()

        class InterruptedProcess:
            pid = 987655

            def wait(self, timeout=None):
                raise KeyboardInterrupt

        process = InterruptedProcess()
        with mock.patch.object(evals.tempfile, "mkdtemp", return_value=str(run_root)), \
             mock.patch.object(evals.subprocess, "Popen", return_value=process), \
             mock.patch.object(evals, "terminate_owned", return_value="timeout-unterminated") as stop, \
             mock.patch.object(evals, "remove_run_root") as cleanup:
            with self.assertRaises(KeyboardInterrupt):
                evals.run_scenario("fixture_scenario", timeout=1)
        stop.assert_called_once_with(process)
        cleanup.assert_not_called()
        self.assertTrue(run_root.exists())

    def test_child_environment_does_not_inherit_provider_secrets(self):
        run_root = Path(self.temp.name) / "another-eval-env"
        run_root.mkdir()

        class CompletedProcess:
            pid = 987656
            returncode = 1

            def wait(self, timeout=None):
                return 1

        process = CompletedProcess()
        with mock.patch.dict(os.environ, {"OPENAI_API_KEY": "secret-sentinel",
                                          "ANTHROPIC_JUDGE_API_KEY": "secret-sentinel-2",
                                          "ANOTHER_ADMIT_KEY": "secret-sentinel-3",
                                          "PATH": os.environ.get("PATH", "")}, clear=True), \
             mock.patch.object(evals.tempfile, "mkdtemp", return_value=str(run_root)), \
             mock.patch.object(evals.subprocess, "Popen", return_value=process) as popen:
            evals.run_scenario("fixture_scenario", timeout=1)
        child_env = popen.call_args.kwargs["env"]
        self.assertEqual(child_env["PATH"], os.environ["PATH"])
        self.assertNotIn("OPENAI_API_KEY", child_env)
        self.assertNotIn("ANTHROPIC_JUDGE_API_KEY", child_env)
        self.assertNotIn("ANOTHER_ADMIT_KEY", child_env)

    def test_failed_and_launch_error_scenarios_retain_diagnostic_logs(self):
        for launch_error in (False, True):
            root = Path(self.temp.name) / f"another-eval-failure-{launch_error}"
            root.mkdir()
            process = mock.Mock(returncode=101)
            process.wait.return_value = 101
            with mock.patch.object(evals.tempfile, "mkdtemp", return_value=str(root)), \
                 mock.patch.object(evals.subprocess, "Popen", return_value=process,
                                   side_effect=OSError("private-provider-text") if launch_error else None), \
                 mock.patch.object(evals, "remove_run_root") as cleanup:
                result = evals.run_scenario(evals.SCENARIOS[0])
            self.assertFalse(result["passed"])
            self.assertEqual(result["retained_root"], str(root))
            self.assertNotIn("private-provider-text", repr(result))
            cleanup.assert_not_called()
            self.assertTrue((root / "output.log").exists())


if __name__ == "__main__":
    unittest.main()
