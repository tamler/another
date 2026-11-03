# Hook packs

Another is hooks-first (see [DECISIONS.md](DECISIONS.md)): agents learn about new mail because a client lifecycle hook injects a short digest, not because they poll or rely on MCP resource subscriptions. Every supported client gets the same three hooks, calling the same wrapper script.

| Moment | What Another does | Output |
|---|---|---|
| Session start | Registers the agent if needed, records `session.started`, renews holds | Inbox + last-hour activity digest (≤ 1500 characters) |
| Each prompt | Heartbeat (renews holds, marks the agent active) | One line, only for mail not announced before; otherwise nothing |
| Session end | Records `session.ended`, releases the agent's holds, marks it offline | Nothing |

## Install

```bash
another hook install --client claude --dry-run
```

```bash
another hook install --client claude
```

`--client` takes `claude`, `codex`, `grok`, `cursor`, or `all` (repeat or comma-separate). The installer:

- writes one POSIX wrapper, `$ANOTHER_HOME/hooks/another-hook.sh` (default `~/.another/hooks/another-hook.sh`), which calls `another hook run <event> --client <client>` and always exits 0;
- adds Another entries to each client's **user-level** config in your home directory (`~/.claude/settings.json`, `~/.codex/hooks.json`, `~/.grok/hooks/another.json`, `~/.cursor/hooks.json`). Hooks are global for that user, not per project; the project is worked out at run time from the client's working directory. Existing hooks and settings stay in place;
- keeps each file's permissions (and owner, where allowed) when rewriting it, so a `0600` settings file stays `0600`; files it creates are `0600`. A symlinked config (e.g. from a dotfiles repo) is edited at its target and stays a symlink;
- is idempotent: running it again changes nothing;
- copies any file it changes to `<file>.another-backup-<timestamp>` first;
- refuses to touch a file that is not valid JSON (or, for Grok, an `another.json` it did not write) and then writes nothing at all.

Check what is installed, or remove it:

```bash
another hook status
```

```bash
another hook uninstall --client all
```

Uninstall removes only entries that reference `another-hook.sh`, and deletes the wrapper once no client uses it. Both install and uninstall also clean up entries left by development builds from before the project was renamed (they referenced `wyres-hook.sh`, and Grok's file was `~/.grok/hooks/wyres.json`): install replaces them with current entries, and uninstall removes them.

## Per client

| Client | Config written | Events | Notes |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd` | Plain stdout is added to context. |
| Codex CLI | `~/.codex/hooks.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd` | New hooks run only after you trust them: open Codex and review them under `/hooks`. |
| Grok CLI | `~/.grok/hooks/another.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd` | User-level hooks need no trust step (`/hooks-trust` applies to project hooks). See "Grok and shared hook files" below. |
| Cursor (best effort) | `~/.cursor/hooks.json` | `sessionStart`, `beforeSubmitPrompt`, `sessionEnd` | Context is injected only at session start (`additional_context`); the prompt hook just renews holds and lets the prompt through. |

### Grok and shared hook files

Grok also executes hooks from Claude Code and Cursor config files. To keep one Grok event from running Another twice, an Another hook invoked with `GROK_HOOK_EVENT` set acts as `grok` and stands down when the Grok pack is installed. Without the Grok pack, Grok still gets Another through the Claude or Cursor entries, as `grok@<project>`.

## Identity

By default a hook acts as `<client>@<project>`, where the project is the nearest directory containing `.git` above the client's working directory (or that directory itself). The agent is registered on first use with the matching program. To use a different identity, set `ANOTHER_AGENT` (for example `ANOTHER_AGENT=reviewer@shop`) in the environment the client runs hooks with; that agent must already be registered.

## The digest

Example session-start digest:

```text
Another: claude@shop has 2 unread (1 urgent), 1 awaiting ack, 1 open request(s).
- [urgent] from codex@shop: "Rebase please" (message 01J...)
- from grok@shop: "Docs updated" (message 01J...)
- request 01J... "Fix flaky test" from codex@shop (open)
Last hour: 7 event(s) since 2026-10-28T08:00:00.000Z; active agents: codex@shop; requests: 1 open, 0 claimed, 0 stale; 1 live hold(s).
- codex@shop holds shop:src/api/** (exclusive)
The Another MCP tools fetch_inbox, acknowledge and claim_request (or `another inbox --agent claude@shop`) show details.
```

Message bodies are never included in digests or the activity feed.

## Contract (for other clients)

Any client that can run a command at session start / prompt / end can use Another:

```bash
another hook run session-start --client claude < event.json
```

- stdin may carry the client's JSON event; Another reads `cwd` (or `workspaceRoot` / `workspace_roots[0]`) and `session_id` (or `sessionId` / `conversation_id`) when present.
- Exit status is always 0; on any error nothing is printed.
- Scripts that prefer explicit calls can use `another --json heartbeat --agent NAME --start|--end`, `another --json inbox --agent NAME --unread`, and `another --json activity --summary`.
