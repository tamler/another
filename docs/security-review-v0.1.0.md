# Security review: v0.1.0

Reviewed on `claude/v0.1.0-readiness` (2026-11-04), before tagging. Scope:

- signed webhooks;
- remote invites, join and kick;
- federation relay identity;
- HTTP auth and scope checks;
- spend lease approval;
- `scripts/install.sh`.

Findings are ranked by severity. Low-risk items were fixed in this PR, each with a regression test that fails before the fix. The rest are listed with a recommendation and left open.

| # | Severity | Finding | Status |
|---|---|---|---|
| 1 | **High** | Without `--auth`, the local HTTP API accepted any `Host`, so DNS rebinding or a tunnel could reach it | **Fixed** (D45), plus a docs change |
| 2 | **Medium** | Local callers are not identified, so the spend approval gate and the relay rules don't hold against a local agent | Open (design) |
| 3 | Low | Remote tokens can point webhooks at the hub's own loopback or link-local addresses (blind SSRF) | Open |
| 4 | Low | `install.sh` executed partially when a `curl \| sh` download was cut off | **Fixed** |
| 5 | Low | `install.sh` checks against a `SHA256SUMS` from the same release (integrity, not authenticity) | Open |
| 6 | Low | `/join` rate limit and audit address trust client-supplied forwarding headers | Open |
| 7 | Low | The federation hub vouches for third-party node keys (trust on first use, via the hub) | Open (documented design) |
| 8 | Info | `agent_kick` is documented for remote managers but refused for every token (fails closed) | Open |
| 9 | Info | Webhook retries are re-signed with a new timestamp; receivers should dedupe by delivery id | Docs updated |

What was checked and found sound is listed at the end.

---

## 1. High: the unauthenticated HTTP API was reachable through DNS rebinding and tunnels (fixed)

**What:** a server without `--auth` (the default `another serve`, and the federation hub as FEDERATION.md set it up) answers as this machine's person. It serves:
- `POST /decisions/{id}/answer`, which includes spend-lease approvals;
- `GET /summary`, `/activity`, `/events/stream`, `/events/poll`, `/decisions`, `/spend` and `/lanes`;
- the remote-agent routes.

rmcp checks the `Host` header for `/mcp` only. The other routes accepted any `Host`.

**Impact:**
- **DNS rebinding.** A web page the user visits can rebind its own name to 127.0.0.1. It can then read the activity feed, summary and decisions, and answer any open decision, including approving a spend lease, with no token.
- **Tunnel exposure.** FEDERATION.md told hubs to run `another serve --federation --bind 127.0.0.1:7468` without `--auth`, behind a tunnel. Only `/fed/*` checks signatures, so anyone who could reach the tunnel URL got the same unauthenticated API.

**Fix:**
- `serve::require_loopback_host`: without `--auth`, every route except `/fed/*` and `/health` refuses a `Host` other than `localhost`, `127.0.0.1` or `[::1]`, with or without a port. These are the names rmcp accepts. The response is a 403.
- A request with no `Host` passes, because browsers always send one.
- `/fed/*` keeps its signature check and still works through a tunnel.
- FEDERATION.md now sets the hub up with `--auth --allowed-host …`. D45 records the rule.

**Tests:**
- `tests/hardening.rs::unauthenticated_servers_only_answer_loopback_host_names`:
  - foreign `Host` names (a rebinding name, a tailnet name, `127.0.0.1.evil.example`) get 403 on the read routes and on the answer route, and the decision stays open;
  - `/fed/pull` still reaches its own signature check (401);
  - loopback names, and the local answer, still work.
  - Before the fix, `GET /summary` with `Host: evil.example` returned 200.
- `serve::tests::loopback_host_names` covers the parser, including `[::1].evil` and lookalike suffixes.

**Remaining:** a tunnel that delivers as `localhost` gets past the Host check, for example `ssh -R` when the remote side connects to `localhost:PORT`. Serve anything you tunnel with `--auth`. REMOTE.md and FEDERATION.md both say so now.

