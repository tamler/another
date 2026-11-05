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

## Optional: lane reports at the end of each turn

`another hook install --client <c> --lane-reports` also installs a **Stop** hook (`stop` for Cursor). It posts an automatic report into the agent's lane: `$ANOTHER_LANE`, or the single open lane the agent leads in that project. Reports are debounced to one every 15 minutes. Installing again without the flag removes the Stop hook. See [LANES.md](LANES.md).

> **Warning: the Stop hook stores what your agent last said.** The report summary is the client's last reply, truncated to 300 characters, and it is stored in the lane and mailed to the lane's counterpart (who may be a remote manager bot). Before anything is stored or mailed, the hook redacts likely secrets, including ones next to punctuation (a full stop, comma, bracket, quote, or a `-`, `_`, `/` or `+` hugging them): `ano_`, `sk-`, `ghp_`/`github_pat_`, `glpat-`, Slack `xox*-`, `hf_`, AWS `AKIA…`, `whsec_` tokens, and long hex or base64 runs. This is a best-effort filter, not a guarantee. Don't enable `--lane-reports` for agents that routinely print credentials, and keep secrets out of agent transcripts.

## Optional: hold files before they are edited

`another hook install --client claude,codex --pre-edit` adds a **PreToolUse** hook:
- Claude Code: matcher `Edit|Write|MultiEdit|NotebookEdit`.
- Codex: matcher `apply_patch`. File names come from the patch's `*** Add/Update/Delete File:` and `*** Move to:` header lines (at the start of a line; an indented context line that only looks like one is ignored), wherever Codex puts the patch (`tool_input.patch`, `tool_input.command` or a plain string).

**Before each edit**, the hook takes an exclusive hold on the files, with reason `pre-edit`:
- The hold is for the agent's lane: `$ANOTHER_LANE`, else the one lane it leads there. See [lane-aware holds](LANES.md#path-holds-and-lanes).
- **If another agent or lane holds a file, the edit is denied** with the standard `permissionDecision: "deny"` output. The reason names the holder, their lane and when the hold expires, so the agent can coordinate or move on.
- Holds by agents in the same lane never block, unless both holds are exclusive (the hook's are).
- Files outside the project are ignored, including a symlink that points out of it. A symlink inside the project is held under both its own name and its target's. File names are held literally: glob characters in a name are escaped (`notes[.md` is held as `notes[[].md`), so they never break the hold.

**Otherwise** the hook prints nothing, and the client's normal permission flow applies. The hook never approves anything.

**Codex: trust the hooks, and mind `codex exec`.** Codex runs a hook it doesn't manage only after you trust it, and trust is tied to the hook's hash, so it is needed again after reinstalling. To trust them, open Codex **interactively** and review them under `/hooks`.
- `codex exec` (non-interactive) can't ask, so untrusted hooks are skipped silently and **edits are not held or blocked**. This was checked on Codex 0.166.0: `another hook run pre-edit` denied the edit correctly, but under `codex exec` the hook never ran and a patch went through over a live hold.
- Trusting the hooks once in an interactive session in the same environment makes `codex exec` run them too.
- For one-off automation whose hook sources you already vet, `codex exec --dangerously-bypass-hook-trust …` runs enabled hooks without stored trust.
- Otherwise, **hold enforcement under `codex exec` isn't guaranteed**. Have such runs take their holds explicitly (`another hold acquire`), or check `another holds` first.

**Lifetime:** holds taken this way expire after 30 minutes unless renewed. The prompt hook's heartbeat renews them, and session end releases them.

**Failing open, visibly:** if `$ANOTHER_LANE` names a closed lane or one from another project, the files are still held (without a lane), and the hook shows a warning (`systemMessage`, also on stderr). If Another itself is unavailable, the hook prints nothing and the edit goes ahead.

**Lane-mates:** two agents in the same lane can't both hold the same file for editing. The pre-edit hook takes exclusive holds, and exclusive holds block each other even inside a lane.

Installing without the flag removes the hook. Your own `PreToolUse` entries are never touched. Grok and Cursor have no equivalent pre-tool hook.

## Per client

| Client | Config written | Events | Notes |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd` | Plain stdout is added to context. |
| Codex CLI | `~/.codex/hooks.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd` | New hooks run only after you trust them: open Codex interactively and review them under `/hooks`. Untrusted hooks are skipped by `codex exec` (see [above](#optional-hold-files-before-they-are-edited)). |
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
