"""Fixture and isolation tests for the quickstart clarity eval (M10)."""
from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from evals.quickstart_clarity.bundle import (
    IsolationViolationError,
    build_stranger_bundle,
    build_stranger_prompts,
)
from evals.quickstart_clarity.runner import DEFAULT_FIXTURE_PATH, REPO_ROOT, run_eval
from evals.quickstart_clarity.sandbox import verify_commands_in_sandbox
from evals.spend_ledger import BillingHardStopError, HttpResponse, LedgerError, SpendLedger, init_ledger


class QuickstartClarityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="another-clarity-test-")
        self.root = Path(self.tmp.name)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _create_isolated_decoy_repo(self) -> tuple[Path, dict[str, str]]:
        """Create a synthetic repo containing public docs plus unpublished/lane decoys."""
        repo = self.root / "repo"
        (repo / "site" / "docs" / "install").mkdir(parents=True)
        (repo / "site" / "docs" / "getting-started").mkdir(parents=True)
        (repo / "docs").mkdir(parents=True)
        (repo / ".lanes" / "lane-x").mkdir(parents=True)

        # Public docs (visible to stranger).
        (repo / "README.md").write_text(
            "# Another\n\n## Quickstart\n\n```bash\nanother init\n```\n",
            encoding="utf-8",
        )
        (repo / "site" / "docs" / "install" / "index.html").write_text(
            "<!doctype html><html><head><title>Install</title></head>"
            "<body><nav>NAV_BOILERPLATE</nav><main><h1>Install Another</h1>"
            "<pre><code>cargo install --locked --git https://github.com/tamler/another another</code></pre>"
            "</main><footer>FOOTER_BOILERPLATE</footer></body></html>",
            encoding="utf-8",
        )
        (repo / "site" / "docs" / "getting-started" / "index.html").write_text(
            "<!doctype html><html><body><main><h1>Getting Started</h1>"
            "<p>Run <code>another init</code> and register alice and bob.</p>"
            "</main></body></html>",
            encoding="utf-8",
        )
        (repo / "docs" / "VERIFY.md").write_text(
            "# Verify\n\nUse shasum -a 256 -c.\n",
            encoding="utf-8",
        )

        # Unpublished / lane decoys (MUST NEVER enter the stranger prompt).
        sentinels = {
            ".lanes/lane-x/CHARTER.md": "SENTINEL_LANE_CHARTER_SECRET_991",
            ".lanes/lane-x/SPEND-RULES.md": "SENTINEL_SPEND_RULES_SECRET_992",
            ".lanes/lane-x/STATUS.md": "SENTINEL_STATUS_SECRET_993",
            ".lanes/lane-x/DONE.md": "SENTINEL_DONE_SECRET_994",
            "AGENTS.md": "SENTINEL_AGENTS_TRIBAL_995",
            "CLAUDE.md": "SENTINEL_CLAUDE_TRIBAL_996",
            "docs/DECISIONS.md": "SENTINEL_DECISIONS_INTERNAL_997",
            "docs/V0.1.0-SCOPE.md": "SENTINEL_SCOPE_INTERNAL_998",
            "docs/SPEC.md": "SENTINEL_SPEC_HISTORICAL_999",
            "UNPUBLISHED_NOTES.md": "SENTINEL_UNPUBLISHED_NOTES_1000",
        }
        for rel, token in sentinels.items():
            target = repo / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(f"# Internal\n{token}\n", encoding="utf-8")

        # Sibling lane outside the repo root.
        outside_lane = self.root / ".lanes" / "outside"
        outside_lane.mkdir(parents=True)
        (outside_lane / "CHARTER.md").write_text("SENTINEL_OUTSIDE_LANE_1001\n", encoding="utf-8")
        sentinels["../.lanes/outside/CHARTER.md"] = "SENTINEL_OUTSIDE_LANE_1001"

        return repo, sentinels

    def test_stranger_prompt_cannot_see_lane_files_or_unpublished_notes(self) -> None:
        repo, sentinels = self._create_isolated_decoy_repo()
        bundle = build_stranger_bundle(repo)
        sys_prompt, user_prompt = build_stranger_prompts(bundle)
        combined = sys_prompt + "\n" + user_prompt

        # Confirm public content IS present and HTML boilerplate is stripped.
        self.assertIn("another init", combined)
        self.assertIn("Install Another", combined)
        self.assertIn("shasum -a 256 -c", combined)
        self.assertNotIn("NAV_BOILERPLATE", combined)
        self.assertNotIn("FOOTER_BOILERPLATE", combined)

        # Confirm ZERO unpublished / lane sentinels or host paths appear in the prompt.
        for rel, token in sentinels.items():
            self.assertNotIn(token, combined, f"Unpublished content from {rel} leaked into stranger prompt")
        self.assertNotIn(str(self.root), combined)
        self.assertNotIn(".lanes", combined)

        # Explicitly attempting to pass any lane file, unpublished note, traversal, or symlink must fail closed.
        forbidden_attempts = [
            ".lanes/lane-x/CHARTER.md",
            ".lanes/lane-x/SPEND-RULES.md",
            ".lanes/lane-x/STATUS.md",
            "AGENTS.md",
            "CLAUDE.md",
            "docs/DECISIONS.md",
            "docs/V0.1.0-SCOPE.md",
            "docs/SPEC.md",
            "UNPUBLISHED_NOTES.md",
            "../.lanes/outside/CHARTER.md",
            "/etc/passwd",
        ]
        for bad_path in forbidden_attempts:
            with self.subTest(bad_path=bad_path):
                with self.assertRaises(IsolationViolationError):
                    build_stranger_bundle(repo, extra_docs=(bad_path,))

        # Symlink masquerading as an allowlisted doc path must also fail closed.
        (repo / "docs" / "HOOKS.md").symlink_to(repo / ".lanes" / "lane-x" / "CHARTER.md")
        with self.assertRaises(IsolationViolationError):
            build_stranger_bundle(repo, extra_docs=("docs/HOOKS.md",))

    def test_captured_api_payload_contains_no_lane_or_unpublished_content(self) -> None:
        repo, sentinels = self._create_isolated_decoy_repo()
        ledger_path = self.root / "payload-check.jsonl"
        captured_bodies: list[str] = []
        fixture_data = json.loads(DEFAULT_FIXTURE_PATH.read_text(encoding="utf-8"))

        def spy_transport(url: str, headers: dict[str, str], body: str, timeout_s: float) -> HttpResponse:
            del url, headers, timeout_s
            captured_bodies.append(body)
            return HttpResponse(200, json.dumps(fixture_data["stranger_response"]), {})

        report = run_eval(
            repo_root=repo,
            ledger_path=ledger_path,
            mode="fixture",
            judge_provider="none",
            transport=spy_transport,
        )
        self.assertEqual(len(captured_bodies), 1)
        for token in sentinels.values():
            self.assertNotIn(token, captured_bodies[0])
        self.assertEqual(report["scope_id"], "M10")
        self.assertEqual(len(report["stranger_result"]["findings"]), 3)

    def test_fixture_end_to_end_with_anthropic_second_family_judge(self) -> None:
        ledger_path = self.root / "e2e-fixture.jsonl"
        report = run_eval(
            repo_root=REPO_ROOT,
            ledger_path=ledger_path,
            mode="fixture",
            judge_provider="anthropic",
        )
        self.assertEqual(report["eval"], "quickstart-clarity")
        self.assertEqual(report["scope_id"], "M10")
        self.assertEqual(report["stranger_call"]["provider"], "openai")
        self.assertEqual(report["stranger_call"]["model"], "gpt-5.4-nano")
        self.assertEqual(report["judge_call"]["provider"], "anthropic")
        self.assertEqual(report["judge_result"]["verdict"], "needs_doc_fixes")
        # Fixture simulations are strictly excluded from actual live spend totals.
        self.assertEqual(report["spend_receipt"]["totals"]["reservations"], 0)
        self.assertEqual(report["spend_receipt"]["totals"]["spent_usd"], 0.0)
        self.assertEqual(report["spend_receipt"]["simulated_totals"]["reservations"], 2)
        self.assertEqual(report["spend_receipt"]["simulated_totals"]["completed"], 2)
        self.assertGreater(report["spend_receipt"]["simulated_totals"]["spent_usd"], 0.0)

        # Verify manifest only contains public docs and no AGENTS/DECISIONS.
        manifest_paths = {item["path"] for item in report["public_docs_manifest"]}
        self.assertEqual(
            manifest_paths,
            {
                "README.md",
                "site/docs/install/index.html",
                "site/docs/getting-started/index.html",
                "docs/VERIFY.md",
            },
        )

    def test_live_mode_fails_closed_without_opt_in_env_or_credit_only_confirmation(self) -> None:
        ledger_path = self.root / "live-blocked.jsonl"
        with self.assertRaises(RuntimeError) as ctx:
            run_eval(
                repo_root=REPO_ROOT,
                ledger_path=ledger_path,
                mode="live",
                env={},
            )
        self.assertIn("ANOTHER_EVAL_ALLOW_LIVE=1", str(ctx.exception))
        self.assertFalse(ledger_path.exists())

        with self.assertRaises(RuntimeError) as ctx2:
            run_eval(
                repo_root=REPO_ROOT,
                ledger_path=ledger_path,
                mode="live",
                env={"ANOTHER_EVAL_ALLOW_LIVE": "1"},
            )
        self.assertIn("ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED=1", str(ctx2.exception))
        self.assertFalse(ledger_path.exists())

    def test_runner_stops_immediately_on_billing_error_without_retry(self) -> None:
        ledger_path = self.root / "billing-stop.jsonl"
        calls = 0

        def billing_fail_transport(url: str, headers: dict[str, str], body: str, timeout_s: float) -> HttpResponse:
            del url, headers, body, timeout_s
            nonlocal calls
            calls += 1
            return HttpResponse(
                429,
                json.dumps(
                    {
                        "error": {
                            "type": "insufficient_quota",
                            "code": "insufficient_quota",
                            "message": "You exceeded your current quota.",
                        }
                    }
                ),
                {},
            )

        with self.assertRaises(BillingHardStopError):
            run_eval(
                repo_root=REPO_ROOT,
                ledger_path=ledger_path,
                mode="fixture",
                judge_provider="anthropic",
                transport=billing_fail_transport,
            )
        # Stopped on the first call with zero retries and never called judge.
        self.assertEqual(calls, 1)

    def test_sandbox_verifier_isolates_home_and_rejects_unsafe_commands(self) -> None:
        # Fake executable script pretending to be `another` for safe sandbox tests.
        fake_bin = self.root / "fake-another"
        fake_bin.write_text(
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            "if [ \"${1:-}\" = \"--version\" ]; then echo \"another 0.1.0\"; exit 0; fi\n"
            "if [ \"${1:-}\" = \"init\" ]; then echo \"ANOTHER_HOME=$ANOTHER_HOME\"; exit 0; fi\n"
            "if [ \"${1:-}\" = \"hold\" ] && [ \"${2:-}\" = \"check\" ]; then echo \"conflict\" >&2; exit 3; fi\n"
            "exit 0\n",
            encoding="utf-8",
        )
        os.chmod(fake_bin, stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)

        commands = [
            {"step_id": "s1", "command": "cargo install another", "expected_exit_code": 0},
            {"step_id": "s2", "command": "another --version", "expected_exit_code": 0},
            {"step_id": "s3", "command": "another init", "expected_exit_code": 0},
            {"step_id": "s4", "command": "another hold check --agent bob src/a.rs", "expected_exit_code": 3},
            {"step_id": "s5", "command": "another status; rm -rf /tmp/x", "expected_exit_code": 0},
            {"step_id": "s6", "command": "another daemon install", "expected_exit_code": 0},
            {"step_id": "s7", "command": "another --home /home/worker/.another status", "expected_exit_code": 0},
            {"step_id": "s8", "command": "another daemon restart --label status", "expected_exit_code": 0},
            {"step_id": "s9", "command": "another daemon status", "expected_exit_code": 0},
            {"step_id": "s10", "command": "another daemon install --dry-run", "expected_exit_code": 0},
            {"step_id": "s11", "command": "another doctor", "expected_exit_code": 0},
            {"step_id": "s12", "command": "another hook install --client all", "expected_exit_code": 0},
            {"step_id": "s13", "command": "another hook uninstall --client all", "expected_exit_code": 0},
            {"step_id": "s14", "command": "another hook install --client all --dry-run", "expected_exit_code": 0},
            {"step_id": "s15", "command": "another hook status", "expected_exit_code": 0},
            {"step_id": "s16", "command": "node -e 'fetch(\"https://example.com\")'", "expected_exit_code": 0},
            {"step_id": "s17", "command": "another node show", "expected_exit_code": 0},
            {"step_id": "s18", "command": "another node unknown", "expected_exit_code": 0},
        ]
        results = verify_commands_in_sandbox(fake_bin, commands)
        by_id = {r["step_id"]: r for r in results}

        self.assertFalse(by_id["s1"]["executed"])
        self.assertEqual(by_id["s1"]["skip_reason"], "non_another_command")

        self.assertTrue(by_id["s2"]["executed"])
        self.assertTrue(by_id["s2"]["matched_expected_exit"])

        self.assertTrue(by_id["s3"]["executed"])
        self.assertIn("another-clarity-sandbox-", by_id["s3"]["stdout_snippet"])
        self.assertNotIn(str(Path.home() / ".another"), by_id["s3"]["stdout_snippet"])

        self.assertTrue(by_id["s4"]["executed"])
        self.assertEqual(by_id["s4"]["actual_exit_code"], 3)
        self.assertTrue(by_id["s4"]["matched_expected_exit"])

        self.assertFalse(by_id["s5"]["executed"])
        self.assertIn("shell_metacharacter_rejected", by_id["s5"]["skip_reason"])

        self.assertFalse(by_id["s6"]["executed"])
        self.assertEqual(by_id["s6"]["skip_reason"], "daemon_command_requires_install_or_uninstall_dry_run")

        self.assertFalse(by_id["s7"]["executed"])
        self.assertEqual(by_id["s7"]["skip_reason"], "explicit_home_flag_rejected")

        self.assertFalse(by_id["s8"]["executed"])
        self.assertEqual(by_id["s8"]["skip_reason"], "daemon_command_requires_install_or_uninstall_dry_run")

        self.assertFalse(by_id["s9"]["executed"])
        self.assertEqual(by_id["s9"]["skip_reason"], "daemon_command_requires_install_or_uninstall_dry_run")

        self.assertTrue(by_id["s10"]["executed"])
        self.assertTrue(by_id["s10"]["matched_expected_exit"])

        self.assertFalse(by_id["s11"]["executed"])
        self.assertEqual(by_id["s11"]["skip_reason"], "subcommand_not_in_sandbox_allowlist:doctor")

        self.assertFalse(by_id["s12"]["executed"])
        self.assertEqual(by_id["s12"]["skip_reason"], "hook_mutating_command_requires_dry_run")

        self.assertFalse(by_id["s13"]["executed"])
        self.assertEqual(by_id["s13"]["skip_reason"], "hook_mutating_command_requires_dry_run")

        self.assertTrue(by_id["s14"]["executed"])
        self.assertTrue(by_id["s14"]["matched_expected_exit"])

        self.assertTrue(by_id["s15"]["executed"])
        self.assertTrue(by_id["s15"]["matched_expected_exit"])

        self.assertFalse(by_id["s16"]["executed"])
        self.assertEqual(by_id["s16"]["skip_reason"], "non_another_command")

        self.assertTrue(by_id["s17"]["executed"])
        self.assertTrue(by_id["s17"]["matched_expected_exit"])

        self.assertFalse(by_id["s18"]["executed"])
        self.assertEqual(by_id["s18"]["skip_reason"], "unsupported_node_action")

    def test_cli_fixture_mode_ignores_live_ledger_env_and_refuses_ledger_flag(self) -> None:
        live_ledger = self.root / "shared-live.jsonl"
        live_obj = SpendLedger(live_ledger, simulated=False)
        res = live_obj.reserve(
            provider="openai",
            model="gpt-5.4-nano",
            body_str='{"real":true}',
            max_output_tokens=100,
        )
        live_obj.record_outcome(res, status="completed", spent_micros=42)
        before_bytes = live_ledger.read_bytes()

        env = dict(os.environ)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        env["ANOTHER_EVAL_LEDGER"] = str(live_ledger)

        # Running CLI in fixture mode must ignore ANOTHER_EVAL_LEDGER and leave it untouched.
        proc = subprocess.run(
            [sys.executable, str(REPO_ROOT / "evals/quickstart_clarity/runner.py"), "--mode", "fixture"],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(live_ledger.read_bytes(), before_bytes)
        self.assertIn("spent_usd=$0.000000", proc.stdout)

        # Explicitly passing --ledger in fixture mode must be refused.
        proc_flag = subprocess.run(
            [
                sys.executable,
                str(REPO_ROOT / "evals/quickstart_clarity/runner.py"),
                "--mode",
                "fixture",
                "--ledger",
                str(live_ledger),
            ],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc_flag.returncode, 2)
        self.assertEqual(live_ledger.read_bytes(), before_bytes)

        # Direct run_eval in fixture mode against an existing live ledger also fails closed.
        with self.assertRaises(LedgerError) as ctx:
            run_eval(
                repo_root=REPO_ROOT,
                ledger_path=live_ledger,
                mode="fixture",
                judge_provider="none",
            )
        self.assertEqual(ctx.exception.error_class, "fixture_live_ledger_collision")
        self.assertEqual(live_ledger.read_bytes(), before_bytes)

    def test_cli_live_mode_missing_ledger_and_malformed_key_stderr_is_sanitized(self) -> None:
        missing_ledger = self.root / "nonexistent-live.jsonl"
        secret = "SUPERSECRET_CLI_LEAK_GUARD_7788"

        # Missing ledger in live mode fails closed without creating an empty file.
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "ANOTHER_EVAL_ALLOW_LIVE": "1",
            "ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED": "1",
            "OPENAI_API_KEY": f"sk-valid-but-ledger-missing",
        }
        proc_missing = subprocess.run(
            [
                sys.executable,
                str(REPO_ROOT / "evals/quickstart_clarity/runner.py"),
                "--mode",
                "live",
                "--ledger",
                str(missing_ledger),
            ],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc_missing.returncode, 1)
        self.assertIn("ledger_missing", proc_missing.stderr)
        self.assertFalse(missing_ledger.exists())

        # Provisioned ledger + multiline malformed key -> sanitized stderr, zero key leakage, no traceback.
        init_ledger(missing_ledger)
        before_bytes = missing_ledger.read_bytes()
        env["OPENAI_API_KEY"] = f"sk-{secret}\nX-Evil: 1"
        proc_bad_key = subprocess.run(
            [
                sys.executable,
                str(REPO_ROOT / "evals/quickstart_clarity/runner.py"),
                "--mode",
                "live",
                "--ledger",
                str(missing_ledger),
            ],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc_bad_key.returncode, 1)
        self.assertIn("key_invalid_format", proc_bad_key.stderr)
        self.assertNotIn(secret, proc_bad_key.stderr)
        self.assertNotIn(secret, proc_bad_key.stdout)
        self.assertNotIn("Traceback", proc_bad_key.stderr)
        self.assertEqual(missing_ledger.read_bytes(), before_bytes)

    def test_cli_max_ledger_usd_flag_and_env_enforce_cumulative_cap(self) -> None:
        ledger_path = self.root / "max-ledger-cli.jsonl"
        init_ledger(ledger_path)
        before_bytes = ledger_path.read_bytes()

        base_env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "ANOTHER_EVAL_ALLOW_LIVE": "1",
            "ANOTHER_EVAL_NO_AUTO_RECHARGE_CONFIRMED": "1",
            "OPENAI_API_KEY": "sk-valid-key-never-reaches-network",
        }

        # Flag --max-ledger-usd 0.0001 must fail closed pre-call with ledger_cap_exceeded.
        proc_flag = subprocess.run(
            [
                sys.executable,
                str(REPO_ROOT / "evals/quickstart_clarity/runner.py"),
                "--mode",
                "live",
                "--ledger",
                str(ledger_path),
                "--max-ledger-usd",
                "0.0001",
            ],
            cwd=str(REPO_ROOT),
            env=base_env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc_flag.returncode, 1)
        self.assertIn("ledger_cap_exceeded", proc_flag.stderr)
        self.assertEqual(ledger_path.read_bytes(), before_bytes)

        # Env ANOTHER_EVAL_MAX_LEDGER_USD=0.0001 must also enforce the cumulative ledger cap.
        env_cap = dict(base_env)
        env_cap["ANOTHER_EVAL_MAX_LEDGER_USD"] = "0.0001"
        proc_env = subprocess.run(
            [
                sys.executable,
                str(REPO_ROOT / "evals/quickstart_clarity/runner.py"),
                "--mode",
                "live",
                "--ledger",
                str(ledger_path),
            ],
            cwd=str(REPO_ROOT),
            env=env_cap,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(proc_env.returncode, 1)
        self.assertIn("ledger_cap_exceeded", proc_env.stderr)
        self.assertEqual(ledger_path.read_bytes(), before_bytes)

    def test_workflow_and_root_readme_enforce_durable_ledger_and_document_eval(self) -> None:
        workflow = (REPO_ROOT / ".github/workflows/eval-quickstart-clarity.yml").read_text(encoding="utf-8")
        # Never cache or upload stale *.lock files or ephemeral Actions caches as spend ledgers.
        self.assertNotIn("actions/cache", workflow)
        self.assertNotIn(".lock", workflow)
        self.assertNotIn("another-eval-ledger", workflow)
        self.assertIn("ANOTHER_EVAL_DURABLE_LEDGER", workflow)
        self.assertIn("EVAL_MAX_LEDGER_USD", workflow)
        self.assertIn("workflow_dispatch:", workflow)

        readme = (REPO_ROOT / "README.md").read_text(encoding="utf-8")
        self.assertIn("evals/quickstart_clarity/README.md", readme)
        self.assertIn("fixture", readme.lower())


if __name__ == "__main__":
    unittest.main()
