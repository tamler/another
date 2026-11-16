# Hook packs

The default pack uses advisory path-hold checks for native edits and permits file creation. It does not retain the file object the client writes. An experimental strict pack, installed explicitly with `another hook install --client claude --strict-controlled`, denies every native edit and uses a restricted checked-handle tool instead. It recognizes only Claude Code 2.1.325, cannot create or delete files, and has pending integrated-client and native Windows acceptance. [Controlled edits](CONTROLLED-EDITS.md) lists its limits and recovery behavior. Reinstall without the flag to return to advisory hooks.

Another is hooks-first (see [DECISIONS.md](DECISIONS.md)): agents learn about new mail because a client lifecycle hook injects a short digest, not because they poll or rely on MCP resource subscriptions. Every supported client gets the same hooks, calling the same wrapper script.

| Moment | What Another does | Output |
|---|---|---|
| Session start | Registers the session actor if needed, with no hold, presence or wake-target lifecycle changes | Inbox + last-hour activity digest (≤ 1500 characters) |
| Each prompt | Checks for unannounced mail without a heartbeat | One line, only for mail not announced before; otherwise nothing |
| After each tool call (`post-tool`) | Checks for mail not announced yet, without renewing holds | One line of context mid-task when there is new mail; otherwise nothing |
| Before a native file edit (default `pre-advisory-edit`) | Checks project path holds and takes narrow advisory holds, or names a conflicting holder | Nothing on no conflict or unavailable coverage; a denial for a conflict |
| Before a native file edit (strict `pre-native-edit`) | Denies the edit before invoking Another | Denial for every native edit, including creation and scratch files |
| Before `mcp__another__controlled_edit` (strict `pre-controlled-edit`) | Checks an existing session and creates a durable per-call admission | No output on admission, otherwise a denial |
| Session end | Does not release holds, change presence or clear wake targets without an incarnation fence | Nothing |

Tool hooks run inside a task, so mail sent to a busy agent reaches it at its next tool call without waiting for the next prompt. Advisory holds are checked before a native edit; the client later reopens the path and other writers can still race it. Strict controlled edits use the separate checked-object contract.

The older `pre-edit` command remains an alias for advisory checks, so upgrading the binary does not silently opt existing packs into strict denial. The unreleased development pack that denied in its wrapper must be reinstalled to choose a mode explicitly.

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
| `PreToolUse` | Another: checking advisory file holds |
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

The default `pre-advisory-edit` resolves path aliases using existing filesystem observations, including existing parents for new files, and groups targets by the deeper of their git checkout and registered project. Scratch paths outside both remain unguarded. The session must already have started for holds to be checked. For each project separately, it checks and takes holds in one transaction:

- if another agent holds an overlapping path, nothing is taken and the edit is denied with a message like this one:

```text
Another stopped this edit: src/api/routes.rs is held by codex@shop-review (exclusive hold on shop-review:src/api/**, "router refactor", until 2026-11-10T16:04:00.000Z). Leave it alone until the hold is released. Ask the holder with `another send --from claude@shop --to codex@shop-review --request --subject "Release src/api/routes.rs?"`, or work on other files and try again later.
```

- if the agent already holds the file (or a directory or glob covering it) exclusively, nothing is written, unless that hold is past half its TTL, which renews it;
- otherwise the agent gets a narrow exclusive hold on that one file, with the default 30 minute TTL and the reason "editing (taken by the edit hook)".

Holds are checked across every checkout of the same repository on this machine: sibling worktrees and separate clones of one origin see each other's holds (see [HOLDS.md](HOLDS.md)). A hook session end cannot prove it belongs to the current incarnation, so it leaves holds in place. Inactive holds expire at their TTL or may be released explicitly by their exact actor.

The guard never prints "allow": that would skip the client's own permission prompts. It only ever adds a denial.

## When a hook cannot do its job

No hook event creates an Another home or migrates one. `another init` and `another hook install` create it. Each hook run has a time budget for reading input, database checks and its own work: 2 seconds for tool hooks and 5 seconds for lifecycle hooks. Tool hooks wait at most 50 ms (`post-tool`) or 300 ms (edit hooks) for a database lock. The rule:

