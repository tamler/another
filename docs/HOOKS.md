# Hook packs

Another is hooks-first (see [DECISIONS.md](DECISIONS.md)): agents learn about new mail because a client lifecycle hook injects a short digest, not because they poll or rely on MCP resource subscriptions. Every supported client gets the same hooks, calling the same wrapper script.

| Moment | What Another does | Output |
|---|---|---|
| Session start | Registers the session actor if needed, with no hold, presence or wake-target lifecycle changes | Inbox + last-hour activity digest (≤ 1500 characters) |
| Each prompt | Checks for unannounced mail without a heartbeat | One line, only for mail not announced before; otherwise nothing |
| After each tool call (`post-tool`) | Checks for mail not announced yet, without renewing holds | One line of context mid-task when there is new mail; otherwise nothing |
| Before a file edit (`pre-edit`) | Takes a narrow hold on each file about to change, or stops the edit if another agent holds it | Nothing when the edit may go ahead; a denial naming the holder when it may not |
| Session end | Does not release holds, change presence or clear wake targets without an incarnation fence | Nothing |

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
| Codex CLI | `~/.codex/hooks.json` | Same five; `PreToolUse` matches `apply_patch\|Edit\|Write` | In the default trust flow, new hooks run only after you trust them: open Codex and review them under `/hooks` (see "Codex trust and coverage" below for the bypass flag). Each Another entry carries a `statusMessage` that says what it does, so it has a clear label and not only a bare command (see "Codex labels" below). The edit guard reads the file names from the `apply_patch` patch text. |
| Grok CLI | `~/.grok/hooks/another.json` | Same five as Claude Code | User-level hooks need no trust step (`/hooks-trust` applies to project hooks). Grok maps `Edit`, `Write` and `MultiEdit` onto its own edit tools. See "Grok and shared hook files" below. |
| Cursor (best effort) | `~/.cursor/hooks.json` | `sessionStart`, `beforeSubmitPrompt`, `sessionEnd`, `postToolUse` | Context is injected at session start and after tool calls (`additional_context`); the prompt hook lets the prompt through without renewing holds. No edit guard: Cursor blocks a tool call when a `preToolUse` reply is not valid JSON, and a hook that fails open prints nothing. |

### Codex trust and coverage

Installing the pack only configures hooks. In Codex's default trust flow, Codex skips new or changed non-managed hooks until you review and trust them yourself in `/hooks`, as described in the [official OpenAI hook documentation](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks). Until that step, automatic mail delivery and edit holds do not cover Codex sessions through these hooks. An update can require review again.

The same documentation describes `--dangerously-bypass-hook-trust`, which runs enabled hooks without persisted trust for that one invocation. A Codex session started with that flag executes Another's hooks before you have reviewed them, so it can deliver mail or block an edit on a held file even though trust was never granted. Another does not recommend the flag and never passes it for you; the warnings in this document and in `another hook status` describe the default flow only.

`another hook status` looks for Another's wrapper marker in client config. Its `installed` flag is configuration evidence only; the Codex `trust_note` in JSON and text says trust is not checked. Neither that flag, a passing test nor manually running `another hook run` proves that Codex executed a hook. Check `/hooks` yourself. While hooks are awaiting trust, use `another inbox`, `hold acquire` and `hold check` explicitly.

Another never grants hook trust or edits Codex's trust store. Keep that human decision in Codex.

### Codex labels

Codex shows a hook's `statusMessage` while the hook runs. Another sets one on each of its Codex entries, so every hook has a clear label and not only a bare command. Whether the `/hooks` review screen shows this text as well has not been checked against a live Codex. The labels are:

| Event | `statusMessage` |
|---|---|
| `SessionStart` | Another: registering this session |
| `UserPromptSubmit` | Another: checking for new messages |
| `PostToolUse` | Another: delivering messages mid-task |
| `PreToolUse` | Another: checking file holds before an edit |
| `SessionEnd` | Another: closing this session |

If `~/.codex/hooks.json` has no top-level `description` (or an empty one), install adds "Another coordination hooks (toanother.one): messages and file holds between coding agents". A description you wrote yourself is left as it is, and uninstall never removes one, since it may describe your other hooks too. Installs from before the labels existed get them the next time you run `another hook install --client codex`. Codex records trust against each hook's hash and skips a changed hook until you trust it again, so after that install open `/hooks` and trust Another's hooks again; until you do, mail and holds do not reach Codex sessions through hooks in the default trust flow. The other clients' files get neither field.

Edits made through a shell command (`sed -i`, `echo >`, a script) are not seen by any client's edit hook, so they are neither held nor stopped. Neither is an edit through a hard link to a held file, since a path cannot reveal one.

## Mid-task notices

`post-tool` runs after every tool call. It reads the database without writing, compares the agent's unread mail with what was already announced (the same once-per-message rule as prompt notices, D6), and prints one line only when there is something new:

```text
Another: 1 new message(s) for claude@shop (1 urgent): "Release src/api?" from codex@shop-review (opens request 01J...). Read them with fetch_inbox (or `another inbox --agent claude@shop --unread`).
```

Each message is announced once, by whichever channel claims it first: a mid-task notice, a prompt notice, or a daemon wake (see [DAEMON.md](DAEMON.md)). The claim is a row per agent and message in the database, written in one transaction with the `push.notified` evidence, so hooks running in parallel and the daemon never announce the same message twice. A large burst is announced in full (up to 1000 messages per notice, the rest on the next call); the line lists three subjects. Unfenced hook events do not renew holds or update presence.

