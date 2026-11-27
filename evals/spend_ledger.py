"""Shared reserve-then-spend ledger and metered LLM transports for Another evals.

Implements the spend controls required by SPEND-RULES.md:
1. Durable reserve-then-spend JSONL ledger at an explicit absolute path, fail closed.
2. Conservative pre-call reservations (never refunded on crash or timeout) and per-call caps.
3. Hard stop on any insufficient-credit or billing error, persisted to the ledger.
4. Zero retries into billing or ambiguous 429 errors; transient non-billing retries reuse
   the existing reservation without reserving again.
5. Per-product key loading from mode-600 files or explicit env vars, never printed or logged.
   Anthropic judge keys are read ONLY from ANTHROPIC_JUDGE_KEY_FILE or
   ANTHROPIC_JUDGE_API_KEY (never ANTHROPIC_API_KEY).
6. Fleshapron weekly receipt fields on every ledger row and summary helper.
"""
from __future__ import annotations

import json
import math
import os
import socket
import stat
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

MICROS_PER_USD = 1_000_000
MAX_KEY_FILE_BYTES = 4096
MAX_REQUEST_BODY_BYTES = 250_000
MAX_OUTPUT_TOKENS_CEILING = 8192
INPUT_FRAMING_MARGIN_TOKENS = 4096

DEFAULT_OPENAI_MODEL = "gpt-5.4-nano"
DEFAULT_ANTHROPIC_JUDGE_MODEL = "claude-haiku-4-5-20251001"
DEFAULT_PER_CALL_CAP_USD = 0.10
DEFAULT_RUN_CAP_USD = 0.25

# Explicit retired/disallowed models that must be rejected before reservation.
RETIRED_MODELS: dict[str, str] = {
    "gpt-5-mini": "shuts down 11 Dec 2026; use gpt-5.4-nano or gpt-5.4-mini",
}


@dataclass(frozen=True)
class ModelSpec:
    provider: str
    model: str
    input_per_million: float
    output_per_million: float
    judge_only: bool = False


# Conservative pricing ceilings (USD per 1M tokens). Unlisted models fail closed.
MODEL_CATALOG: dict[str, ModelSpec] = {
    "gpt-5.4-nano": ModelSpec("openai", "gpt-5.4-nano", 0.20, 1.25),
    "gpt-5.4-mini": ModelSpec("openai", "gpt-5.4-mini", 0.75, 4.50),
    "claude-haiku-4-5-20251001": ModelSpec(
        "anthropic", "claude-haiku-4-5-20251001", 1.00, 5.00, judge_only=True
    ),
    "claude-sonnet-4-5-20250929": ModelSpec(
        "anthropic", "claude-sonnet-4-5-20250929", 3.00, 15.00, judge_only=True
    ),
    "claude-sonnet-4-6": ModelSpec(
        "anthropic", "claude-sonnet-4-6", 3.00, 15.00, judge_only=True
    ),
    "gemini-3.1-flash-lite": ModelSpec("google", "gemini-3.1-flash-lite", 0.25, 1.50),
}


class SpendError(RuntimeError):
    """Base error for spend-control failures."""

    def __init__(self, message: str, *, error_class: str = "spend_error") -> None:
        super().__init__(message)
        self.error_class = error_class


class LedgerError(SpendError):
    """Raised when the ledger path, lock, or contents are invalid (fail closed)."""


class BudgetExceededError(SpendError):
    """Raised when a per-call or cumulative budget cap would be exceeded."""


class KeyConfigError(SpendError):
    """Raised when a provider key is missing, insecurely permissioned, or misconfigured."""


class BillingHardStopError(SpendError):
    """Raised on any provider billing or insufficient-credit error. Never retry."""


class ProviderRequestError(SpendError):
    """Raised on non-billing provider or transport failures."""

    def __init__(
        self,
        message: str,
        *,
        error_class: str,
        retryable: bool = False,
        status_code: int | None = None,
    ) -> None:
        super().__init__(message, error_class=error_class)
        self.retryable = retryable
        self.status_code = status_code


@dataclass(frozen=True)
class HttpResponse:
    status: int
    body: str
    headers: dict[str, str]


TransportFn = Callable[[str, dict[str, str], str, float], HttpResponse]


@dataclass(frozen=True)
class Reservation:
    reservation_id: str
    time: str
    product: str
    provider: str
    model: str
    reserved_micros: int
    reserved_usd: float


@dataclass(frozen=True)
class CallResult:
    reservation_id: str
    provider: str
    model: str
    text: str
    input_tokens: int
    output_tokens: int
    reserved_usd: float
    spent_usd: float
    attempts: int