- **Push fails open.** Missing database, old schema, locked database, unknown session: `post-tool` prints nothing and exits 0. If an opened database then fails, it appends a line to `$ANOTHER_HOME/logs/hooks.log`, capped at 256 KB (one older copy is kept as `hooks.log.1`).
- **Default edits are advisory.** Conflicts produce a denial when checked successfully. Missing, invalid or unavailable identity/database/input produces no coverage and no denial; a missing wrapper also provides no coverage. A multi-project edit can acquire holds in one project before finding a conflict in another. Scratch paths remain unguarded even when mixed with project paths. At most 128 target names are inspected; larger calls have no advisory coverage.
- **Advisory holds keep their lifecycle.** New paths receive exclusive holds; already covered paths retain their holds, renewing past half their TTL. Pending, claimed and unknown controlled-operation holds cannot be renewed by this route. A discovered checkout can be registered under the existing actor. The hook does not create or modify the target file and cannot make the client's later path open safe.
- **Strict edits fail closed.** The strict wrapper denies every native edit. The controlled hook admits only a supported client, existing bound session and one existing bounded file with a call ID; it denies missing input, unavailable state or any mismatch. Only the controlled server can confirm the checked-object operation. Its exact new hold is not substituted for a manual or earlier hold. See [controlled recovery](CONTROLLED-EDITS.md#confirmation-and-recovery) for fixed expiry and permanent unknown outcomes.

### Unconfirmed commits

Starting a session, controlled admission/settlement and `agent register` use fenced identity transactions. Before commit, Another publishes a synced `identity-ops/<id>.pending` file named after the transaction's last event and removes it once commit is confirmed. An interrupted file stays for reconciliation under the database write lock: the exact event is then present or will never appear. Unknown directory entries or more than 16 files stop writes pending inspection; reads remain possible. Advisory native edits are not durably admitted operations and can proceed without coverage on a write failure. Older binaries do not honor these fences; do not mix them against the home.

Native Windows paths now include private home/sidecar checks, retained handles, durable publication and a PowerShell launcher. They compile against the Windows SDK, but native execution remains unverified and is required before shipping.

With nothing new, `post-tool` only reads, and SQLite in WAL mode lets readers past a writer. Announcing needs the claim write: if another writer holds the database for longer than 50 ms, the notice is not shown on that call and arrives on the next one.

The budget is about 100 ms per tool call. Measured on a laptop over 50 runs each, both hooks took about 13 to 16 ms at the 95th percentile, with or without new mail and with the daemon up or down; with the database locked outright they gave up in about 75 ms (`post-tool`) and 350 ms (`pre-edit`). The pull request that added them has the method.

### Grok and shared hook files

Grok also executes hooks from Claude Code and Cursor config files. To keep one Grok event from running Another twice, an Another hook invoked with `GROK_HOOK_EVENT` set acts as `grok` and stands down when the Grok pack is installed. Without the Grok pack, Grok still gets Another through the Claude or Cursor entries, as `grok@<project>`.

## Identity

Hooks derive a session-specific address from the effective client, project, base name and the client's session ID. The session-start digest gives the full address. It reads without sweeping, so request or hold counts can include expired items until a separate sweep. A missing or invalid ID cannot silently use a shared identity. For commands outside hooks, pass the full address explicitly or run `another session exec --agent <full-address> -- <program> [args...]` so child processes inherit `ANOTHER_AGENT` and `ANOTHER_AGENT_MODE=session`; a hook cannot set its parent shell's environment. For a custom hook base, set `ANOTHER_AGENT` and `ANOTHER_AGENT_MODE=base` before starting the client. If both variables are unset, the client name is the base. A missing, empty or invalid mode with an agent, or a mode without an agent, denies instead of guessing; update and restart older launchers. Session mode requires the full address and verifies the existing binding against the event's session ID. If a launcher drops both variables, it is indistinguishable from an unconfigured default, so verify propagation before relying on that launcher. The derived address is an identifier, not an authentication credential. Existing base-address mail and holds are not moved. A session's Herdr target must be explicitly set; its derived name is not a pane target. Actual client session ID and denial behavior must be verified before claiming coverage. Duplicate imported Grok admission handling remains a separate integration dependency.

New session events omit the raw client session ID and may include a session fingerprint. Historical signed events and replicated legacy events retain their original bytes: raw event listings, federation traffic and authorized database access can still reveal historical IDs. Activity and dashboard views redact those IDs. Upgrading requires a verified quiescent writer fence and a consistent SQLite backup including WAL content, as described in the session identity spec; mixed old and new binaries do not guarantee protection. This does not authorize an installation or restart.

An invalid hook identity returns a bounded diagnostic through supported lifecycle/context routes. Strict edit hooks deny on identity failure; the default advisory edit hook provides no coverage and emits no denial in that case. Visible identity diagnostics are unsupported on Grok prompt, Cursor prompt and every session-end route until actual client invocation and display are proven. Client display has not been verified for every client and platform; a missing hook cannot report an error.

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
- Tool hooks also read `tool_name` / `toolName` and `tool_input` / `toolInput`. The default advisory hook takes native file names from `file_path`, `filePath`, `notebook_path`, `path` or `target_notebook`, and from patch file headers in `command`. The strict controlled hook parses the separate typed operation described in [controlled edits](CONTROLLED-EDITS.md).
- Exit status is always 0; on any error nothing is printed.
- Scripts that prefer explicit calls can use `another --json heartbeat --agent NAME --start|--end`, `another --json inbox --agent NAME --unread`, and `another --json activity --summary`.