## The edit guard

`pre-edit` runs before each file edit. For every file the tool is about to change it resolves the path the way the filesystem does (symlinks are followed before any `..` after them; a new file is resolved through its existing parent directories; a dangling link is followed to the file it would create; when the file is itself a link, both the link and the file it points at are checked; on a volume that ignores letter case, a new file matches holds spelled in any case; see [HOLDS.md](HOLDS.md)), works out the file's project from that real path (the deeper of its git checkout and the closest registered project; files in neither, such as scratch space, are left alone), then in one transaction:

- if another agent holds an overlapping path, nothing is taken and the edit is denied with a message like this one:

```text
Another stopped this edit: src/api/routes.rs is held by codex@shop-review (exclusive hold on shop-review:src/api/**, "router refactor", until 2026-11-10T16:04:00.000Z). Leave it alone until the hold is released. Ask the holder with `another send --from claude@shop --to codex@shop-review --request --subject "Release src/api/routes.rs?"`, or work on other files and try again later.
```

- if the agent already holds the file (or a directory or glob covering it) exclusively, nothing is written, unless that hold is past half its TTL, which renews it;
- otherwise the agent gets a narrow exclusive hold on that one file, with the default 30 minute TTL and the reason "editing (taken by the edit hook)".

Holds are checked across every checkout of the same repository on this machine: sibling worktrees and separate clones of one origin see each other's holds (see [HOLDS.md](HOLDS.md)). A hook session end cannot prove it belongs to the current incarnation, so it leaves holds in place. Inactive holds expire at their TTL or may be released explicitly by their exact actor.

The guard never prints "allow": that would skip the client's own permission prompts. It only ever adds a denial.

## Failing open

Both tool hooks open the database without creating or migrating it and wait at most 50 ms (`post-tool`) or 300 ms (`pre-edit`) for a lock. The rule:

- **Push fails open.** Missing database, old schema, locked database, unknown agent: the hook prints nothing and exits 0.
- **The edit guard denies a known conflict or an invalid session identity.** If the database is missing or locked it still lets the edit through and appends a line to `$ANOTHER_HOME/logs/hooks.log` saying why. That log is capped at 256 KB (one older copy is kept as `hooks.log.1`). The session-identity denial requires the client to invoke the hook and honor its denial response; that has not been established for every client or platform.

With nothing new, `post-tool` only reads, and SQLite in WAL mode lets readers past a writer. Announcing needs the claim write: if another writer holds the database for longer than 50 ms, the notice is not shown on that call and arrives on the next one.

The budget is about 100 ms per tool call. Measured on a laptop over 50 runs each, both hooks took about 13 to 16 ms at the 95th percentile, with or without new mail and with the daemon up or down; with the database locked outright they gave up in about 75 ms (`post-tool`) and 350 ms (`pre-edit`). The pull request that added them has the method.

### Grok and shared hook files

Grok also executes hooks from Claude Code and Cursor config files. To keep one Grok event from running Another twice, an Another hook invoked with `GROK_HOOK_EVENT` set acts as `grok` and stands down when the Grok pack is installed. Without the Grok pack, Grok still gets Another through the Claude or Cursor entries, as `grok@<project>`.

## Identity

Hooks derive a session-specific address from the effective client, project, base name and the client's session ID. The session-start digest gives the full address. It reads without sweeping, so request or hold counts can include expired items until a separate sweep. A missing or invalid ID cannot silently use a shared identity. For commands outside hooks, pass the full address explicitly or run `another session exec --agent <full-address> -- <program> [args...]` so child processes inherit `ANOTHER_AGENT` and `ANOTHER_AGENT_MODE=session`; a hook cannot set its parent shell's environment. For a custom hook base, set `ANOTHER_AGENT` and `ANOTHER_AGENT_MODE=base` before starting the client. If both variables are unset, the client name is the base. A missing, empty or invalid mode with an agent, or a mode without an agent, denies instead of guessing; update and restart older launchers. Session mode requires the full address and verifies the existing binding against the event's session ID. If a launcher drops both variables, it is indistinguishable from an unconfigured default, so verify propagation before relying on that launcher. The derived address is an identifier, not an authentication credential. Existing base-address mail and holds are not moved. A session's Herdr target must be explicitly set; its derived name is not a pane target. Actual client session ID and denial behavior must be verified before claiming coverage. Duplicate imported Grok admission handling remains a separate integration dependency.

New session events omit the raw client session ID and may include a session fingerprint. Historical signed events and replicated legacy events retain their original bytes: raw event listings, federation traffic and authorized database access can still reveal historical IDs. Activity and dashboard views redact those IDs. Upgrading requires a verified quiescent writer fence and a consistent SQLite backup including WAL content, as described in the session identity spec; mixed old and new binaries do not guarantee protection. This does not authorize an installation or restart.

An invalid hook identity returns a bounded configuration diagnostic through the start and supported prompt or post-tool output format; a matched pre-edit returns a denial. Visible identity diagnostics are unsupported on Grok prompt, Cursor prompt and every session-end route until actual client invocation and display are proven. The reviewed Grok prompt route can discard allowing output; Cursor prompt and session-end have no verified context channel. Client display of other outputs has not been verified for every client and platform; a missing or unlaunched hook cannot report an error.

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