## 2. Medium: local callers are not identified (open, design)

**What:** stdio MCP and the unauthenticated loopback server carry no caller identity, so every local process is "this machine". As a result:
- A local agent can answer its own decision. It can call `decision_answer` with `relayed_by` naming a confirmed person or manager. It can POST to `/decisions/{id}/answer` on the loopback server, with no relayer needed. Or, with a shell, it can run `another decision answer`.
- That includes the spend approval decision. **"The borrower can't relay its own approval" holds only for token callers.** Locally the approval gate is a convention, not a control.
- The same goes for `agent_kick` over local MCP (naming a `grok_bot` agent as `agent`) and for `send_message` (`from` any agent).

**Recommendation:**
- In SPEND.md and HUMAN-DECISIONS.md, state that approvals guard against remote agents, and against local agents only by convention.
- For a real local gate, answer approvals only through an authenticated channel: a manager bot with a token, or the person's phone through it. Run local agents with per-agent tokens on an `--auth` server (D15 already allows this).
- Later: per-agent local credentials, for example a token in each hook-installed client config, so local MCP calls carry an identity.

Not fixed here. Any fix changes how every local client connects.

## 3. Low: a remote token's webhooks can target the hub itself (open)

**What:** `validate_webhook_url` allows `http://` to loopback and `https://` to any host, and this applies to webhooks a token creates too. The hub then POSTs the event JSON to that address, for example `http://127.0.0.1:<port>/…`, `https://169.254.169.254/…` or a LAN address.

**Impact: blind SSRF.**
- The body is Another's fixed, signed event JSON, and the headers are fixed.
- The token sees only `pending`, `delivered` and `dead` counts, never a response, so it can probe the hub's local ports for "answers 2xx".
- Cloud metadata services want GET or PUT with special headers, so a POST gets nothing from them.

**Why it isn't fixed here:** refusing loopback for token webhooks breaks a manager that holds a token and has a receiver on the same machine. `tests/push.rs` covers that setup.

**Recommendation:**
- Add an opt-in node setting, such as `remote_webhook_private_hosts`, defaulting to off. With it off, refuse loopback, unspecified and link-local hosts for token-created webhooks, including the `127.1` and `0x7f000001` forms.
- Check the resolved address at delivery, so a DNS name that resolves to 127.0.0.1 can't bypass the check.

## 4. Low: `curl | sh` partial execution (fixed)

**What:** `install.sh` ran at top level. If the download stopped part-way, `sh` would run the lines it had received. Cutting after line 45 started the download step.

**Fix:** the body is one `main` function, called on the last line. A truncated script defines an unfinished function, or one that is never called, and runs nothing.

**Test:** `tests/install_script.rs::a_truncated_download_runs_nothing` cuts the script after every line, runs each prefix with `sh -s`, and asserts it produces no output and writes nothing. Before the fix it failed at line 45 ("downloading another 0.1.0 …").

## 5. Low: checksums come from the same place as the archive (open)

**What:** `install.sh` downloads the archive and `SHA256SUMS` from the same release. It refuses mismatches and missing lines, verifies before unpacking, and swaps the binary in atomically. That catches corruption and a tampered mirror archive, but not a compromised release or account, which could replace both files.

**Recommendation:**
- Sign `SHA256SUMS` (minisign or cosign) or publish GitHub artifact attestations, and verify in the script when the tool is available.
- Pass `--proto '=https' --proto-redir '=https'` to `curl` when `ANOTHER_DOWNLOAD_BASE` is not overridden. Tests and mirrors use `file://`.

## 6. Low: forwarding headers are trusted for the `/join` limit and the audit address (open)

**What:** `remote_addr` uses the first value of `cf-connecting-ip`, `x-forwarded-for` or `x-real-ip` when present. A client that sends its own `X-Forwarded-For` can rotate values past the 10-per-minute `/join` limit, and choose the address the audit log records. The audit entry does name the header, for example `x-forwarded-for: 1.2.3.4`.

