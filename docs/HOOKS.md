# Hook packs

Another is hooks-first (see [DECISIONS.md](DECISIONS.md)): agents learn about new mail because a client lifecycle hook injects a short digest, not because they poll or rely on MCP resource subscriptions. Every supported client gets the same hooks, calling the same wrapper script.

| Moment | What Another does | Output |
|---|---|---|
| Session start | Registers the agent if needed, records `session.started`, renews holds, records the terminal pane for wakes | Inbox + last-hour activity digest (≤ 1500 characters) |
| Each prompt | Heartbeat (renews holds, marks the agent active) | One line, only for mail not announced before; otherwise nothing |
| After each tool call (`post-tool`) | Checks for mail not announced yet; at most once a minute marks the agent active and renews holds past half their TTL | One line of context mid-task when there is new mail; otherwise nothing |
| Before a file edit (`pre-edit`) | Takes a narrow hold on each file about to change, or stops the edit if another agent holds it | Nothing when the edit may go ahead; a denial naming the holder when it may not |
| Session end | Records `session.ended`, releases the agent's holds, forgets the pane, marks it offline | Nothing |

The two tool hooks run inside a task, so mail sent to a busy agent reaches it at its next tool call, without waiting for the next prompt, and two agents editing the same file find out before the second one writes.

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
| Claude Code | `~/.claude/settings.json` | `SessionStart`, `UserPromptSubmit`, `SessionEnd`, `PostToolUse` (all tools), `PreToolUse` (`Edit\|Write\|MultiEdit\|NotebookEdit`) | Plain stdout is added to context at session start and prompts. Tool hooks answer in JSON: `additionalContext` after a tool, `permissionDecision: "deny"` before an edit. |
| Codex CLI | `~/.codex/hooks.json` | Same five; `PreToolUse` matches `apply_patch\|Edit\|Write` | New hooks run only after you trust them: open Codex and review them under `/hooks`. The edit guard reads the file names from the `apply_patch` patch text. |
| Grok CLI | `~/.grok/hooks/another.json` | Same five as Claude Code | User-level hooks need no trust step (`/hooks-trust` applies to project hooks). Grok maps `Edit`, `Write` and `MultiEdit` onto its own edit tools. See "Grok and shared hook files" below. |
| Cursor (best effort) | `~/.cursor/hooks.json` | `sessionStart`, `beforeSubmitPrompt`, `sessionEnd`, `postToolUse` | Context is injected at session start and after tool calls (`additional_context`); the prompt hook just renews holds and lets the prompt through. No edit guard: Cursor blocks a tool call when a `preToolUse` reply is not valid JSON, and a hook that fails open prints nothing. |

Edits made through a shell command (`sed -i`, `echo >`, a script) are not seen by any client's edit hook, so they are neither held nor stopped. Neither is an edit through a hard link to a held file, since a path cannot reveal one.

## Mid-task notices

`post-tool` runs after every tool call. It reads the database without writing, compares the agent's unread mail with what was already announced (the same once-per-message rule as prompt notices, D6), and prints one line only when there is something new:

```text
Another: 1 new message(s) for claude@shop (1 urgent): "Release src/api?" from codex@shop-review (opens request 01J...). Read them with fetch_inbox (or `another inbox --agent claude@shop --unread`).
```

Each message is announced once, by whichever channel claims it first: a mid-task notice, a prompt notice, or a daemon wake (see [DAEMON.md](DAEMON.md)). The claim is a row per agent and message in the database, written in one transaction with the `push.notified` evidence, so hooks running in parallel and the daemon never announce the same message twice. A large burst is announced in full (up to 1000 messages per notice, the rest on the next call); the line lists three subjects. At most once a minute the hook also marks the agent active and renews its holds that are past half their TTL, so a long task without prompts keeps its holds and shows as live on the dashboard.

## The edit guard

`pre-edit` runs before each file edit. For every file the tool is about to change it resolves the path the way the filesystem does (symlinks are followed before any `..` after them; a new file is resolved through its existing parent directories; a dangling link is followed to the file it would create; when the file is itself a link, both the link and the file it points at are checked; on a volume that ignores letter case, a new file matches holds spelled in any case; see [HOLDS.md](HOLDS.md)), works out the file's project from that real path (the deeper of its git checkout and the closest registered project; files in neither, such as scratch space, are left alone), then in one transaction:

- if another agent holds an overlapping path, nothing is taken and the edit is denied with a message like this one:

```text
Another stopped this edit: src/api/routes.rs is held by codex@shop-review (exclusive hold on shop-review:src/api/**, "router refactor", until 2026-11-10T16:04:00.000Z). Leave it alone until the hold is released. Ask the holder with `another send --from claude@shop --to codex@shop-review --request --subject "Release src/api/routes.rs?"`, or work on other files and try again later.
```

- if the agent already holds the file (or a directory or glob covering it) exclusively, nothing is written, unless that hold is past half its TTL, which renews it;
- otherwise the agent gets a narrow exclusive hold on that one file, with the default 30 minute TTL and the reason "editing (taken by the edit hook)".

Holds are checked across every checkout of the same repository on this machine: sibling worktrees and separate clones of one origin see each other's holds (see [HOLDS.md](HOLDS.md)). A session's holds, including the ones the guard took, are released at session end.

The guard never prints "allow": that would skip the client's own permission prompts. It only ever adds a denial.

## Failing open

Both tool hooks open the database without creating or migrating it and wait at most 50 ms (`post-tool`) or 300 ms (`pre-edit`) for a lock. The rule:

- **Push fails open.** Missing database, old schema, locked database, unknown agent: the hook prints nothing and exits 0.
- **The edit guard denies only a known conflict.** If it cannot check (the database is missing or locked), it lets the edit through and appends a line to `$ANOTHER_HOME/logs/hooks.log` saying why. That log is capped at 256 KB (one older copy is kept as `hooks.log.1`).

With nothing new, `post-tool` only reads, and SQLite in WAL mode lets readers past a writer. Announcing needs the claim write: if another writer holds the database for longer than 50 ms, the notice is not shown on that call and arrives on the next one.

The budget is about 100 ms per tool call. Measured on a laptop over 50 runs each, both hooks took about 13 to 16 ms at the 95th percentile, with or without new mail and with the daemon up or down; with the database locked outright they gave up in about 75 ms (`post-tool`) and 350 ms (`pre-edit`). The pull request that added them has the method.

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
- `post-tool` and `pre-edit` also read `tool_name` / `toolName` and `tool_input` / `toolInput`. The edit guard takes file names from `file_path`, `filePath`, `notebook_path`, `path` or `target_notebook`, and from the `*** Add File:`, `*** Update File:`, `*** Delete File:` and `*** Move to:` lines of a patch in `command`.
- Exit status is always 0; on any error nothing is printed.
- Scripts that prefer explicit calls can use `another --json heartbeat --agent NAME --start|--end`, `another --json inbox --agent NAME --unread`, and `another --json activity --summary`.
