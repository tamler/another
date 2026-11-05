# AGENTS.md — build brief (coordination series)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and every decision in [docs/DECISIONS.md](docs/DECISIONS.md); contributor rules in [CONTRIBUTING.md](CONTRIBUTING.md). This file is the scope for the current work: the **coordination series**, which turns Another from a shared inbox into the coordination layer between a manager (often a remote bot relaying a person on their phone) and coding agents.

## Status

Milestones 1–6 are complete (tamler/another#1–#8). The coordination series is in progress. It is a stack of PRs, one per phase, each based on the previous one:

| Phase | Scope | Design | PR |
|---|---|---|---|
| 1 | Lanes: units of work with charter, lead, status, structured reports | [01-lanes](docs/design/01-lanes.md) | tamler/another#10 |
| 2 | Push: scope-filtered SSE / long-poll, signed webhooks | [02-push](docs/design/02-push.md) | tamler/another#11 |
| 3 | Human decisions: structured questions answered by phone/HTTP/CLI | [03-decisions](docs/design/03-decisions.md) | tamler/another#12 |
| 4 | Lane-aware holds, pre-edit hook | [04-lane-holds](docs/design/04-lane-holds.md) | tamler/another#13 |
| 5 | Spend ledger, approval gate | [05-spend](docs/design/05-spend.md) | tamler/another#14 |
| 6 | Daily summary | [06-summary](docs/design/06-summary.md) | tamler/another#15 |
| 7 | First-class remote agents: invites, join, lifecycle, federation tie-in | remote-agents | (planned) |

Keep the [README.md](README.md) status and [CHANGELOG.md](CHANGELOG.md) (Unreleased) current in every PR.

## How each phase is built

1. **Design note first**: `docs/design/NN-*.md`, committed before any code. Record decisions in DECISIONS.md (next free D-number).
2. **Migrations are additive**: new tables or nullable columns, an upgrade test from the previous schema (`Store::open_at_schema`). Unreleased migrations in an open PR may be edited in place; released ones never.
3. **Don't break existing CLI/MCP behaviour**: deprecate rather than remove; new behaviour is opt-in or additive.
4. **Every remote surface** (MCP tool, HTTP route) sits behind `--auth`, respects token project globs via `remote::current_scope()` / store `scope` arguments, and is audited.
5. **Scoping rules**:
   - Out-of-scope objects look exactly like missing ones (same `not_found` error, same message shape). Filters on list tools are matched against what the caller may see; they never resolve names node-wide (no `ambiguous` errors naming hidden objects).
   - Events carry ids, names and statuses, never bodies (charters, summaries, report text, links).
   - A security test proves a token scoped to project A can't read project B's data through every new surface.
6. **Checks**: `cargo test --workspace`, `cargo +stable clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo +1.88 check --workspace --all-targets --locked`.
7. **Testing never touches live data**: use a temporary `ANOTHER_HOME` and ephemeral or non-default ports. Never `~/.another`, never `127.0.0.1:7466`.
8. **Open the PR and stop**: no merges, no tags, no DNS, domain or deploy changes.

## Review routing rule

Every PR gets a review from a **different model family** than the one that wrote it:

| Author | Reviewer | How to request |
|---|---|---|
| Claude | Codex | Comment `@codex review` on the PR |
| Codex | Claude | Ask in the PR, or open a Claude Code review session |
| Grok | Claude or Codex | As above |

If no reviewer from another family is available or responds, the author writes a **self-review** in the PR description: risks, trade-offs, what was not tested. Open product questions go under **NEEDS DECISION** in the PR description, and the work continues.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** and the public docs of integrated tools (tmux, cmux, Herdr, GitHub Actions, client hook docs) only.
- **Do not** copy code, schemas, SQL, phrasing or distinctive APIs from `mcp_agent_mail*`, its forks, or any project with a non-permissive license rider.
- cmux is GPL: call its CLI as a separate process only; never link or vendor it.

## Locked defaults

- Apache-2.0; free and open source, always.
- Agent names unique per project; lane names unique per project.
- Hooks first; wake adapters are optional extras.
- Loopback only without auth. Token auth on everything remote. No new hosted dependencies.
- Never reap without human approval; never reap remotely.
- Never commit secrets, tokens, emails, phone numbers or machine-specific absolute paths.
- MSRV 1.88.

## Explicitly OUT of scope

Web UI / TUI, auto-reap policies, mesh federation, end-to-end mail encryption, and bundling network software such as Tailscale into the binary. See SPEC "Later".
