"""Strict public-doc bundler and prompt builder for the stranger clarity eval.

Guarantees that the stranger model sees ONLY explicitly allowlisted public docs
(README, quickstart, install, verify) and never lane files, CHARTER/SPEND-RULES,
AGENTS.md, DECISIONS.md, V0.1.0-SCOPE.md, symlinks, or host filesystem paths.
"""
from __future__ import annotations

import hashlib
import json
import os
import stat
from dataclasses import dataclass
from html.parser import HTMLParser
from pathlib import Path, PurePosixPath
from typing import Any

# Core public quickstart/install docs seen by a stranger by default.
DEFAULT_PUBLIC_DOCS: tuple[str, ...] = (
    "README.md",
    "site/docs/install/index.html",
    "site/docs/getting-started/index.html",
    "docs/VERIFY.md",
)

# Additional public user-facing docs that may be opted into via --extra-doc.
# Internal/unpublished docs (AGENTS.md, CLAUDE.md, DECISIONS.md, V0.1.0-SCOPE.md,
# SPEC.md, PR-TRIAGE.md, PROMOTION.md, COMPETITORS.md, .lanes/*) are NEVER here.
ALLOWED_OPTIONAL_PUBLIC_DOCS: frozenset[str] = frozenset(
    {
        "docs/ADDRESSING.md",
        "docs/BOARD.md",
        "docs/DAEMON.md",
        "docs/DOCTOR.md",
        "docs/GLOSSARY.md",
        "docs/HOLDS.md",
        "docs/HOOKS.md",
        "docs/INBOX.md",
        "docs/NEEDS.md",
        "docs/QUEUE.md",
        "docs/REQUESTS.md",
        "examples/README.md",
        "site/docs/concepts/index.html",
        "site/docs/faq/index.html",
        "site/docs/index.html",
    }
)

ALL_ALLOWED_PUBLIC_DOCS: frozenset[str] = frozenset(DEFAULT_PUBLIC_DOCS) | ALLOWED_OPTIONAL_PUBLIC_DOCS

# Explicit defense-in-depth denylist for internal/unpublished files.
FORBIDDEN_BASENAMES: frozenset[str] = frozenset(
    {
        "AGENTS.md",
        "CLAUDE.md",
        "CHARTER.md",
        "SPEND-RULES.md",
        "STATUS.md",
        "DONE.md",
        "DECISIONS.md",
        "V0.1.0-SCOPE.md",
        "SPEC.md",
        "PR-TRIAGE.md",
        "PROMOTION.md",
        "COMPETITORS.md",
        ".env",
    }
)

VALID_FINDING_CATEGORIES = frozenset(
    {
        "missing_step",
        "contradiction",
        "unpublished_knowledge",
        "ordering_or_ambiguity",
    }
)

VALID_SEVERITIES = frozenset({"blocker", "major", "minor"})


class IsolationViolationError(ValueError):
    """Raised when a non-public, symlinked, or out-of-repo file is requested."""


@dataclass(frozen=True)
class PublicDoc:
    rel_path: str
    sha256: str
    raw_bytes: int
    content: str


@dataclass(frozen=True)
class StrangerBundle:
    docs: tuple[PublicDoc, ...]

    def manifest(self) -> list[dict[str, Any]]:
        return [
            {
                "path": doc.rel_path,
                "sha256": doc.sha256,
                "raw_bytes": doc.raw_bytes,
                "rendered_chars": len(doc.content),
            }
            for doc in self.docs
        ]

    def render_corpus(self) -> str:
        sections: list[str] = []
        for doc in self.docs:
            sections.append(
                f"=== PUBLIC DOC: {doc.rel_path} ===\n{doc.content.strip()}\n=== END DOC: {doc.rel_path} ==="
            )
        return "\n\n".join(sections)


