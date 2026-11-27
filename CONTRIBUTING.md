# Contributing to Another

Thanks for helping. Another is free and open source, always: every feature ships in this repository under [Apache-2.0](LICENSE). There is no paid tier, no telemetry and no open-core split.

## Ground rules

1. **Clean room.** Implement from this repository's docs ([docs/SPEC.md](docs/SPEC.md), [docs/DECISIONS.md](docs/DECISIONS.md), [AGENTS.md](AGENTS.md)) and public documentation of the tools we integrate with.
   - Do **not** copy code, schemas, SQL, distinctive APIs or phrasing from `mcp_agent_mail`, its forks, or any project with a non-permissive license or license rider.
   - Prior art listed in SPEC is for ideas only.
2. **Vendor-neutral.** Claude Code, Codex, Grok CLI, Grok Bot and Cursor are treated equally. Don't tune behaviour for one vendor; when clients differ, handle the difference explicitly and document it.
3. **Safe defaults.**
   - Bind to loopback by default. A non-loopback bind requires both `--auth` and `--allow-remote-bind`.
   - Never terminate a process without a human confirmation flag.
   - Never allow remote reaping.
   - Agents act only on their own node.
4. **No secrets or personal data in the tree.** That means tokens, emails, phone numbers and machine-specific absolute paths. Tests use temporary directories and placeholder paths such as `/work/app`.
5. **Record design calls.** If a change makes a decision the docs don't cover, add a row to the decisions table in [docs/DECISIONS.md](docs/DECISIONS.md) in the same PR.

## Setup

You need Rust **1.88 or newer**; that is the MSRV and CI checks it.

```bash
git clone https://github.com/tamler/another
```

```bash
cd another && cargo test
```

The workspace has two crates:

| Crate | What lives there |
|---|---|
| `crates/another-core` | SQLite store, event log and signatures, agents, mail, requests, holds, activity, resources, tokens, federation logic. No async, no networking beyond reachability checks. |
| `crates/another` | The `another` binary and library: CLI, MCP server (`rmcp`), HTTP serving and auth, the loopback dashboard, hook packs, orphan scanner and reaping, federation transport, wake adapters. |

Schema changes are new entries at the end of `MIGRATIONS` in `crates/another-core/src/store.rs`; never edit an existing migration. Each new migration also appends its digest to `CATALOGS` in the same file (the test `every_schema_version_has_its_frozen_catalog` prints it). The digest covers columns, indexes and foreign keys; it does not cover CHECK constraints, partial-index predicates, collations, generated columns, triggers or views. No schema has one today, so a migration that adds one must widen `SHAPE` first. Every mutation appends an event first, then updates projections in the same transaction.

## Before you open a PR

A PR needs a ready queue item and must name its scope ID. See [AGENTS.md](AGENTS.md) for admission rules and [the queue guide](docs/QUEUE.md) for commands.

Run what CI runs:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo test
```

```bash
cargo +1.88 check --workspace --all-targets --locked
```

- Add tests for new behaviour. Core logic gets tests in `crates/another-core/tests`; CLI, MCP, hooks and HTTP get tests in `crates/another/tests`, which run the real binary, with a temporary `HOME` / `ANOTHER_HOME` where relevant.
- Keep the **Status** section of [README.md](README.md) and the current [AGENTS.md](AGENTS.md) brief accurate when milestone progress changes.
- Dependency updates must keep building on Rust 1.88. `resolver = "3"` in `Cargo.toml` makes `cargo update` prefer compatible versions.
- Copy lint: `python3 scripts/check-copy.py`.
- Website edits: `sh site/build.sh` with Python 3. That regenerates `site/dist`, checks links and SEO metadata, and runs `scripts/check-copy.py` (em dashes, en dashes, and banned phrases in `site/`, `docs/`, and CLI strings in `crates/another/src/cli.rs`). Deploying the site is a maintainer action.

## Releases

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`. It builds macOS (arm64, x86_64) and static Linux (x86_64, arm64) archives and publishes them with SHA-256 checksums as a GitHub Release. [docs/VERIFY.md](docs/VERIFY.md) names the files it uploads and shows how a user checks one.

## License

By contributing you agree that your contributions are licensed under the Apache License 2.0, the same terms as the project.
