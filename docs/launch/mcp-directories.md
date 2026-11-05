# MCP directories — drafts only

Nothing here has been submitted, and no account or package was created. Rules were read 2026-11-05; recheck them on the day. Timing: after the install path a directory visitor would follow works, and after [ORDER.md](ORDER.md) step 1. [awesome-lists.md](awesome-lists.md) covers the GitHub lists.

## Shared copy

**Name:** Another

**Short description (under 160 characters):** Local-first inbox, activity feed and resource registry for coding agents. MCP tools, hooks and a Rust CLI. Apache-2.0.

**Longer description:** Another lets independently run coding agents exchange mail, claim requests, signal advisory path holds and borrow registered resources. State is local SQLite, with no signup or product telemetry. It does not start or manage agents. Remote peers and federation are optional.

**Links:** repository `https://github.com/tamler/another`; documentation `https://toanother.one/docs/`; MCP setup `https://toanother.one/guides/mcp-agent-messaging/` (this route returns 404 until the guides are deployed, so use `https://toanother.one/docs/getting-started/` until then).

**Client configuration:**

```json
{
  "mcpServers": {
    "another": {
      "command": "another",
      "args": ["serve", "--stdio"]
    }
  }
}
```

## Official MCP Registry

**Status: not ready. Another has no package the registry can point to.**

- The [registry README](https://github.com/modelcontextprotocol/registry) describes a preview, warns that breaking changes or data resets may occur, and says publishing uses the `mcp-publisher` CLI from the server's own project. A pull request to the registry repository does not list a server.
- The [package types page](https://raw.githubusercontent.com/modelcontextprotocol/registry/main/docs/modelcontextprotocol-io/package-types.mdx) covers npm, PyPI, NuGet, Cargo (crates.io only), Docker/OCI and MCPB. It describes no source-only option.
- Publishing under `io.github.tamler/...` needs a login as that GitHub user or a GitHub Action from its repositories. A custom namespace needs a DNS or HTTP challenge, and DNS changes need separate authorization.

**Route to decide (owner):**

| Route | Needs | Note |
|---|---|---|
| Cargo | Publish to crates.io; a visible `mcp-name:` token in the README | The crate name `another` was unclaimed on 2026-11-05. `crates.io` publishing needs an account and its own decision |
| OCI | Publish an image with the `io.modelcontextprotocol.server.name` annotation | No image exists today |
| MCPB | A release artifact with "mcp" in its URL and a `fileSha256` | Needs a release; no MCPB bundle exists today |

Do not write `server.json` by hand from this note. Generate it with the current `mcp-publisher`, once a route is chosen.

## mcpservers.org

**Status: ready to submit once the shared copy is approved.** [Form](https://mcpservers.org/submit) fields, read 2026-11-05:

| Field | Draft |
|---|---|
| Server Name | Another |
| Category | Communication (Development also fits; the human chooses) |
| Short Description | Shared copy above |
| Repository, Website or Documentation | `https://github.com/tamler/another` |
| Official MCP Registry Name | Leave empty; none exists |
| Supports remote connections | Leave unchecked. The listed setup is local stdio. Authenticated HTTP exists, but each user hosts it |
| Contact Email | **Owner decision.** Use an address that exposes no personal name |
| Submission plan | **Free** ($0, review within 2 weeks) |

Premium Submit costs $39. It buys faster review, a badge, search priority and a dofollow link. It is spending, is not authorized, and is not recommended here.

## Glama

**Status: process unverified.** The [server list](https://glama.ai/mcp/servers) shows an "Add Server" button but gives no steps, requirements or fees. Read the form before filling it in. The shared copy applies if the fields match. Do not assume it is free.
