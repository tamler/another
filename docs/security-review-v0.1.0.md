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
| 3 | Low | Remote tokens can point webhooks at the hub's own loopback or link-local addresses (blind SSRF) | **Fixed** (D46); private LAN ranges stay allowed |
| 4 | Low | `install.sh` executed partially when a `curl \| sh` download was cut off | **Fixed** |
| 5 | Low | `install.sh` checks against a `SHA256SUMS` from the same release (integrity, not authenticity) | Documented; signing still open |
| 6 | Low | `/join` rate limit and audit address trust client-supplied forwarding headers | **Fixed** (D47) |
| 7 | Low | The federation hub vouches for third-party node keys (trust on first use, via the hub) | Open (documented design) |
| 8 | Info | `agent_kick` was documented for remote managers but is refused for every token (fails closed) | **Docs fixed**; behaviour unchanged |
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

## 3. Low: a remote token's webhooks can target the hub itself (fixed)

**What:** `validate_webhook_url` allowed `http://` to loopback and `https://` to any host, and this applied to webhooks a token creates too. The hub then POSTed the event JSON to that address, for example `http://127.0.0.1:<port>/…`, `https://169.254.169.254/…` or a LAN address.

**Impact: blind SSRF.**
- The body is Another's fixed, signed event JSON, and the headers are fixed.
- The token sees only `pending`, `delivered` and `dead` counts, never a response, so it can probe the hub's local ports for "answers 2xx".
- Cloud metadata services want GET or PUT with special headers, so a POST gets nothing from them.

**Fix (D46):**
- New node setting `remote_webhook_private_hosts`, default `off`, settable only on the machine. With it off, a token's `webhook_subscribe` is refused for loopback, unspecified (`0.0.0.0/8`, `::`), link-local (`169.254.0.0/16`, `fe80::/10`) and known metadata hosts (`100.100.100.200`, `168.63.129.16`, `fd00:ec2::254`, `metadata.google.internal`, `metadata.goog`, `instance-data.ec2.internal`).
- The check covers the spellings a system resolver accepts: `127.1`, `0x7f000001`, `2130706433`, `0177.0.0.1`, a trailing dot, `*.localhost`, and IPv4 embedded in IPv6 (mapped, compatible, NAT64, 6to4). A host that isn't plainly a DNS name or an address (percent-encoding, IPv6 zone ids, a numeric last label that isn't a valid address) is refused too.
- At delivery, the hub re-checks the host and every address the name resolves to. The check runs inside the connection's own lookup, so a second DNS answer can't get past it, and one refused address refuses the whole answer. Turning the setting off applies to existing webhooks.
- These deliveries skip `HTTP(S)_PROXY`, because a proxy would resolve the name itself.

**Compatibility:** a manager that holds a token and has a receiver on the same machine now needs `another config remote_webhook_private_hosts on`. Local webhooks are unaffected. A hub that can reach the internet only through a proxy can't deliver token webhooks while the setting is off.

**Tests** (each failed before the fix, except the two marked as controls):
- `another-core tests/push.rs::token_webhooks_refuse_hub_local_and_metadata_addresses`. Before the fix: "http://127.0.0.1:9/x was accepted".
- `another-core tests/push.rs::the_person_can_allow_private_hosts_for_token_webhooks`.
- `another tests/push.rs::a_token_webhook_to_loopback_is_not_delivered_once_private_hosts_are_off`.
- `another src/push.rs::a_name_resolving_to_a_hub_address_is_not_connected_to` uses a stub DNS answer and a real listener: nothing connects, and the same answer without the guard does. This test was written after the code; it was confirmed by disabling the guard.
- Control: `token_webhooks_keep_external_https_and_local_webhooks_keep_loopback` keeps external https and local loopback working.
- Control: `remote_webhooks_and_streams_respect_token_scope` still covers a same-machine receiver, now with the setting on.

**Remaining:**
- Private LAN and tailnet addresses (`10/8`, `172.16/12`, `192.168/16`, `fc00::/7`, `100.64/10`) stay reachable over https, because a receiver there is an ordinary setup. A token can still probe them for "answers 2xx with a certificate the hub trusts" through the delivery counts. Refusing them would be a separate decision, and would need a way to allow specific receivers.
- The delivery check uses ureq's `unversioned` resolver API, which can change in a minor release. `Cargo.lock` pins the version, and the test above fails if it stops working.

## 4. Low: `curl | sh` partial execution (fixed)

**What:** `install.sh` ran at top level. If the download stopped part-way, `sh` would run the lines it had received. Cutting after line 45 started the download step.

**Fix:** the body is one `main` function, called on the last line. A truncated script defines an unfinished function, or one that is never called, and runs nothing.

**Test:** `tests/install_script.rs::a_truncated_download_runs_nothing` cuts the script after every line, runs each prefix with `sh -s`, and asserts it produces no output and writes nothing. Before the fix it failed at line 45 ("downloading another 0.1.0 …").

## 5. Low: checksums come from the same place as the archive (documented; signing open)

**What:** `install.sh` downloads the archive and `SHA256SUMS` from the same release. It refuses mismatches and missing lines, verifies before unpacking, and swaps the binary in atomically.