class _MainHtmlExtractor(HTMLParser):
    """Extract readable plain text with fenced code blocks from site HTML <main>."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._in_main = False
        self._skip_depth = 0
        self._in_pre = False
        self._in_code = False
        self._parts: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        tag = tag.lower()
        if tag == "main":
            self._in_main = True
            return
        if not self._in_main:
            return
        if tag in ("script", "style", "nav", "footer", "svg"):
            self._skip_depth += 1
            return
        if self._skip_depth > 0:
            return
        if tag in ("h1", "h2", "h3", "h4"):
            level = int(tag[1])
            self._parts.append("\n\n" + ("#" * level) + " ")
        elif tag in ("p", "div", "section"):
            self._parts.append("\n\n")
        elif tag == "li":
            self._parts.append("\n- ")
        elif tag == "br":
            self._parts.append("\n")
        elif tag == "pre":
            self._in_pre = True
            self._parts.append("\n\n```\n")
        elif tag == "code":
            self._in_code = True
            if not self._in_pre:
                self._parts.append("`")

    def handle_endtag(self, tag: str) -> None:
        tag = tag.lower()
        if tag == "main":
            self._in_main = False
            return
        if not self._in_main:
            return
        if tag in ("script", "style", "nav", "footer", "svg"):
            if self._skip_depth > 0:
                self._skip_depth -= 1
            return
        if self._skip_depth > 0:
            return
        if tag == "pre":
            self._in_pre = False
            self._parts.append("\n```\n\n")
        elif tag == "code":
            self._in_code = False
            if not self._in_pre:
                self._parts.append("`")

    def handle_data(self, data: str) -> None:
        if not self._in_main or self._skip_depth > 0:
            return
        if self._in_pre:
            self._parts.append(data)
        else:
            collapsed = " ".join(data.split())
            if collapsed:
                if self._parts and not self._parts[-1].endswith((" ", "\n", "`")):
                    self._parts.append(" ")
                self._parts.append(collapsed)

    def get_text(self) -> str:
        raw = "".join(self._parts)
        lines = [line.rstrip() for line in raw.splitlines()]
        cleaned: list[str] = []
        blank_run = 0
        for line in lines:
            if not line:
                blank_run += 1
                if blank_run <= 2:
                    cleaned.append("")
            else:
                blank_run = 0
                cleaned.append(line)
        return "\n".join(cleaned).strip()


def extract_html_main_text(html_text: str) -> str:
    parser = _MainHtmlExtractor()
    parser.feed(html_text)
    parser.close()
    return parser.get_text()


def _normalize_and_validate_rel_path(raw_rel: str) -> str:
    if not raw_rel or raw_rel.startswith("/") or "\\" in raw_rel:
        raise IsolationViolationError(f"Public doc path must be a relative POSIX path: {raw_rel!r}")
    posix = PurePosixPath(raw_rel)
    parts = posix.parts
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise IsolationViolationError(f"Path traversal or dot segment forbidden: {raw_rel!r}")
    if any(part.startswith(".") for part in parts):
        raise IsolationViolationError(f"Hidden path component forbidden: {raw_rel!r}")
    if any(part in FORBIDDEN_BASENAMES for part in parts):
        raise IsolationViolationError(f"Unpublished or internal file forbidden: {raw_rel!r}")
    normalized = posix.as_posix()
    if normalized not in ALL_ALLOWED_PUBLIC_DOCS:
        raise IsolationViolationError(
            f"Path {normalized!r} is not in the public stranger doc allowlist"
        )
    return normalized


def _read_regular_file_inside_repo(repo_root: Path, rel_path: str) -> bytes:
    resolved_root = repo_root.resolve()
    current = resolved_root
    for part in PurePosixPath(rel_path).parts:
        current = current / part
        if current.is_symlink():
            raise IsolationViolationError(f"Symlink rejected in stranger doc path: {rel_path!r}")
    resolved_target = current.resolve()
    try:
        resolved_target.relative_to(resolved_root)
    except ValueError as exc:
        raise IsolationViolationError(
            f"Path escapes repository root: {rel_path!r}"
        ) from exc

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        fd = os.open(str(current), flags)
    except FileNotFoundError as exc:
        raise FileNotFoundError(f"Required public doc not found: {rel_path}") from exc
    except OSError as exc:
        raise IsolationViolationError(
            f"Unable to safely open public doc {rel_path!r}"
        ) from exc
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            raise IsolationViolationError(f"Public doc is not a regular file: {rel_path!r}")
        chunks: list[bytes] = []
        while True:
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            chunks.append(chunk)
    finally:
        os.close(fd)
    return b"".join(chunks)


def build_stranger_bundle(
    repo_root: Path,
    *,
    doc_paths: tuple[str, ...] = DEFAULT_PUBLIC_DOCS,
    extra_docs: tuple[str, ...] = (),
) -> StrangerBundle:
    """Load and sanitize only explicitly allowlisted public docs for the stranger prompt."""
    requested = list(doc_paths)
    for item in extra_docs:
        if item not in requested:
            requested.append(item)
    if not requested:
        raise IsolationViolationError("At least one allowlisted public doc is required")

    docs: list[PublicDoc] = []
    for raw_rel in requested:
        rel_path = _normalize_and_validate_rel_path(raw_rel)
        raw_bytes = _read_regular_file_inside_repo(repo_root, rel_path)
        text = raw_bytes.decode("utf-8")
        rendered = extract_html_main_text(text) if rel_path.endswith(".html") else text
        docs.append(
            PublicDoc(
                rel_path=rel_path,
                sha256=hashlib.sha256(raw_bytes).hexdigest(),
                raw_bytes=len(raw_bytes),
                content=rendered,
            )
        )
    return StrangerBundle(docs=tuple(docs))


STRANGER_SYSTEM_PROMPT = """You are playing a developer who is a complete stranger to this software ("Another").
You have ZERO prior knowledge of the project, its internal team conventions, or unpublished notes.
Use ONLY the public documentation provided in the user message.