BILLING_ERROR_CODES = frozenset(
    {
        "insufficient_quota",
        "billing_hard_limit_reached",
        "billing_not_active",
        "account_deactivated",
        "credit_balance_too_low",
        "insufficient_credits",
        "payment_required",
        "billing_error",
        "billing_disabled",
    }
)

BILLING_MESSAGE_MARKERS = (
    "insufficient_quota",
    "exceeded your current quota",
    "credit balance is too low",
    "insufficient credits",
    "billing hard limit",
    "billing_hard_limit",
    "billing is not active",
    "plan and billing",
    "add credits",
    "purchase credits",
    "payment required",
    "payment method",
    "account balance",
    "out of credits",
    "spending limit",
    "usage limit reached",
    "billing_disabled",
    "credit balance",
)

NON_BILLING_RATE_LIMIT_CODES = frozenset(
    {
        "rate_limit_exceeded",
        "rate_limit_error",
        "overloaded_error",
    }
)


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def get_model_spec(model: str, *, require_provider: str | None = None, role: str = "player") -> ModelSpec:
    if model in RETIRED_MODELS:
        raise SpendError(
            f"Model {model!r} is disallowed: {RETIRED_MODELS[model]}",
            error_class="retired_model",
        )
    spec = MODEL_CATALOG.get(model)
    if spec is None:
        raise SpendError(
            f"Unreviewed or unpriced model {model!r}; add an explicit pricing ceiling to MODEL_CATALOG",
            error_class="unreviewed_model",
        )
    if require_provider is not None and spec.provider != require_provider:
        raise SpendError(
            f"Model {model!r} belongs to provider {spec.provider!r}, expected {require_provider!r}",
            error_class="provider_mismatch",
        )
    if spec.judge_only and role != "judge":
        raise SpendError(
            f"Model {model!r} ({spec.provider}) is restricted to judge role only (SPEND-RULES.md)",
            error_class="judge_only_model",
        )
    return spec


def _read_mode_600_key_file(path: Path, label: str) -> str | None:
    """Read a key from a regular mode-0600 file without following symlinks.

    Returns None only if the file does not exist (ENOENT).
    Raises KeyConfigError on permission violations, symlinks, empty files, or read errors.
    """
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_NONBLOCK"):
        flags |= os.O_NONBLOCK
    try:
        fd = os.open(str(path), flags)
    except FileNotFoundError:
        return None
    except OSError as exc:
        raise KeyConfigError(
            f"{label} key file unavailable (check regular file / no symlink)",
            error_class="key_file_unavailable",
        ) from exc
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            raise KeyConfigError(
                f"{label} key path must be a regular file",
                error_class="key_file_not_regular",
            )
        mode = stat.S_IMODE(st.st_mode)
        if mode != 0o600:
            raise KeyConfigError(
                f"{label} key file must have mode 0600 (found {oct(mode)})",
                error_class="key_file_bad_mode",
            )
        if st.st_size > MAX_KEY_FILE_BYTES:
            raise KeyConfigError(
                f"{label} key file exceeds {MAX_KEY_FILE_BYTES} bytes",
                error_class="key_file_too_large",
            )
        raw = os.read(fd, MAX_KEY_FILE_BYTES + 1)
    finally:
        os.close(fd)
    key = raw.decode("utf-8").strip()
    if not key:
        raise KeyConfigError(f"{label} key file is empty", error_class="key_file_empty")
    return key