**What that proves:**
- **Integrity: yes.** It catches a truncated or corrupted download, and an archive changed without also changing `SHA256SUMS`.
- **Authenticity: no.** Nothing shows who built the files. `SHA256SUMS` is unsigned, so a compromised release or GitHub account can replace both files and the check still passes. So can a mirror named with `ANOTHER_DOWNLOAD_BASE`, which supplies both. Trust rests on HTTPS and on that account.

README.md's install section now says this.

**Recommendation (still open):**
- Sign `SHA256SUMS` (minisign or cosign) or publish GitHub artifact attestations, and verify in the script when the tool is available.
- Pass `--proto '=https' --proto-redir '=https'` to `curl` when `ANOTHER_DOWNLOAD_BASE` is not overridden. Tests and mirrors use `file://`.

## 6. Low: forwarding headers are trusted for the `/join` limit and the audit address (fixed)

**What:** `remote_addr` used the first value of `cf-connecting-ip`, `x-forwarded-for` or `x-real-ip` when present. A client that sent its own `X-Forwarded-For` could rotate values past the 10-per-minute `/join` limit, and choose the address the audit log recorded.

**Impact:** low. Invite codes carry 192 random bits and are compared as hashes in constant time, so the limit isn't what stops guessing.

**Fix (D47):**
- By default, forwarding headers are ignored. The limit and the audit log use the TCP peer.
- `another serve --auth --trusted-proxy-header NAME` names the one header the local tunnel sets (`cf-connecting-ip`, `x-forwarded-for` or `x-real-ip`). It counts only when the TCP peer is loopback, only its **last** value (the one the tunnel added), and only if that parses as an IP address. Other headers stay ignored.
- This goes further than this review first recommended (trust them from any loopback peer). That wouldn't have been enough: `ssh -R` and proxies that pass headers through also deliver from loopback, with whatever the client wrote.

**Behaviour change:** without the flag, all clients of a tunnel share one `/join` limit, and the audit log shows `127.0.0.1:PORT`. Ten bad attempts a minute through the tunnel can make a real redeemer wait a minute. REMOTE.md explains how to name the header.

**Tests:**
- `tests/hardening.rs::forwarding_headers_are_ignored_by_default`. Before the fix, twelve guesses with rotating `X-Forwarded-For` all got 403 and never 429.
- `tests/hardening.rs::a_trusted_proxy_header_names_the_client_by_its_last_value`: clients get their own limit, a spoofed first hop doesn't help, and other headers stay ignored.
- `tests/hardening.rs::the_trusted_proxy_header_flag_needs_auth`.
- `remote::tests::forwarding_headers_count_only_from_a_local_peer_and_only_when_named`: non-loopback peers, IPv4-mapped loopback, a second header line and non-address values. This test was written after the code; it was confirmed by disabling the peer check.

**Remaining:** naming a header that the tunnel neither overwrites nor appends to brings the spoofing back. The tests use direct requests; no real tunnel was run, so what Tailscale Serve and cloudflared send wasn't verified here. REMOTE.md tells the operator to confirm it in the audit log.

## 7. Low: the hub vouches for third-party node keys (open, documented)

**What:** a spoke learns other nodes' public keys from the hub's node list. Keys are pinned on first sight: a changed key is refused ("presented a different key than before"). A malicious hub, though, can introduce new node identities, and it reads relayed mail bodies (D28).

**Recommendation:** none for 0.1.0 beyond hosting the hub yourself, as FEDERATION.md says. Later: show key fingerprints in `another peers`, plus an optional allowlist of node keys per spoke.

## 8. Info: `agent_kick` is refused for every token (docs fixed, fails closed)

**What:** REMOTE-AGENTS.md, the design note and the tool description said a manager bot with a token could kick agents in its projects. `Store::kick_agent_as` implements those checks: a `grok_bot` caller acting as itself, the token's projects only, no people. But `remote::authorize` has no arm for `agent_kick`, so every token gets "tool is not available to remote peers".

**Fix:** the docs and the tool description now match the code: local only. The behaviour didn't change, and `tests/remote.rs::agent_kick_is_refused_for_every_token` pins it. That test passed before the docs change too.

**Still a decision:** enabling it means adding `"agent_kick" => acting(args, "agent")` to `remote::authorize` with a token test. It grants a privilege, so it's beyond a hardening fix. Until then, the token branch in `kick_agent_as` can't be reached through a token.

## 9. Info: dedupe webhook retries by delivery id (docs)

Each attempt is signed with a new `X-Another-Timestamp`, so a receiver with a replay window still accepts a retry. That is intended. Receivers should dedupe on `X-Another-Delivery`. PUSH.md now says so.

---

## Checked and sound

**Signed webhooks:**
- HMAC-SHA256 over `"<timestamp>.<body>"`, with a 32-byte random `whsec_` secret per webhook, shown once.
- PUSH.md shows verification with `hmac.compare_digest` and a 5-minute window.
- No redirects (`max_redirects(0)`), a 10 s timeout, and `https`, or `http` to loopback only (for tokens, see finding 3), with no credentials in URLs.
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