Your job:
1. Try to follow the documented install and core quickstart workflow from start to finish:
   - Install Another today (distinguishing what works right now from future release archives).
   - Initialize an isolated local node and check that it is healthy.
   - Run the core local shell workflow: register two agents (alice and bob), send a request, check inbox, check activity summary, test a path hold conflict, and reply/ack.
   - Preview/install client hooks and understand any manual trust steps and MCP wiring.
2. Output the exact shell commands you would run based strictly on the provided docs.
3. Flag every step that is missing, contradictory across sections/pages, requires unpublished knowledge, or is ambiguous/out of order.

Rules:
- Never invent undocumented flags or claim a problem unless you cite the exact doc path and quote from the provided text.
- Be concrete and specific. Prefer real blockers and contradictions over stylistic nitpicks.
- Respond with a single JSON object matching the exact schema requested in the user message. No markdown fences outside JSON."""


def build_stranger_prompts(bundle: StrangerBundle) -> tuple[str, str]:
    """Build (system_prompt, user_prompt) containing ONLY the allowlisted public docs."""
    schema_hint = {
        "proposed_commands": [
            {
                "step_id": "step-1",
                "task": "short task label",
                "command": "exact command string from docs",
                "expected_exit_code": 0,
                "doc_citation": "README.md",
                "notes": "why this command and any uncertainty",
            }
        ],
        "findings": [
            {
                "id": "F1",
                "category": "missing_step | contradiction | unpublished_knowledge | ordering_or_ambiguity",
                "severity": "blocker | major | minor",
                "step": "which quickstart/install step",
                "doc_ref": "README.md or site/docs/install/index.html etc.",
                "quote": "short verbatim excerpt from the provided public doc",
                "explanation": "what fails, contradicts, or requires unpublished knowledge",
                "suggested_fix": "minimal concrete doc improvement",
            }
        ],
        "summary": {
            "could_complete_without_guessing": False,
            "blocking_gaps_count": 0,
            "overall_clarity_score_1_to_5": 3,
            "stranger_verdict": "short 1-2 sentence summary",
        },
    }
    user_prompt = (
        "Follow the public documentation below as a first-time stranger trying to install Another "
        "and complete its core quickstart workflow.\n\n"
        "Return ONLY valid JSON matching this structure:\n"
        f"{json.dumps(schema_hint, indent=2)}\n\n"
        "Public documentation corpus:\n\n"
        f"{bundle.render_corpus()}\n"
    )
    return STRANGER_SYSTEM_PROMPT, user_prompt


JUDGE_SYSTEM_PROMPT = """You are an independent second-family reviewer auditing a docs-clarity evaluation.
You are given:
1. The exact public docs bundle that the stranger model saw.
2. The stranger model's proposed commands and clarity findings.
3. Optional sandbox execution results if commands were run against an isolated temporary home.