def load_provider_key(
    provider: str,
    *,
    env: dict[str, str] | None = None,
    home_dir: Path | None = None,
    role: str = "player",
    anthropic_zero_cash_confirmed: bool = False,
) -> str:
    """Load an API key for the given provider following SPEND-RULES.md.

    Keys are never logged or included in exception messages.
    """
    source_env = os.environ if env is None else env
    home = Path.home() if home_dir is None else home_dir

    if provider == "openai":
        env_key = source_env.get("OPENAI_API_KEY", "").strip()
        if env_key:
            return env_key
        file_key = _read_mode_600_key_file(home / ".config" / "openai" / "key", "OpenAI")
        if file_key:
            return file_key
        raise KeyConfigError(
            "OpenAI key unavailable: set OPENAI_API_KEY or create mode-600 ~/.config/openai/key",
            error_class="key_missing",
        )

    if provider == "google":
        for var in ("GEMINI_API_KEY", "GOOGLE_API_KEY"):
            env_key = source_env.get(var, "").strip()
            if env_key:
                return env_key
        file_key = _read_mode_600_key_file(home / ".config" / "gemini" / "key", "Google")
        if file_key:
            return file_key
        raise KeyConfigError(
            "Google key unavailable: set GEMINI_API_KEY, GOOGLE_API_KEY, or mode-600 ~/.config/gemini/key",
            error_class="key_missing",
        )

    if provider == "anthropic":
        if role != "judge":
            raise KeyConfigError(
                "Anthropic promo key is allowed for judge role only (SPEND-RULES.md)",
                error_class="anthropic_judge_only",
            )
        confirmed = anthropic_zero_cash_confirmed or (
            source_env.get("ANOTHER_EVAL_ANTHROPIC_ZERO_CASH_CONFIRMED", "").strip() == "1"
        )
        if not confirmed:
            raise KeyConfigError(
                "Refusing Anthropic judge call without explicit confirmation that cash balance is $0 "
                "and auto-recharge is off (--confirm-anthropic-zero-cash or "
                "ANOTHER_EVAL_ANTHROPIC_ZERO_CASH_CONFIRMED=1)",
                error_class="anthropic_zero_cash_unconfirmed",
            )
        # Never read ANTHROPIC_API_KEY (per SPEND-RULES.md).
        key_file_raw = source_env.get("ANTHROPIC_JUDGE_KEY_FILE", "").strip()
        if key_file_raw:
            key_path = Path(key_file_raw)
            if not key_path.is_absolute():
                raise KeyConfigError(
                    "ANTHROPIC_JUDGE_KEY_FILE must be an absolute path",
                    error_class="key_path_not_absolute",
                )
            file_key = _read_mode_600_key_file(key_path, "Anthropic judge")
            if file_key:
                return file_key
            raise KeyConfigError(
                "ANTHROPIC_JUDGE_KEY_FILE does not exist",
                error_class="key_file_missing",
            )
        judge_env_key = source_env.get("ANTHROPIC_JUDGE_API_KEY", "").strip()
        if judge_env_key:
            return judge_env_key
        raise KeyConfigError(
            "Anthropic judge key unavailable: set ANTHROPIC_JUDGE_KEY_FILE (mode 600) or "
            "ANTHROPIC_JUDGE_API_KEY (ANTHROPIC_API_KEY is intentionally ignored)",
            error_class="key_missing",
        )

    raise KeyConfigError(f"Unsupported provider {provider!r}", error_class="unsupported_provider")


def estimate_reservation_micros(body_str: str, max_output_tokens: int, spec: ModelSpec) -> int:
    body_bytes = len(body_str.encode("utf-8"))
    if body_bytes <= 0 or body_bytes > MAX_REQUEST_BODY_BYTES:
        raise BudgetExceededError(
            f"Request body size ({body_bytes} bytes) out of bounds (1..{MAX_REQUEST_BODY_BYTES})",
            error_class="request_too_large",
        )
    if not isinstance(max_output_tokens, int) or max_output_tokens < 1 or max_output_tokens > MAX_OUTPUT_TOKENS_CEILING:
        raise BudgetExceededError(
            f"max_output_tokens ({max_output_tokens}) out of bounds (1..{MAX_OUTPUT_TOKENS_CEILING})",
            error_class="invalid_max_output_tokens",
        )
    # Conservative ceiling: 1 token per UTF-8 byte + framing margin; full max_output_tokens.
    micros = math.ceil(
        (body_bytes + INPUT_FRAMING_MARGIN_TOKENS) * spec.input_per_million
        + max_output_tokens * spec.output_per_million
    )
    if micros <= 0:
        raise BudgetExceededError("Computed reservation must be positive", error_class="invalid_reservation")
    return micros


def compute_spent_micros(input_tokens: int, output_tokens: int, spec: ModelSpec) -> int:
    if not isinstance(input_tokens, int) or input_tokens < 0:
        raise ProviderRequestError(
            "Provider returned invalid input_tokens",
            error_class="invalid_usage",
            retryable=False,
        )
    if not isinstance(output_tokens, int) or output_tokens < 0:
        raise ProviderRequestError(
            "Provider returned invalid output_tokens",
            error_class="invalid_usage",
            retryable=False,
        )
    return math.ceil(
        input_tokens * spec.input_per_million + output_tokens * spec.output_per_million
    )


