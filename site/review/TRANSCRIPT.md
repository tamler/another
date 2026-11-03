# Transcript provenance

Captured from the release binary built from source commit
`60617866de8f42e2a02fc50f13a919abac23f328` (`another 0.1.0`). The capture is fixture
data, not customer usage. Product source was unchanged during site work.

## Setup and substitutions

- A fresh temporary Another home and a project directory named `shop` were used.
- The capture invoked `target/release/another` explicitly after `cargo build --release`.
- Explicit `--home` selected the temporary node for the final captures; `HOME`
  was never changed.
- The handoff fixture used `/tmp/shop`. Agent registration, send, Bob's inbox,
  reply, acknowledgement and activity ran continuously on one fresh node.
  Output is verbatim, including IDs, timestamps, path and the eight-event count.
  The activity CLI's summary displays seven highlights for those eight events.
  Shell commands wrap visually at whitespace. The homepage shows only the inbox
  summary line and explicitly labels that excerpt; the full, unwrapped columnar
  inbox and activity output are in Docs. No output has been rewritten to fit.
- A separate setup node was used for wake, token and server examples. Wake setup
  used `bob@shop`, adapter `tmux`, target `%3`. Only `--dry-run` was
  invoked; no agent terminal or adapter process was nudged. The dry-run output
  is transcribed in full in Docs. All operational recordings are visible without
  opening a disclosure.
- Hook install used `--client claude,codex,grok --dry-run`. Docs displays only
  its first output line, explicitly labeled an excerpt. Source paths for the
  hook installer and wake dry-run were inspected; real client config hashes and
  mtimes were unchanged across the hook capture. No global hooks were installed.
- `token create --agent bob@shop --project shop --expires 1h` ran on the setup node.
  Its example appears in the remote section, separate from the handoff.
  It ran in fixture state. Its secret was held only in memory and replaced with `[secret redacted]`
  before any output or file write. The scoped token authenticated to a fixture
  loopback `/activity` endpoint with HTTP 200, then was revoked. No secret was
  published. The non-secret token ID and the rest of its output are verbatim.
- `serve --auth --bind 127.0.0.1:17466` ran against the same fixture node. Its
  startup line is verbatim. Only the owned fixture process was stopped. Commands
  were captured across separate shell sessions.
- The setup node's activity capture is not displayed. No setup mutation is
  represented as part of the continuous handoff. Grouping follows the relevant
  setup and remote instructions.

## Source map

| Site content | Repository authority |
|---|---|
| Definition, tagline, install, init, stdio MCP JSON, CLI and free/open positioning | `README.md` |
| Hook flow, client config paths, user-global scope, trust and identity | `docs/HOOKS.md` |
| Wake targets, dry-run, no-work skip and optional adapters | `docs/WAKE.md` |
| herdr install, actions/pane and `another.inbox` identity | `docs/WAKE.md`, `integrations/herdr/README.md`, `integrations/herdr/herdr-plugin.toml` |
| Resource leases, orphan candidates, human-approved owning-node reap | `docs/RESOURCES.md`, `README.md` |
| Remote tokens, auth on loopback, TLS/tunnel and allowed hosts | `docs/REMOTE.md`, actual token creation and authenticated serve fixture |
| Federation, signed logs, cross-node mail, node-local requests | `docs/FEDERATION.md`, `docs/DECISIONS.md` |
| License | `LICENSE` |

The homepage install command text matches README exactly; it wraps visually at whitespace.
a global install was not performed. First-run registration, send, inbox, activity and wake were checked in
fixture state, including the docs' bare-name exchange in a separate node.
Hook changes were dry-run only. The JSON server definition comes from README;
the page explicitly notes that client configuration formats vary. No live MCP
client setup is claimed. The third-party herdr install command was checked
against repository documentation; no plugin was installed into a live workspace.
The site makes no claim that prebuilt releases are currently published.