Verify each finding strictly against the provided public docs:
- Confirm findings that are genuinely supported by the docs (or proved by sandbox failures).
- Reject findings that misread the docs, invent requirements, or cite text that is not actually contradictory.
- Note any real missing step, contradiction, or unpublished-knowledge gap the stranger missed.

Respond with a single valid JSON object matching the requested schema. No extra prose."""


def build_judge_prompts(
    bundle: StrangerBundle,
    stranger_output: dict[str, Any],
    sandbox_results: list[dict[str, Any]] | None = None,
) -> tuple[str, str]:
    judge_schema = {
        "verdict": "pass | needs_doc_fixes | fail",
        "confirmed_findings": [
            {
                "finding_id": "F1",
                "severity": "blocker | major | minor",
                "reason": "why this finding is grounded in the docs",
            }
        ],
        "rejected_findings": [
            {
                "finding_id": "F2",
                "reason": "why the stranger misread or over-claimed",
            }
        ],
        "missed_issues": [
            {
                "category": "missing_step | contradiction | unpublished_knowledge | ordering_or_ambiguity",
                "severity": "blocker | major | minor",
                "doc_ref": "README.md",
                "explanation": "real clarity gap missed by the stranger",
            }
        ],
        "summary": "concise verdict summary for M10 docs triage",
    }
    payload_sections = [
        "Return ONLY valid JSON matching this schema:\n" + json.dumps(judge_schema, indent=2),
        "Stranger model output:\n" + json.dumps(stranger_output, indent=2),
    ]
    if sandbox_results is not None:
        payload_sections.append(
            "Sandbox CLI execution transcript (isolated temp ANOTHER_HOME):\n"
            + json.dumps(sandbox_results, indent=2)
        )
    payload_sections.append("Public documentation corpus:\n\n" + bundle.render_corpus())
    return JUDGE_SYSTEM_PROMPT, "\n\n".join(payload_sections)


def parse_and_validate_stranger_output(raw_text: str) -> dict[str, Any]:
    text = raw_text.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        if lines and lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    data = json.loads(text)
    if not isinstance(data, dict):
        raise ValueError("Stranger output must be a JSON object")
    if not isinstance(data.get("proposed_commands"), list):
        raise ValueError("Stranger output missing list field 'proposed_commands'")
    if not isinstance(data.get("findings"), list):
        raise ValueError("Stranger output missing list field 'findings'")
    summary = data.get("summary")
    if not isinstance(summary, dict):
        raise ValueError("Stranger output missing object field 'summary'")

    for idx, finding in enumerate(data["findings"]):
        if not isinstance(finding, dict):
            raise ValueError(f"Finding #{idx} must be an object")
        cat = finding.get("category")
        if cat not in VALID_FINDING_CATEGORIES:
            raise ValueError(f"Finding #{idx} has invalid category {cat!r}")
        sev = finding.get("severity")
        if sev not in VALID_SEVERITIES:
            raise ValueError(f"Finding #{idx} has invalid severity {sev!r}")
        for field in ("id", "doc_ref", "explanation"):
            if not isinstance(finding.get(field), str) or not finding[field].strip():
                raise ValueError(f"Finding #{idx} missing non-empty string field {field!r}")
    return data


def parse_and_validate_judge_output(raw_text: str) -> dict[str, Any]:
    text = raw_text.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        if lines and lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    data = json.loads(text)
    if not isinstance(data, dict):
        raise ValueError("Judge output must be a JSON object")
    if data.get("verdict") not in ("pass", "needs_doc_fixes", "fail"):
        raise ValueError(f"Invalid judge verdict: {data.get('verdict')!r}")
    for key in ("confirmed_findings", "rejected_findings", "missed_issues"):
        if not isinstance(data.get(key), list):
            raise ValueError(f"Judge output missing list field {key!r}")
    if not isinstance(data.get("summary"), str) or not data["summary"].strip():
        raise ValueError("Judge output missing non-empty summary")
    return data