def classify_http_error(provider: str, status: int, body: str) -> tuple[bool, bool, str]:
    """Classify a non-2xx HTTP error.

    Returns (is_billing_hard_stop, is_retryable_transient, error_class).
    Never returns raw response body text.
    """
    code_str = ""
    type_str = ""
    msg_lower = ""
    parsed_json = False

    try:
        parsed = json.loads(body) if body.strip() else None
        if isinstance(parsed, dict):
            err = parsed.get("error")
            if isinstance(err, dict):
                parsed_json = True
                code_str = str(err.get("code") or "").strip().lower()
                type_str = str(err.get("type") or "").strip().lower()
                msg_lower = str(err.get("message") or "").strip().lower()
            elif isinstance(err, str):
                parsed_json = True
                msg_lower = err.strip().lower()
            elif "message" in parsed:
                parsed_json = True
                msg_lower = str(parsed.get("message") or "").strip().lower()
    except json.JSONDecodeError:
        parsed_json = False

    raw_lower = body.lower() if body else ""

    if status == 402:
        return True, False, "billing_payment_required"

    if code_str in BILLING_ERROR_CODES:
        return True, False, f"billing_{code_str}"
    if type_str in BILLING_ERROR_CODES:
        return True, False, f"billing_{type_str}"

    for marker in BILLING_MESSAGE_MARKERS:
        if marker in msg_lower or marker in raw_lower:
            return True, False, "billing_credit_or_quota_exhausted"

    if status == 429:
        # Only retry a 429 when it explicitly identifies a non-billing rate limit.
        has_billing_word = any(
            word in msg_lower or word in raw_lower
            for word in ("quota", "billing", "credit", "balance", "payment", "plan", "spend")
        )
        if parsed_json and not has_billing_word and (
            code_str in NON_BILLING_RATE_LIMIT_CODES or type_str in NON_BILLING_RATE_LIMIT_CODES
        ):
            return False, True, f"transient_rate_limit_{provider}"
        # Ambiguous 429 fails closed with zero retries to prevent retrying into billing.
        return False, False, f"http_429_ambiguous_{provider}"

    if status in (500, 502, 503, 529):
        return False, True, f"transient_http_{status}"

    if status == 504:
        # Gateway timeout may have already executed upstream; fail closed without retry.
        return False, False, "http_504_timeout_no_retry"

    if status in (401, 403):
        return False, False, f"auth_http_{status}"

    return False, False, f"http_{status}"