**Impact:** low. Invite codes carry 192 random bits and are compared as hashes in constant time, so the limit isn't what stops guessing.

**Recommendation:** trust forwarding headers only when the TCP peer is loopback (a local tunnel). Take the **last** `X-Forwarded-For` hop the tunnel added, not the first, which the client wrote.

## 7. Low: the hub vouches for third-party node keys (open, documented)

**What:** a spoke learns other nodes' public keys from the hub's node list. Keys are pinned on first sight: a changed key is refused ("presented a different key than before"). A malicious hub, though, can introduce new node identities, and it reads relayed mail bodies (D28).

**Recommendation:** none for 0.1.0 beyond hosting the hub yourself, as FEDERATION.md says. Later: show key fingerprints in `another peers`, plus an optional allowlist of node keys per spoke.

## 8. Info: `agent_kick` is refused for every token (open, fails closed)

**What:** REMOTE-AGENTS.md and the tool description say a manager bot with a token can kick agents in its projects. `Store::kick_agent_as` implements those checks: a `grok_bot` caller acting as itself, the token's projects only, no people. But `remote::authorize` has no arm for `agent_kick`, so every token gets "tool is not available to remote peers".

**Recommendation:** decide which is intended. To enable it, add `"agent_kick" => acting(args, "agent")` to `remote::authorize` with a token test. To keep it local, fix the docs and the tool description. It was left as is: enabling it grants a privilege, which is beyond a hardening fix.

## 9. Info: dedupe webhook retries by delivery id (docs)

Each attempt is signed with a new `X-Another-Timestamp`, so a receiver with a replay window still accepts a retry. That is intended. Receivers should dedupe on `X-Another-Delivery`. PUSH.md now says so.

---

## Checked and sound

**Signed webhooks:**
- HMAC-SHA256 over `"<timestamp>.<body>"`, with a 32-byte random `whsec_` secret per webhook, shown once.
- PUSH.md shows verification with `hmac.compare_digest` and a 5-minute window.
- No redirects (`max_redirects(0)`), a 10 s timeout, and `https`, or `http` to loopback only, with no credentials in URLs.
- Webhooks a token created stop when the token is revoked or expires, and follow its scope (D38).

**Tokens:**
- `ano_<id>_<secret>`, stored as SHA-256 and compared with `ct_eq`.
- Unknown, malformed, revoked and expired tokens all get one error.
- Expiry and revocation are checked on every request.
- `--auth` checks loopback requests too (D15).

**Invites and join:**
- `anj_<id>_<secret>` with 192 random bits, stored hashed and compared with `ct_eq`.
- Single-use by default, with an expiry.
- An invite never takes over an existing agent's name.
- `/join` is served only with `--auth`, and every attempt is audited.

**Federation:**
- Requests are signed with ed25519 over method, path and query, time, and the body hash, within ±5 minutes.
- Only an unrevoked spoke may call.
- Replays inside the window are harmless: push is idempotent by `event_id`, and pull is read-only.
- Every event is signed by its origin and verified end to end.
- Spokes may push only events they authored. Events about the receiving node are refused.
- A relayed decision answer must come from an agent of the node that signed it, and still goes through the local relayer rules.

**HTTP scope:**
- The tool allowlist (`remote::authorize`) denies anything it doesn't name.
- Hidden objects get the same `not_found` as missing ones.
- Reaping, spend account configuration and `all_projects` webhooks are local only.
- A non-loopback bind needs both `--auth` and `--allow-remote-bind`.

**Spend approval** (for token callers):
- The asker can't relay its own answer.
- A relayer must be a `grok_bot` or `human` confirmed on its own node; people created on demand can't relay.
- Withdrawing a pending lease withdraws its decision.
- Tokens see other projects' accounts read-only.

**`install.sh`:**
- Checks the archive against `SHA256SUMS` before unpacking, refuses a mismatch or a missing line, and needs `sha256sum` or `shasum`.
- Installs through a temporary file and `mv`, with `set -eu`.
- Cleans up its temporary files with `trap`.
