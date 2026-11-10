# AGENTS.md: build brief (Milestone 7: hosted connectors)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and every milestone decision in [docs/DECISIONS.md](docs/DECISIONS.md); contributor rules in [CONTRIBUTING.md](CONTRIBUTING.md). This file is the scope for the current coding milestone.

## Status

Milestone 7: **in review** (definition of done below). Milestone 6 is complete. Earlier: M1 local core (tamler/another#1), M2 mail checking + activity feed (tamler/another#2), M3 resource registry (tamler/another#3), M4 authenticated remote peer (tamler/another#4), M5 federation (tamler/another#5). Keep the [README.md](README.md) status current in every PR.

## Milestone 7, in scope

Hosted apps (ChatGPT custom connectors, Claude.ai and Claude Desktop connectors, and any MCP client that signs in with OAuth) join the same node as local agents. No paid relay and no telemetry. Bearer tokens from milestone 4 keep working.

1. **OAuth 2.1 on `another serve --auth --public-url`.** Protected-resource and authorization-server metadata, S256 PKCE, RFC 8707 `resource`, RFC 9207 `iss`. Client ID Metadata Documents and dynamic registration. Public clients only. A grant exists only after `another oauth approve` on the machine that holds the database. Each grant is one agent within project globs. Access tokens last one hour. Refresh tokens rotate, and reuse revokes the grant. The authorization code and the one-time redirect token are stored only as SHA-256 hashes. The code is minted in the server on the first poll and returned once.
2. **`another remote up`.** Prints the URL to paste. Named-tunnel recipe uses a placeholder hostname. Quick tunnel is optional and ephemeral.
3. **ChatGPT desktop local path.** Settings, MCP servers: stdio `another serve --stdio`, or Streamable HTTP to `http://127.0.0.1:7466/mcp`. No tunnel. Documented in [docs/REMOTE.md](docs/REMOTE.md). Web plugins and developer-mode custom connectors stay remote (public HTTPS or Secure MCP Tunnel).
4. **`whats_new`.** One call for a chat app: unread subjects, open requests, recent activity, no message bodies.
5. **Client metadata fetch.** Resolve the URL's real port, refuse non-public addresses, then connect only to that pinned address with the original Host and SNI. Cache is capped. Body read stops at 64 KiB.

### Definition of done (Milestone 7)

- [x] OAuth flow, scope enforcement, revocation, refresh reuse, Host and Origin checks, and OAuth rate limits covered by tests
- [x] Authorization code plaintext is not stored; only the hash, issued once
- [x] CIMD fetch pins the validated address and uses the URL port; tests cover the pin, the port, the body cap, and the cache cap
- [x] Bearer tokens still work
- [x] `whats_new` and `another remote up`
- [x] README status and this brief name milestone 7
- [x] docs/REMOTE.md explains ChatGPT desktop (local), ChatGPT web, and Claude
- [ ] CI green on the PR (test, clippy `-D warnings`, fmt, MSRV 1.88, copy lint)

## Clean-room rules (mandatory)

- Implement from **this repository's docs** and public docs of integrated tools (tmux, cmux, Herdr, GitHub Actions) only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.
- cmux is GPL: call its CLI as a separate process only; never link or vendor it.

## Locked defaults

Apache-2.0; agent names unique per project; hooks-first (wake adapters are optional extras); free and open source always; loopback-only without auth; never reap without human approval; never remote reap; MSRV 1.88.

## Milestone 6 — in scope

1. **`another wake`** — nudge idle agents with unread mail through optional adapters: `tmux` (send-keys), `cmux` (`send --surface` or `notify`), `herdr` (`agent prompt`). Stored per-agent targets (`another wake set|clear|list`), auto-captured by session hooks from `$TMUX_PANE` / `$CMUX_SURFACE_ID` (never replacing manual targets); `--all`, `--dry-run`, `--force`, `--message`. Argv only, no shell; skip when nothing is waiting.
2. **Herdr plugin (optional)** — [integrations/herdr](integrations/herdr): `herdr-plugin.toml` with wake + summary actions and a live activity pane; POSIX `sh` helper.
3. **Release builds** — `.github/workflows/release.yml`: macOS arm64 + x86_64, Linux static musl x86_64 + arm64; smoke tests; SHA-256 sums; GitHub Release on `v*` tags; build-only on PRs touching the workflow.
4. **Docs** — clean README quickstart, [CONTRIBUTING.md](CONTRIBUTING.md), [docs/WAKE.md](docs/WAKE.md).

### Definition of done (Milestone 6)

- [x] `another wake` with tmux / cmux / Herdr adapters, stored + auto-captured targets, tests with fake adapter binaries
- [x] Optional Herdr plugin (manifest + helper), validated in tests
- [x] Release workflow for macOS (arm64, x86_64) and Linux, verified by a PR build
- [x] README quickstart + CONTRIBUTING.md + docs/WAKE.md
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope

- TUI, auto-reap policies, pre-commit hold guard, mesh federation, end-to-end mail encryption (future work; see SPEC "Later")

A loopback dashboard shipped after this milestone. It is not part of the M6 brief above. See [docs/DASHBOARD.md](docs/DASHBOARD.md) and decisions D39-D44.

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