class SpendLedger:
    """Durable, fail-closed, lock-guarded reserve-then-spend JSONL ledger.

    Modeled on compoundinterest.ing `app/app/lib/analyst-budget.server.ts`.
    A crashed lock fails closed; an operator reconciles it, never a stale timer.
    Reservations are permanent and conservative: crashes or timeouts never refund spend.
    """

    def __init__(
        self,
        ledger_path: str | Path,
        *,
        product: str = "another",
        eval_name: str = "quickstart-clarity",
        max_call_usd: float = DEFAULT_PER_CALL_CAP_USD,
        max_run_usd: float = DEFAULT_RUN_CAP_USD,
        max_ledger_usd: float | None = None,
    ) -> None:
        raw = str(ledger_path).strip()
        if not raw or not os.path.isabs(raw):
            raise LedgerError(
                "Absolute ledger path is required (fail closed)",
                error_class="relative_ledger_path",
            )
        if not math.isfinite(max_call_usd) or max_call_usd <= 0:
            raise BudgetExceededError("max_call_usd must be finite and > 0", error_class="invalid_cap")
        if not math.isfinite(max_run_usd) or max_run_usd <= 0:
            raise BudgetExceededError("max_run_usd must be finite and > 0", error_class="invalid_cap")
        if max_ledger_usd is not None and (not math.isfinite(max_ledger_usd) or max_ledger_usd <= 0):
            raise BudgetExceededError("max_ledger_usd must be finite and > 0", error_class="invalid_cap")

        self.path = Path(raw)
        self.lock_path = Path(f"{raw}.lock")
        self.product = product
        self.eval_name = eval_name
        self.max_call_micros = math.floor(max_call_usd * MICROS_PER_USD)
        self.max_run_micros = math.floor(max_run_usd * MICROS_PER_USD)
        self.max_ledger_micros = (
            math.floor(max_ledger_usd * MICROS_PER_USD) if max_ledger_usd is not None else None
        )
        self._run_reserved_micros = 0
        self._hard_stopped_providers: set[str] = set()

    def _acquire_lock(self) -> None:
        if not self.path.parent.exists():
            raise LedgerError(
                f"Ledger parent directory does not exist: {self.path.parent}",
                error_class="ledger_parent_missing",
            )
        try:
            os.mkdir(self.lock_path, 0o700)
        except FileExistsError as exc:
            raise LedgerError(
                f"Ledger lock already exists (fail closed; operator must reconcile): {self.lock_path}",
                error_class="ledger_locked",
            ) from exc
        except OSError as exc:
            raise LedgerError(
                f"Unable to acquire ledger lock: {exc.strerror}",
                error_class="ledger_lock_failed",
            ) from exc

    def _release_lock(self) -> None:
        os.rmdir(self.lock_path)

    def _read_rows_unlocked(self) -> list[dict[str, Any]]:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            fd = os.open(str(self.path), flags)
        except FileNotFoundError:
            return []
        except OSError as exc:
            raise LedgerError(
                "Ledger file unavailable or symlink rejected",
                error_class="ledger_unreadable",
            ) from exc
        try:
            st = os.fstat(fd)
            if not stat.S_ISREG(st.st_mode):
                raise LedgerError("Ledger path is not a regular file", error_class="ledger_not_regular")
            chunks: list[bytes] = []
            while True:
                chunk = os.read(fd, 65536)
                if not chunk:
                    break
                chunks.append(chunk)
        finally:
            os.close(fd)

        content = b"".join(chunks).decode("utf-8").strip()
        if not content:
            return []
        rows: list[dict[str, Any]] = []
        for idx, line in enumerate(content.splitlines(), 1):
            try:
                value = json.loads(line)
            except json.JSONDecodeError as exc:
                raise LedgerError(
                    f"Malformed JSON in spend ledger at line {idx}",
                    error_class="ledger_corrupt",
                ) from exc
            if not isinstance(value, dict):
                raise LedgerError(
                    f"Invalid ledger row at line {idx}: expected JSON object",
                    error_class="ledger_corrupt",
                )
            for req in ("time", "product", "provider", "model", "status"):
                if not isinstance(value.get(req), str) or not value[req]:
                    raise LedgerError(
                        f"Invalid ledger row at line {idx}: missing {req}",
                        error_class="ledger_corrupt",
                    )
            reserved_micros = value.get("reserved_micros")
            if not isinstance(reserved_micros, int) or isinstance(reserved_micros, bool) or reserved_micros < 0:
                raise LedgerError(
                    f"Invalid ledger row at line {idx}: invalid reserved_micros",
                    error_class="ledger_corrupt",
                )
            rows.append(value)
        return rows

    def _append_row_unlocked(self, row: dict[str, Any]) -> None:
        flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            fd = os.open(str(self.path), flags, 0o600)
        except OSError as exc:
            raise LedgerError(
                "Unable to open spend ledger for durable append",
                error_class="ledger_append_failed",
            ) from exc
        try:
            st = os.fstat(fd)
            if not stat.S_ISREG(st.st_mode):
                raise LedgerError("Ledger path is not a regular file", error_class="ledger_not_regular")
            payload = (json.dumps(row, sort_keys=True) + "\n").encode("utf-8")
            written = 0
            while written < len(payload):
                written += os.write(fd, payload[written:])
            os.fsync(fd)
        finally:
            os.close(fd)

    def reserve(
        self,
        *,
        provider: str,
        model: str,
        body_str: str,
        max_output_tokens: int,
        role: str = "player",
    ) -> Reservation:
        """Conservatively reserve spend before any network request is made."""
        spec = get_model_spec(model, require_provider=provider, role=role)
        if provider in self._hard_stopped_providers:
            raise BillingHardStopError(
                f"Provider {provider!r} is in billing hard-stop state for this session",
                error_class="billing_hard_stop_active",
            )

        micros = estimate_reservation_micros(body_str, max_output_tokens, spec)
        if micros > self.max_call_micros:
            raise BudgetExceededError(
                f"Estimated reservation ${micros / MICROS_PER_USD:.6f} exceeds per-call cap "
                f"${self.max_call_micros / MICROS_PER_USD:.6f}",
                error_class="per_call_cap_exceeded",
            )
        if self._run_reserved_micros + micros > self.max_run_micros:
            raise BudgetExceededError(
                f"Run reservation would reach ${(self._run_reserved_micros + micros) / MICROS_PER_USD:.6f}, "
                f"exceeding run cap ${self.max_run_micros / MICROS_PER_USD:.6f}",
                error_class="run_cap_exceeded",
            )

        self._acquire_lock()
        try:
            rows = self._read_rows_unlocked()
            total_ledger_micros = 0
            for row in rows:
                if row.get("provider") == provider and row.get("status") == "billing_hard_stop":
                    self._hard_stopped_providers.add(provider)
                    raise BillingHardStopError(
                        f"Ledger records an active billing hard stop for provider {provider!r}; "
                        "operator reconciliation required before resuming",
                        error_class="billing_hard_stop_recorded",
                    )
                if row.get("status") == "reserved":
                    total_ledger_micros += int(row["reserved_micros"])

            if self.max_ledger_micros is not None and total_ledger_micros + micros > self.max_ledger_micros:
                raise BudgetExceededError(
                    f"Cumulative ledger spend would reach "
                    f"${(total_ledger_micros + micros) / MICROS_PER_USD:.6f}, "
                    f"exceeding ledger cap ${self.max_ledger_micros / MICROS_PER_USD:.6f}",
                    error_class="ledger_cap_exceeded",
                )

            reservation_id = uuid.uuid4().hex
            now = utc_now_iso()
            reserved_usd = round(micros / MICROS_PER_USD, 6)
            record = {
                "time": now,
                "product": self.product,
                "eval": self.eval_name,
                "provider": provider,
                "model": model,
                "reservation_id": reservation_id,
                "reserved_micros": micros,
                "reserved_usd": reserved_usd,
                "spent_micros": None,
                "spent_usd": None,
                "status": "reserved",
                "error_class": None,
            }
            # Charge in-memory run total before append so a failed fsync cannot refund.
            self._run_reserved_micros += micros
            self._append_row_unlocked(record)
            return Reservation(
                reservation_id=reservation_id,
                time=now,
                product=self.product,
                provider=provider,
                model=model,
                reserved_micros=micros,
                reserved_usd=reserved_usd,
            )
        finally:
            self._release_lock()

    def record_outcome(
        self,
        reservation: Reservation,
        *,
        status: str,
        spent_micros: int | None = None,
        error_class: str | None = None,
    ) -> None:
        """Append the final outcome row for a prior reservation (never refunds reserved_micros)."""
        if status == "billing_hard_stop":
            self._hard_stopped_providers.add(reservation.provider)
        spent_usd = round(spent_micros / MICROS_PER_USD, 6) if spent_micros is not None else None
        record = {
            "time": utc_now_iso(),
            "product": reservation.product,
            "eval": self.eval_name,
            "provider": reservation.provider,
            "model": reservation.model,
            "reservation_id": reservation.reservation_id,
            "reserved_micros": reservation.reserved_micros,
            "reserved_usd": reservation.reserved_usd,
            "spent_micros": spent_micros,
            "spent_usd": spent_usd,
            "status": status,
            "error_class": error_class,
        }
        self._acquire_lock()
        try:
            self._append_row_unlocked(record)
        finally:
            self._release_lock()


