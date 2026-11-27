"""Isolated sandbox verifier for stranger-proposed `another` CLI commands.

Always runs in a throwaway temporary HOME, ANOTHER_HOME, and git repo (`shop`).
Never touches live `~/.another`, never uses shell=True, and only executes
allowlisted non-daemon `another` subcommands.
"""
from __future__ import annotations

import os
import shlex
import subprocess
import tempfile
from pathlib import Path
from typing import Any

SHELL_METACHARACTERS = (";", "&&", "||", "|", ">", "<", "`", "$(", "\n", "\r")

ALLOWED_SUBCOMMANDS = frozenset(
    {
        "--version",
        "--help",
        "-h",
        "init",
        "status",
        "agent",
        "send",
        "inbox",
        "reply",
        "ack",
        "threads",
        "thread",
        "requests",
        "request",
        "activity",
        "hold",
        "holds",
        "board",
        "stats",
        "queue",
        "needs",
        "node",
        "events",
        "heartbeat",
        "hook",
        "daemon",
    }
)


def _check_safe_command(command: str) -> tuple[list[str] | None, str | None]:
    """Validate and split a proposed command.

    Returns (argv, skip_reason). If skip_reason is not None, the command is not executed.
    """
    stripped = command.strip()
    if not stripped:
        return None, "empty_command"
    for bad in SHELL_METACHARACTERS:
        if bad in stripped:
            return None, f"shell_metacharacter_rejected:{bad!r}"
    try:
        argv = shlex.split(stripped)
    except ValueError:
        return None, "unparseable_command"
    if not argv or argv[0] != "another":
        return None, "non_another_command"
    if len(argv) < 2:
        return None, "missing_subcommand"

    # Never allow a command to override --home toward a host path.
    for tok in argv:
        if tok == "--home" or tok.startswith("--home="):
            return None, "explicit_home_flag_rejected"

    # Skip global flags like --json to inspect the subcommand.
    sub_idx = 1
    while sub_idx < len(argv) and argv[sub_idx] in ("--json",):
        sub_idx += 1
    if sub_idx >= len(argv):
        return None, "missing_subcommand"

    sub = argv[sub_idx]
    if sub not in ALLOWED_SUBCOMMANDS:
        return None, f"subcommand_not_in_sandbox_allowlist:{sub}"
    if sub == "daemon":
        # Strictly require positional `daemon install|uninstall` WITH `--dry-run`.
        # Never allow `daemon restart` or `daemon status`, which query/touch the host service manager.
        action = argv[sub_idx + 1] if sub_idx + 1 < len(argv) else ""
        if action not in ("install", "uninstall") or "--dry-run" not in argv[sub_idx + 2 :]:
            return None, "daemon_command_requires_install_or_uninstall_dry_run"
    if sub == "hook":
        # Strictly require positional `hook status|install|uninstall` (safe under temp HOME).
        action = argv[sub_idx + 1] if sub_idx + 1 < len(argv) else ""
        if action not in ("status", "install", "uninstall"):
            return None, "unsupported_hook_action"
    return argv, None


def verify_commands_in_sandbox(
    binary_path: str | Path,
    proposed_commands: list[dict[str, Any]],
    *,
    timeout_s: float = 15.0,
) -> list[dict[str, Any]]:
    """Run safe `another` commands in a brand-new temp HOME and ANOTHER_HOME."""
    bin_path = Path(binary_path).resolve()
    if not bin_path.is_file() or not os.access(bin_path, os.X_OK):
        raise FileNotFoundError(f"Executable another binary not found: {bin_path}")

    real_live_home = (Path.home() / ".another").resolve()
    results: list[dict[str, Any]] = []

    with tempfile.TemporaryDirectory(prefix="another-clarity-sandbox-") as tmp:
        tmp_path = Path(tmp)
        home_dir = tmp_path / "home"
        data_dir = tmp_path / "data"
        shop_dir = tmp_path / "shop"
        home_dir.mkdir(mode=0o700)
        data_dir.mkdir(mode=0o700)
        shop_dir.mkdir(mode=0o700)

        if data_dir.resolve() == real_live_home:
            raise RuntimeError("Refusing to run sandbox against live ~/.another")

        subprocess.run(
            ["git", "init", "-q"],
            cwd=str(shop_dir),
            check=True,
            capture_output=True,
            timeout=timeout_s,
        )

        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home_dir),
            "ANOTHER_HOME": str(data_dir),
            "LANG": "C.UTF-8",
        }

        for entry in proposed_commands:
            step_id = str(entry.get("step_id", ""))
            command = str(entry.get("command", ""))
            expected_exit = entry.get("expected_exit_code")
            argv, skip_reason = _check_safe_command(command)
            if skip_reason is not None or argv is None:
                results.append(
                    {
                        "step_id": step_id,
                        "command": command,
                        "executed": False,
                        "skip_reason": skip_reason,
                        "expected_exit_code": expected_exit,
                        "actual_exit_code": None,
                        "matched_expected_exit": None,
                    }
                )
                continue

            exec_argv = [str(bin_path), *argv[1:]]
            try:
                proc = subprocess.run(
                    exec_argv,
                    cwd=str(shop_dir),
                    env=env,
                    shell=False,
                    capture_output=True,
                    text=True,
                    timeout=timeout_s,
                )
                matched = (
                    (proc.returncode == expected_exit)
                    if isinstance(expected_exit, int)
                    else None
                )
                results.append(
                    {
                        "step_id": step_id,
                        "command": command,
                        "executed": True,
                        "skip_reason": None,
                        "expected_exit_code": expected_exit,
                        "actual_exit_code": proc.returncode,
                        "matched_expected_exit": matched,
                        "stdout_snippet": proc.stdout[:600],
                        "stderr_snippet": proc.stderr[:600],
                    }
                )
            except subprocess.TimeoutExpired:
                results.append(
                    {
                        "step_id": step_id,
                        "command": command,
                        "executed": True,
                        "skip_reason": "timeout",
                        "expected_exit_code": expected_exit,
                        "actual_exit_code": None,
                        "matched_expected_exit": False,
                    }
                )

    return results
