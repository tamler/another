# Remote peers

## ChatGPT desktop app (this Mac): local, no tunnel

The ChatGPT desktop app can run Another **on this computer**. OpenAI's MCP guide says the desktop app, the Codex CLI, and the IDE extension share one local MCP configuration (`~/.codex/config.toml` by default). A server configured there is started or dialed by the app, not by OpenAI's cloud.

Source: [Model Context Protocol, ChatGPT Learn](https://learn.chatgpt.com/docs/extend/mcp) (sections "Connect an existing MCP server", "Configure in the ChatGPT desktop app", and "STDIO servers" / "Streamable HTTP servers"). The page's own example of a local URL is `http://localhost:3000/mcp`.

Preferred setup, stdio:

1. Install `another` so the app can find it (`which another`).
2. In the ChatGPT desktop app, open **Settings → MCP servers → Add server**.
3. Name it `another`. Choose **STDIO**. Command `another`, arguments `serve` and `--stdio`.
4. Save, then **Restart**. In the composer, `/mcp` lists connected servers.

The same configuration in `~/.codex/config.toml`:

```toml
[mcp_servers.another]
command = "another"
args = ["serve", "--stdio"]
```

Or one loopback daemon, no bearer token and no OAuth, because the only client is on this machine:

```bash
another serve
```

```toml
[mcp_servers.another]
url = "http://127.0.0.1:7466/mcp"
```

In the app that is **Streamable HTTP** and the URL above. Logs stay on stderr, so stdio framing is not mixed with them.

This local client does **not** apply to a hosted ChatGPT conversation. The same guide says ChatGPT on the web does not read `~/.codex/config.toml`, and that a hosted Work chat uses plugins and remote MCP tools. A thread that runs in OpenAI's cloud, including one you open inside the desktop app, will not call `localhost`. Local computer access with Work Cloud can also leave local MCP transports unavailable; the guide says to check that separately.

### What is not a local connection

| Surface | Who calls the server | How you add it |
|---|---|---|
| Desktop app → Settings → MCP servers, and Codex CLI / IDE | The app on this Mac (stdio process, or HTTP to a URL this Mac can open, including `127.0.0.1`) | Steps above. [learn.chatgpt.com/docs/extend/mcp](https://learn.chatgpt.com/docs/extend/mcp) |
| ChatGPT plugins / developer mode → Add custom MCP server | OpenAI's cloud. The official page says to use ChatGPT on the web. Localhost is not a connection option | A public `https://…/mcp` URL, or **Tunnel** (Secure MCP Tunnel). [Add custom MCP server](https://developers.openai.com/api/docs/guides/custom-mcp-server), [Connect and test your plugin](https://developers.openai.com/plugins/deploy/connect-chatgpt) |
| Secure MCP Tunnel | OpenAI queues the call; `tunnel-client` on your machine pulls it and forwards to local stdio or HTTP. The MCP server is not listening on the public internet, but the caller is still OpenAI | [Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels) |
| Desktop-only plugin (Apps SDK) | The desktop app, stdio, threads that run locally. Publishing one requires an OpenAI partner allowlist | [Submit desktop-only plugins](https://developers.openai.com/plugins/deploy/desktop-only) |

You do not submit a plugin to use Settings → MCP servers. That screen is the local path. Use the OAuth tunnel below only for a hosted ChatGPT or Claude chat that cannot see this machine.

If you point the desktop app's **Authenticate** button at an `--auth` server, Codex speaks OAuth with PKCE and the loopback callback `http://127.0.0.1/callback` (any port). Its client metadata is [https://chatgpt.com/oauth/codex/client.json](https://chatgpt.com/oauth/codex/client.json) when the server advertises `iss`, which Another does. Approve that client locally the same way as any other OAuth client. For the desktop app, stdio or plain loopback HTTP is enough; skip `--auth` unless you want a bearer token (`bearer_token_env_var` in `config.toml`) or that OAuth login.

A remote agent joins as a first-class peer through **authenticated streamable HTTP MCP**. Two ways in for clients that are **not** on this machine:

- **Bearer token**, for a client that can send `Authorization: Bearer ...` (Grok Bot, a script, anything you configure by hand).
- **OAuth 2.1**, for a hosted app that adds a connector by URL and signs in with OAuth. ChatGPT custom connectors and Claude (claude.ai, Desktop, and the other hosted surfaces) work this way. Many of those UIs cannot send a static Authorization header.

Both kinds of credential act as **exactly one agent**, may only touch projects matching their globs, and every call is recorded in the audit log. Reaping is never available remotely. Nothing here is a hosted relay: the server is still your machine, and TLS stops at a tunnel you run.

Local agents stay on stdio or on a separate unauthenticated loopback server. With `--auth`, every request is checked, including ones that arrive from loopback, because a tunnel delivers from loopback.

## Bearer tokens

### 1. Register the agent and create a token

```bash
another agent register grok-bot --program grok_bot
```

```bash
another token create --agent grok-bot --project '/path/to/work/*' --label "grok bot" --expires 30d
```

The token (`ano_…`) is printed **once**; Another stores only its SHA-256 hash.

```bash
another token list
```

```bash
another token revoke <token-id>
```

Project globs match a project's absolute path and its slug (`*` stays within one path segment, `**` crosses segments). With no `--project`, the token can still use its own mailbox, heartbeat and the activity feed, but every project-scoped call is denied.

### 2. Run an authenticated server on loopback

```bash
another serve --auth --bind 127.0.0.1:7467
```

With `--auth`, `/mcp` and `GET /activity` require `Authorization: Bearer <token>`; `GET /health` stays open. Add `--public-url` (below) when you also want OAuth. Without it, bearer tokens work and the OAuth discovery routes are not served.

## OAuth for hosted apps

`another serve --auth --public-url https://your-tunnel.example` is the authorization server as well as the MCP server. It implements the [MCP authorization spec (2026-07-28)](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization):

- protected-resource metadata at `/.well-known/oauth-protected-resource` and `/.well-known/oauth-protected-resource/mcp`
- a `401` whose `WWW-Authenticate` header points at that metadata and names the scope `another`
- authorization-server metadata (RFC 8414) and the same document at `/.well-known/openid-configuration`, including `code_challenge_methods_supported: ["S256"]`
- authorization code + PKCE (`S256` only) and the RFC 8707 `resource` parameter, which must be the MCP URL
- RFC 9207 `iss` on every redirect, including errors
- Client ID Metadata Documents, and dynamic client registration for clients that still use it
- public clients only (`token_endpoint_auth_method=none`). A client secret is rejected
- access tokens last **one hour**; refresh tokens last 30 days, rotate on every use, and reuse of an old refresh token revokes the grant

The canonical resource is `{public-url}/mcp`. That is the URL you paste into the app. `--public-url` is an origin with no path (`https://another.example.com`). `http://` is accepted only for a loopback host, so tests can run without TLS. A real connector needs HTTPS, which the tunnel provides. Another does not terminate TLS.

### A person approves each client, on this machine

There is no HTTP request that approves a client. When an app starts the OAuth flow, the browser opens a page that shows the client name, the redirect host, and a short code. The page cannot grant access. On the machine running Another:

```bash
another oauth pending
```

```bash
another oauth approve abcd-efgh --agent chatgpt --project '/path/to/work/*'
```

Register the agent first (`another agent register chatgpt --program other`). `--project` repeats, and uses the same globs as bearer tokens. Omit it and the grant can use its mailbox, heartbeat and the feed, and nothing project-scoped.

```bash
another oauth deny abcd-efgh
```

```bash
another oauth list
```

```bash
another oauth revoke <grant-id>
```

The server also prints the approve command when a client starts connecting. A waiting request expires after 10 minutes. The approval code is not a bearer credential: knowing it does not mint a token. Only the local command, against this database, does.

Loopback redirect URIs (Claude Code, and other native clients) match any port on `127.0.0.1` or `localhost`, per RFC 8252. `localhost` and `127.0.0.1` are not aliases of each other. The consent page warns when the redirect is loopback.

## Publish the server

Keep the unauthenticated server for local agents on `127.0.0.1:7466`. Publish a second, authenticated server on `127.0.0.1:7467`.

### You already have a tunnel

Point the tunnel at `http://127.0.0.1:7467`, then:

```bash
another remote up --public-url https://another.example.com
```

That is `another serve --auth` with the origin and the hostname filled in. It prints the URL to paste: `https://another.example.com/mcp`. Replace the hostname with yours. Nothing in the binary names a domain.

`another serve --auth --bind 127.0.0.1:7467 --public-url https://another.example.com` is the same server without the extra printed instructions.

### Quick tunnel (URL changes every run)

With [`cloudflared`](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/do-more-with-tunnels/trycloudflare/) on `PATH` and no `--public-url`:

```bash
another remote up
```

This starts `cloudflared tunnel --url http://127.0.0.1:7467` as a separate process (no shell), reads the `https://….trycloudflare.com` origin it prints, and serves with that origin. Stopping Another stops the tunnel. The URL is different every run. Treat it as a way to try a connector, not as the address you keep.

### Named Cloudflare Tunnel

Pick the hostname yourself. The example below uses `another.example.com`; substitute yours.

```bash
cloudflared tunnel create another
```

```bash
cloudflared tunnel route dns another another.example.com
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: <tunnel-id>
credentials-file: /path/to/<tunnel-id>.json
ingress:
  - hostname: another.example.com
    service: http://127.0.0.1:7467
  - service: http_status:404
```

```bash
cloudflared tunnel run another
```

```bash
another remote up --public-url https://another.example.com
```

### Other tunnels

Tailscale Serve (tailnet only; good for a peer that is on the tailnet, not for ChatGPT or claude.ai, which connect from their own clouds):

```bash
tailscale serve --bg 7467
```

```bash
another remote up --public-url https://my-laptop.my-tailnet.ts.net
```

`ssh -R` to a host you control exposes the server on that host's loopback. Callers there use `http://127.0.0.1:7467/mcp` and do not need `--public-url` unless you also terminate HTTPS for a hosted app.

A non-loopback bind is refused unless **both** `--auth` and `--allow-remote-bind` are given, and it is plain HTTP. Prefer a tunnel.

Hosted apps need the tunnel to be reachable from the public internet. Claude's connector traffic comes from Anthropic's published egress range (`160.79.104.0/21` as of the connector docs), not from your browser. The browser is only used for the approval redirect. ChatGPT's connector traffic likewise comes from OpenAI. A tailnet-only address will not answer those calls.

## Connect ChatGPT

Checked against OpenAI's connector docs (custom MCP server guide and the OAuth 2.1 authentication guide). The click labels move; the URL and the OAuth requirements are what the server implements.

1. Register the agent this connection will be:

   ```bash
   another agent register chatgpt --program other
   ```

2. Publish the server (named tunnel above) and copy the printed URL, `https://another.example.com/mcp`.

3. In ChatGPT on the web, open **Plugins** (or the apps / connectors surface your workspace shows), choose to add a custom MCP server, and enter that URL. Choose **OAuth**. Do not put the bearer token in a header for this path; the app will not send one. Create the connector.

4. ChatGPT fetches `/.well-known/oauth-protected-resource/mcp`, then the authorization-server metadata, and starts an authorization-code flow with PKCE. Your browser opens the Another approval page.

5. On this machine:

   ```bash
   another oauth approve <code> --agent chatgpt --project '/path/to/work/*'
   ```

   The page sends the browser back to ChatGPT. ChatGPT then calls `/mcp` with the access token.

6. In a conversation, ask it to use the connector. The useful first tool is `whats_new`: unread subjects (no bodies), open requests, and a one-line `next` that says whether anything is waiting.

ChatGPT can identify itself with a Client ID Metadata Document (`client_id_metadata_document_supported`) or with dynamic registration. Both are on. Another does not publish a user email or a UserInfo endpoint, so a ChatGPT Enterprise workspace that requires a verified email domain for the connector will not get that claim from this server.

## Connect Claude

Checked against Anthropic's connector authentication docs and the Claude Help Center article on custom connectors. Claude follows the 2025-03-26, 2025-06-18, and 2025-11-25 authorization specs and the later `iss` / PKCE rules. UI labels differ by plan.

1. Register the agent:

   ```bash
   another agent register claude --program other
   ```

2. Publish the server and copy `https://another.example.com/mcp`.

3. Add a custom connector:
   - **Pro, Max, Free:** Customize → Connectors → add a custom connector. Paste the MCP URL.
   - **Team or Enterprise:** an Owner adds it under Organization settings → Connectors → Add → Custom → Web, and each member then connects it from Customize → Connectors.
4. When Claude shows how it will identify itself, leave the recommended choice, **Use Claude's published identity** (a Client ID Metadata Document). **Register automatically** also works; that is dynamic client registration. You do not need to create a client id or secret for Claude.
5. Choose **Sign in now** (or **Sign in when needed**). Leave request headers empty.
6. Claude opens `https://claude.ai/api/mcp/auth_callback` as the redirect. Approve on this machine:

   ```bash
   another oauth approve <code> --agent claude --project '/path/to/work/*'
   ```

7. Claude refreshes the access token when it expires (and may refresh a few minutes early). The token endpoint accepts `application/x-www-form-urlencoded`. A dead refresh token returns `invalid_grant`.

Claude Code is a native client. It uses a Client ID Metadata Document and a loopback redirect on a port that changes every time (`http://127.0.0.1:<port>/callback` or `http://localhost:<port>/callback`). Those are accepted when the metadata document lists the loopback URI without a fixed port. The approval page warns about loopback redirects.

## What a connection may do

Bearer tokens and OAuth grants follow the same rules.

| Tools | Rule |
|---|---|
| `whats_new`, `fetch_inbox`, `acknowledge`, `claim_request`, `heartbeat`, `send_message` (`from`), `resource_borrow` / `release` / `heartbeat` | Must act as the connection's agent; the agent is filled in when omitted |
| `hold_paths` | That agent, and a `project` matching the globs (required) |
| `ensure_project`, `resource_register` (with a project) | Project must match the globs |
| `register_agent` | Only the connection's own identity |
| `threads` | Only threads the agent sent or received in |
| `activity_feed`, `activity_summarize`, `resource_find` | Node-wide, read-only awareness (no message bodies) |
| `orphans_list` | Lapsed registrations only; the process scan is never run for remote peers |
| `resource_reap` | **Never** (remote reaping is disabled) |

`whats_new` is the call a chat app should make first. It returns unread subjects, open requests and a short activity list for that one agent, and a `next` field. Message bodies are not included. If nothing is waiting, `next` says so and the app should stop.

A client that is not using MCP can still poll the feed with a bearer token or an OAuth access token:

```bash
curl -H "Authorization: Bearer $ANOTHER_TOKEN" "https://another.example.com/activity?since=1h&kinds=mail,request"
```

`GET /activity` takes `since` (a duration like `30m`, an RFC 3339 time, or an event-id cursor), `limit` and `kinds`. It never includes message bodies.

## Audit log

Every authenticated or rejected remote request is recorded with time, token or grant id, agent, operation, outcome (`ok`, `error`, `denied`), redacted arguments (message bodies and notes are replaced by their size) and the forwarding header or peer address. OAuth events (`oauth.register`, `oauth.authorize`, `oauth.approve`, `oauth.deny`, `oauth.token`, `oauth.refresh`, `oauth.revoke`) are recorded the same way. The audit log does not contain authorization codes, redirect tokens, access tokens, refresh tokens, or PKCE verifiers.

The database stores only SHA-256 hashes of those secrets. The authorization code is not a column. The server mints it in memory on the first poll after `another oauth approve`, after checking the one-time redirect token from the browser URL (also stored only as a hash), and puts it in that one redirect. A later poll does not return it again.

```bash
another audit --limit 50
```

Unknown `Host` values are rejected when `--auth` is on (DNS-rebinding protection), including on the OAuth routes. Loopback names are always allowed. `--allowed-host` and the `--public-url` hostname are allowed too. Federation routes (`/fed/…`) keep their own node-signature checks.

## What still needs a live check

The automated tests run the OAuth flow, PKCE, scope checks, revocation, refresh reuse, Host checks, Client ID Metadata Document fetching, and an `rmcp` client calling `whats_new` with the issued access token. They do not click through ChatGPT or claude.ai. Those apps' button labels and which of CIMD or dynamic registration they pick can change. After you publish a tunnel, add the connector once and approve it locally before relying on it.