ALLOWED_API_PREFIXES: tuple[str, ...] = (
    "https://api.openai.com/",
    "https://api.anthropic.com/",
)


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse all HTTP redirects so Authorization / x-api-key headers never cross origins."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(
            req.full_url,
            code,
            f"HTTP redirect ({code}) refused for credential safety",
            headers,
            fp,
        )


_NO_REDIRECT_OPENER = urllib.request.build_opener(_NoRedirectHandler())


def default_http_transport(url: str, headers: dict[str, str], body: str, timeout_s: float) -> HttpResponse:
    if not url.startswith(ALLOWED_API_PREFIXES):
        raise ProviderRequestError(
            "Refusing request to unapproved API endpoint",
            error_class="unapproved_endpoint",
            retryable=False,
        )
    req = urllib.request.Request(
        url=url,
        data=body.encode("utf-8"),
        headers=headers,
        method="POST",
    )
    try:
        with _NO_REDIRECT_OPENER.open(req, timeout=timeout_s) as resp:
            resp_body = resp.read().decode("utf-8", errors="replace")
            resp_headers = {k.lower(): v for k, v in resp.headers.items()}
            return HttpResponse(status=int(resp.status), body=resp_body, headers=resp_headers)
    except urllib.error.HTTPError as exc:
        # Let socket timeouts / I/O failures while reading the error body propagate so the
        # caller fails closed without retrying a possibly-billed request.
        err_body = exc.read().decode("utf-8", errors="replace") if exc.fp is not None else ""
        resp_headers = {k.lower(): v for k, v in exc.headers.items()} if exc.headers else {}
        return HttpResponse(status=int(exc.code), body=err_body, headers=resp_headers)


def metered_chat_json(
    *,
    ledger: SpendLedger,
    provider: str,
    model: str,
    system_prompt: str,
    user_prompt: str,
    max_output_tokens: int = 3000,
    role: str = "player",
    api_key: str | None = None,
    env: dict[str, str] | None = None,
    home_dir: Path | None = None,
    anthropic_zero_cash_confirmed: bool = False,
    transport: TransportFn = default_http_transport,
    timeout_s: float = 60.0,
    max_transient_retries: int = 1,
    sleep_fn: Callable[[float], None] = time.sleep,
) -> CallResult:
    """Execute one metered JSON completion against OpenAI or Anthropic (judge only).

    Reserves spend before any HTTP call. Never retries on billing errors or ambiguous 429s.
    Transient non-billing errors retry at most `max_transient_retries` times under the
    same unchanged reservation.
    """
    spec = get_model_spec(model, require_provider=provider, role=role)
    key = api_key if api_key is not None else load_provider_key(
        provider,
        env=env,
        home_dir=home_dir,
        role=role,
        anthropic_zero_cash_confirmed=anthropic_zero_cash_confirmed,
    )
    if not key.strip():
        raise KeyConfigError(f"Empty API key for provider {provider!r}", error_class="key_empty")

    if provider == "openai":
        url = "https://api.openai.com/v1/chat/completions"
        headers = {
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        }
        payload: dict[str, Any] = {
            "model": model,
            "temperature": 0,
            "max_completion_tokens": max_output_tokens,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
        }
    elif provider == "anthropic":
        url = "https://api.anthropic.com/v1/messages"
        headers = {
            "x-api-key": key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        }
        payload = {
            "model": model,
            "max_tokens": max_output_tokens,
            "temperature": 0,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_prompt}],
        }
    else:
        raise SpendError(f"Unsupported chat provider {provider!r}", error_class="unsupported_provider")

    body_str = json.dumps(payload, ensure_ascii=False)
    reservation = ledger.reserve(
        provider=provider,
        model=model,
        body_str=body_str,
        max_output_tokens=max_output_tokens,
        role=role,
    )

    attempts = 0
    total_allowed = 1 + max(0, max_transient_retries)
    while attempts < total_allowed:
        attempts += 1
        try:
            resp = transport(url, headers, body_str, timeout_s)
        except (TimeoutError, socket.timeout, urllib.error.URLError, ConnectionError, OSError) as exc:
            # Never retry after a timeout or dropped connection on a POST: the provider may
            # already have accepted and billed the first request, so retrying under one
            # reservation could double-charge. Fail closed and keep the reservation permanent.
            error_class = "transient_timeout" if isinstance(exc, (TimeoutError, socket.timeout)) else "transient_network"
            ledger.record_outcome(reservation, status="transient_error", spent_micros=None, error_class=error_class)
            raise ProviderRequestError(
                f"{provider} request aborted on {error_class}; no retry attempted to avoid double-billing",
                error_class=error_class,
                retryable=False,
            ) from exc

        if 200 <= resp.status < 300:
            try:
                data = json.loads(resp.body)
            except json.JSONDecodeError as exc:
                ledger.record_outcome(
                    reservation, status="failed", spent_micros=None, error_class="invalid_json_response"
                )
                raise ProviderRequestError(
                    f"{provider} returned non-JSON response",
                    error_class="invalid_json_response",
                    retryable=False,
                    status_code=resp.status,
                ) from exc

            text, input_tokens, output_tokens = _extract_response(provider, data)
            spent_micros = compute_spent_micros(input_tokens, output_tokens, spec)
            if spent_micros > reservation.reserved_micros:
                ledger.record_outcome(
                    reservation,
                    status="failed",
                    spent_micros=spent_micros,
                    error_class="usage_exceeded_reservation",
                )
                raise BudgetExceededError(
                    f"{provider} reported spend ${spent_micros / MICROS_PER_USD:.6f} "
                    f"exceeding reservation ${reservation.reserved_usd:.6f}",
                    error_class="usage_exceeded_reservation",
                )

            ledger.record_outcome(
                reservation, status="completed", spent_micros=spent_micros, error_class=None
            )
            return CallResult(
                reservation_id=reservation.reservation_id,
                provider=provider,
                model=model,
                text=text,
                input_tokens=input_tokens,
                output_tokens=output_tokens,
                reserved_usd=reservation.reserved_usd,
                spent_usd=round(spent_micros / MICROS_PER_USD, 6),
                attempts=attempts,
            )

        is_billing, is_retryable, error_class = classify_http_error(provider, resp.status, resp.body)
        if is_billing:
            ledger.record_outcome(
                reservation,
                status="billing_hard_stop",
                spent_micros=0,
                error_class=error_class,
            )
            raise BillingHardStopError(
                f"Hard stop: {provider} returned billing/credit error ({error_class}, HTTP {resp.status}); "
                "no retries attempted",
                error_class=error_class,
            )

        if is_retryable and attempts < total_allowed:
            sleep_fn(0.5 * attempts)
            continue

        final_status = "transient_error" if is_retryable else "failed"
        ledger.record_outcome(reservation, status=final_status, spent_micros=None, error_class=error_class)
        raise ProviderRequestError(
            f"{provider} request failed with HTTP {resp.status} ({error_class})",
            error_class=error_class,
            retryable=is_retryable,
            status_code=resp.status,
        )

    # Unreachable loop guard
    raise ProviderRequestError("Unexpected retry termination", error_class="unexpected_retry_end")


def _extract_response(provider: str, data: dict[str, Any]) -> tuple[str, int, int]:
    if not isinstance(data, dict):
        raise ProviderRequestError(f"{provider} response must be a JSON object", error_class="invalid_response")
    usage = data.get("usage")
    if not isinstance(usage, dict):
        raise ProviderRequestError(f"{provider} response missing usage object", error_class="missing_usage")

    if provider == "openai":
        choices = data.get("choices")
        if not isinstance(choices, list) or not choices:
            raise ProviderRequestError("OpenAI response missing choices", error_class="invalid_response")
        msg = choices[0].get("message") if isinstance(choices[0], dict) else None
        content = msg.get("content") if isinstance(msg, dict) else None
        if not isinstance(content, str):
            raise ProviderRequestError("OpenAI response missing text content", error_class="invalid_response")
        in_tok = usage.get("prompt_tokens", usage.get("input_tokens"))
        out_tok = usage.get("completion_tokens", usage.get("output_tokens"))
        if not isinstance(in_tok, int) or not isinstance(out_tok, int):
            raise ProviderRequestError("OpenAI usage missing integer token counts", error_class="invalid_usage")
        return content, in_tok, out_tok

    if provider == "anthropic":
        content_blocks = data.get("content")
        if not isinstance(content_blocks, list) or not content_blocks:
            raise ProviderRequestError("Anthropic response missing content blocks", error_class="invalid_response")
        parts = [
            block.get("text", "")
            for block in content_blocks
            if isinstance(block, dict) and block.get("type") == "text"
        ]
        text = "\n".join(part for part in parts if isinstance(part, str)).strip()
        if not text:
            raise ProviderRequestError("Anthropic response contained no text block", error_class="invalid_response")
        in_tok = usage.get("input_tokens")
        out_tok = usage.get("output_tokens")
        cache_create = usage.get("cache_creation_input_tokens", 0) or 0
        cache_read = usage.get("cache_read_input_tokens", 0) or 0
        if not isinstance(in_tok, int) or not isinstance(out_tok, int):
            raise ProviderRequestError("Anthropic usage missing integer token counts", error_class="invalid_usage")
        return text, in_tok + int(cache_create) + int(cache_read), out_tok

    raise ProviderRequestError(f"Unsupported provider {provider!r}", error_class="unsupported_provider")


def summarize_ledger(ledger_path: str | Path) -> dict[str, Any]:
    """Summarize a JSONL spend ledger into Fleshapron-ready weekly receipt fields."""
    path = Path(ledger_path)
    if not path.is_absolute():
        raise LedgerError("Absolute ledger path required", error_class="relative_ledger_path")
    if not path.exists():
        return {
            "ledger_path": str(path),
            "generated_at": utc_now_iso(),
            "entries": [],
            "totals": {
                "reserved_usd": 0.0,
                "spent_usd": 0.0,
                "reservations": 0,
                "completed": 0,
                "billing_hard_stops": 0,
                "failed": 0,
            },
        }

    rows: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))

    by_id: dict[str, dict[str, Any]] = {}
    for row in rows:
        rid = str(row.get("reservation_id") or "")
        if not rid:
            continue
        if rid not in by_id or row.get("status") != "reserved":
            by_id[rid] = {
                "time": row.get("time"),
                "product": row.get("product"),
                "provider": row.get("provider"),
                "model": row.get("model"),
                "reserved_usd": row.get("reserved_usd"),
                "spent_usd": row.get("spent_usd"),
                "status": row.get("status"),
                "error_class": row.get("error_class"),
                "_reserved_micros": int(row.get("reserved_micros") or 0),
                "_spent_micros": row.get("spent_micros"),
            }

    total_reserved_micros = 0
    total_spent_micros = 0
    completed = 0
    hard_stops = 0
    failed = 0
    entries: list[dict[str, Any]] = []
    for item in by_id.values():
        total_reserved_micros += item["_reserved_micros"]
        if isinstance(item["_spent_micros"], int):
            total_spent_micros += item["_spent_micros"]
        st = item["status"]
        if st == "completed":
            completed += 1
        elif st == "billing_hard_stop":
            hard_stops += 1
        elif st in ("failed", "transient_error"):
            failed += 1
        entries.append(
            {
                "time": item["time"],
                "product": item["product"],
                "provider": item["provider"],
                "model": item["model"],
                "reserved_usd": item["reserved_usd"],
                "spent_usd": item["spent_usd"],
                "status": st,
                "error_class": item["error_class"],
            }
        )

    return {
        "ledger_path": str(path),
        "generated_at": utc_now_iso(),
        "entries": entries,
        "totals": {
            "reserved_usd": round(total_reserved_micros / MICROS_PER_USD, 6),
            "spent_usd": round(total_spent_micros / MICROS_PER_USD, 6),
            "reservations": len(by_id),
            "completed": completed,
            "billing_hard_stops": hard_stops,
            "failed": failed,
        },
    }
