# Waking agents through a transport

A working agent learns about mail from its hooks: a notice after its next tool call or at its next prompt (see [HOOKS.md](HOOKS.md)). An agent sitting idle at its prompt runs no hook, so something has to type into its session. That is a wake, and it is a core part of Another (D84): the daemon wakes idle agents by itself, and `another wake` does it on demand. Hooks are the fallback everywhere: with no transport and no daemon, mail still arrives at the agent's next turn.

A **transport** is a tool that runs agent sessions. Another talks to every transport through one interface with three operations:

| Operation | What it answers |
|---|---|
| `wake` | Deliver one line to a session, and say whether it was delivered and whether the agent started working. |
| `status` | What is at one target now: its state, and which session it is. |
| `list` | Every session the transport can see. |

Another never reads pane output and never sends keys. That stays inside the transport.

This build has two kinds of transport. **Herdr** is built in. A **command transport** is one you describe in a config file, as the commands to run for the same three operations: that is how tmux, cmux or any other tool with a CLI is used (see [Command transports](#command-transports)). There is no built-in tmux or cmux adapter any more. The name `rex` is reserved for a transport that is not in this build.

## What a wake can end as

Every wake call ends in one of seven outcomes. This table is the only definition of a failed wake.

| Outcome | Delivered | Started | Failed wake | Meaning |
|---|---|---|---|---|
| `started` | yes | yes | no | The transport saw the agent working, or at a dialog, after the line. |
| `not_started` | yes | no | **yes** | The line was submitted and the transport saw no activity. |
| `unconfirmed` | yes | unknown | no | Submitted; the transport could not say what followed. Not counted as a successful wake. |
| `refused_blocked` | no | no | no | Refused before anything was sent: the agent is waiting on a person. |
| `gone` | no | no | no | The transport says there is no such session. |
| `undelivered` | no | no | **yes** | Nothing was sent: the program could not be started, or refused the command. |
| `unknown` | unknown | unknown | **yes** | An unreadable answer, or Another's own deadline. The line may be in the terminal. |

A wake that was delivered but did not start work is a recorded failure, and it is shown to you (see [What you see](#what-you-see)).

## States

`status` and `list` report one of these states for a target:

| State | Meaning |
|---|---|
| `working` | A turn is running. |
| `idle` | Ready for input. Herdr's `idle` and `done` both mean this: after a turn Herdr says `done`. |
| `blocked` | Waiting on a person at an approval or question dialog. |
| `present` | Something is at the target and the transport reports no state. |
| `unknown` | The transport sees an agent and cannot classify it. |
| `gone` | The transport answered, and nothing is at the target. |
| `no_answer` | The transport could not be asked: program missing, server down, timeout, unreadable output. |

`gone` and `no_answer` are never merged: a transport that cannot be asked says nothing about whether a session is still there. `stuck` is not a state a transport reports. Another shows it for a session whose last wake did not start work.

## The Herdr transport

The binary is `$HERDR_BIN_PATH`, else `herdr` on `PATH`. Another calls Herdr with exactly three subcommands, each as its own process with no shell:

| Operation | Command | Deadline |
|---|---|---|
| `wake` | `herdr agent prompt <pane> "<line>" --wait --until working --until blocked --until idle --until done --until unknown --timeout 8000` | 12 seconds |
| `status` | `herdr agent get <pane>` | 3 seconds |
| `list` | `herdr agent list` | 3 seconds |

Herdr is never called bare and never to focus a pane. Its answers are read as Herdr 0.9.5 gives them: a success as JSON on stdout with exit 0, a server error as JSON on stderr with exit 1.

| Herdr answers to a prompt | Outcome |
|---|---|
| exit 0, result type `agent_prompted` | `started` |
| exit 0 with no readable result | `unknown` |
| error `agent_prompt_stalled` | `not_started` |
| error `agent_blocked` | `refused_blocked` |
| error `agent_not_found` | `gone` |
| error `timeout` | `unconfirmed` |
| any other error, exit 1 with no JSON, or Another's 12 second deadline | `unknown` |
| exit 2, or the program could not be started | `undelivered` |

Two things follow from how Herdr works. After `not_started` the line may still sit in the agent's input box, in front of whatever is typed next, so Another does not type into that session again by itself and tells you to clear it. And Herdr's wait tracks the agent's state, not a turn: if the agent was already working when the line landed, the answer is `started`. That is accepted, since a working agent gets its mail from its post-tool hook.

## Sessions

Another keeps one row per agent **session**, not per agent name: two sessions under one name are two rows, each with its own state, its own wakes and its own stop.

- A local hook that runs inside Herdr (`$HERDR_PANE_ID` is set) or inside a command transport (the variable named by its `capture_env` is set) makes sure the session's row exists, with the client's session id from the hook input and that pane as the place to look first. When several of those variables are set, because one multiplexer runs inside another, the transport whose name equals `$TERM_PROGRAM` wins, else Herdr, else the first one in the order the config lists them. Session start and every prompt stamp the row; tool hooks stamp it at most once a minute per session.
- The session-end hook deletes that session's row, and only that one.
- From then on the session is found in Herdr by its **session id**, never by its pane alone. Herdr reports a session id for Claude Code agents, and it is the same id the hook saw. When the pane moves, the row follows it.
- Herdr reports no session id for Codex agents today. Such a session gets a **weak match**: the same pane, the same client kind, and the same directory as its own hook saw, and no newer session of that client kind captured in that pane. A weak match is enough to wake the agent. It is never taken as proof that the session is alive, and it cannot be followed when the pane moves.
- A pane that reports a different session id is `unknown`: it hosts another session (a nested agent CLI, or a cleared or replaced conversation). It is never woken and never called gone.

One risk remains with a weak match: another agent of the same kind, started in the same directory in that same pane, is taken for this one and gets its wake line, mail subjects included. With default agent names that is the same Another agent. With `ANOTHER_AGENT` names it may not be.

**Automatic wakes for weak matches are switched on in this build** (`HERDR_WAKES_WEAK_MATCH` in `crates/another/src/transport/herdr.rs`), as they were before this interface, when every idle Herdr pane was woken. They rest on Herdr reporting a Codex at an approval dialog as `blocked` and refusing the prompt, which has not been checked against a live Codex yet. If that check fails, the constant is switched off: the daemon and `another wake --all` then skip every weakly matched session and say why, and `another wake --agent` still reaches it.

After an upgrade to this build, an idle session is not woken by the daemon until it has run one hook, because the hook is what makes its row.

## When the daemon wakes an agent

The daemon ([DAEMON.md](DAEMON.md)) asks each transport in use what it can see every 30 seconds and records it per session. Every 2 seconds it looks for agents with mail that no hook has announced yet, and for each one:

1. picks the agent's `idle` session whose hooks ran last, even while a sibling session is working. If every session is working, the mail is left to a post-tool hook;
2. asks `status` for that pane, and goes on only if the answer is `idle` and identifies the session (by its session id, or by the weak match);
3. reserves the session and claims the mail in one step that reads the session's row again, so a hook that announced the mail meanwhile wins and nothing is typed, and two passes that overlap (two daemons on one home, or `another wake --all` beside the daemon) cannot both type into one pane;
4. calls `wake`, and records the outcome.

| Outcome | The mail | The session |
|---|---|---|
| `started` | announced | ready for the next wake after 60 seconds |
| `unconfirmed` | announced | no change |
| `not_started`, `unknown` | given back to the hooks | **stopped**: not typed into again |
| `refused_blocked`, `gone` | given back to the hooks | no change |
| `undelivered` | given back to the hooks | tried again after 60 seconds; after three in a row, once per 10 minutes |

**A line that was or may have been typed is never typed again by itself.** Typing blind into a menu can choose an option, and the input box may hold the earlier line. The stop on a session ends only when the transport sees it `working` or `blocked`, when a hook of that session runs, when you wake it by hand and it starts, or when you run `another wake clear`. Giving the mail back means the hooks announce it at the agent's next turn, so a failed wake never hides mail.

The daemon wakes only sessions it can identify. A target set by hand, a bare Herdr agent name, and a session in a command transport are never woken automatically. A `blocked` agent is never prompted, and neither is one in any state other than `idle`.

A pane id that Herdr lists becomes a session's target, so it is held to the rule for every target. If one listed pane id breaks that rule (empty, longer than 200 characters, starting with `-`, or holding a control character), the whole listing counts as no answer: every Herdr session reads `no_answer` and none is woken until a later check lists only usable ids. Dropping just the bad entry would make that session look gone.

## What you see

```bash
another transport status
```

```bash
another transport status --attention
```

One line per session: agent, session id (shortened), transport and pane, the shown state and for how long, the last wake and its age, and why the session needs you, if it does. It reads what the daemon recorded and calls no transport. For example:

```text
codex@shop 7f3a..  herdr w1:p3  stuck 12m  last wake: delivered, did not start 12m ago  needs attention: stuck
  the wake line may still be in the agent's input box; clear it before typing
```

`--attention` lists only sessions with a reason:

| Reason | When |
|---|---|
| `blocked` | The agent is waiting on a person. Shown at once. |
| `stuck` | The last wake did not start work (`not_started` or `unknown`). |
| `gone` | The transport has not listed the session for 60 seconds. |
| `unknown`, `no_answer` | That state for 10 minutes. |
| `undelivered` | Three wakes in a row could not be delivered. |

Other marks: `weak match`; `transport not configured` for a row whose transport no longer exists; for a row that only holds the outcome of a wake by hand, where its target came from (`target set by hand`, `captured pane, woken by hand`, or `woken by hand` for `--target` or the agent's name); `not checked since <time>` for a session the daemon has not looked at in 5 minutes. When nothing was checked in the last 5 minutes, the first line asks whether the daemon is running. When `transports.json` was refused, a line says why.

`--json` gives every field. Two of them have no column in the text form:

| Field | Values |
|---|---|
| `origin` | Where the row's target came from. `hook`: a hook of the session recorded it. The other three are for a row that only holds the outcome of a wake by hand: `hand_set` (a target stored with `another wake set`), `captured` (a pane that a hook of an earlier build stored as the agent's wake target), `given` (`--target`, or the agent's bare name). |
| `transport_configured` | `false` when nothing provides the row's transport now: not `herdr`, and not in `transports.json`. |

The report itself has `config_error`, set when `transports.json` was refused.

Herdr has no state that means "at a rate-limit menu". An idle-looking session at a menu with mail waiting ends `not_started` and is shown as `stuck`.

## Is the session gone?

A session that the transport stops listing is `gone` at the next check and is shown under `--attention` after 60 seconds.

Another can also **confirm** a session gone, which later lets the queue release that session's claims. A session is confirmed only when it has been `gone` for 15 minutes of watched time (a gap with no check restarts the window), no hook of that session ran in the window, and the listing is trusted: it still shows at least one other session, by its session id and at the pane it had before. So the last or only session matched by id is never confirmed, and neither is a weakly matched one.

**In this build that confirmation is switched off for Herdr** (`HERDR_CONFIRMS_GONE` in `crates/another/src/transport/herdr.rs`). How Herdr lists agents across its own restart has not been checked yet, and a wrong answer there could confirm a live session gone. Until it is checked, a vanished session is noticed and shown, and nothing is confirmed.

## Waking by hand

```bash
another wake --agent codex@shop
```

```bash
another wake --all
```

```bash
another wake --agent codex@shop --dry-run
```

- **`--agent`** is a direct action by you. The target is, in order: `--target`; a target you set with `another wake set`; the agent's session whose hooks ran last; a pane that a hook of an earlier build captured from tmux or cmux; and with `--via herdr`, the agent's bare name. When the target comes from a Herdr session, Another first asks `status` and types nothing unless the pane still shows that session. A command transport cannot say which session is in a pane, so a session there is typed into as recorded. It does not require `idle` (Herdr itself refuses a blocked agent; a command transport refuses one where its `status` can say `blocked`), and it ignores a stop and the 60 second gap. The outcome is recorded.
- `--via` takes `herdr` or the name of a configured command transport. Any other name is refused with what to set; `--via rex` answers "the rex transport is not in this build".
- **`--all`** is one wake pass of the daemon, run from the command line, which makes it suitable for a cron job or a Herdr action when no daemon runs: the same choice of session, the same check, the same claim on the mail, the same stop and gap. It wakes for mail that no hook has announced yet, so after a hook has shown the mail it wakes nobody. It no longer wakes targets set by hand or bare Herdr names, and it never wakes a session in a command transport. It prints why it skipped each agent.
- The default line is a one-line inbox summary: unread count, urgent count, open requests and up to three subjects. Message bodies are never included. With nothing unread and no open requests, `--agent` skips the agent unless you pass `--force`. `--message` replaces the text.
- `--dry-run` prints the transport's commands and runs nothing.
- The command exits non-zero when a wake failed (see the table of outcomes) or could not be attempted. For each wake it prints what happened in the words of that table, and what the transport answered.

**Nothing a sender chose is typed as a key press.** Before a line reaches any transport, control characters in it (line breaks, tabs, escape) become spaces, each subject is cut to 80 characters and the line to 1,000. `send` refuses a subject with control characters, and subjects arriving from another node are stored without them (D75). `--message` text is cleaned the same way.

Agents take custom text literally, so name commands that exist. There is no `another mail` command. For example:

```bash
another wake --agent codex@shop --force --message "New mail in Another: run fetch_inbox (or another inbox --agent codex@shop), reply with another reply <message-id>, then another ack <message-id>."
```

## Targets set by hand

```bash
another wake set --agent codex@shop --via herdr --target w1:p3
```

```bash
another wake list
```

```bash
another wake clear --agent codex@shop
```

Another stores at most one hand-set target per agent. It is used by `another wake --agent` only: it is never checked by the daemon and never woken automatically. `--via` takes `herdr` or the name of a configured command transport. `another wake list` shows each stored target and marks one whose transport is not configured. `another wake clear` forgets the target and deletes the agent's session rows, which also lifts a stop.

A target is 1-200 printable characters and must not start with `-`: it is passed on a command line, and Herdr takes it as a positional argument, so a leading `-` would be read as an option. A wake also refuses such a target when it comes from `--target`, from a target stored before this rule, or from Herdr's default of the agent's name.

`another wake prune` (and the daemon, every 10 minutes) drops what has aged out, without asking any transport: a session confirmed gone more than a day ago; a session whose hooks have been quiet for a day, unless the transport still matched it inside that day; a row left by a wake by hand, a day after that wake; and a pane that an earlier build captured from tmux or cmux, once its agent ended its session or has not been seen for a day. Targets you set are never pruned.

## Command transports

A command transport is a named set of commands in **`$ANOTHER_HOME/transports.json`** (`~/.another/transports.json` by default). You write that file; Another reads it and never writes it. There is no command, no flag, no MCP tool and no message from a peer that can set a command: the file is the only way in. With no file there are no command transports, and Herdr works as before.

This example gives you tmux and cmux as the built-in adapters of earlier builds ran them:

```json
{
  "version": 1,
  "transports": {
    "tmux": {
      "wake": [["tmux", "send-keys", "-t", "{target}", "-l", "{message}"],
               ["tmux", "send-keys", "-t", "{target}", "Enter"]],
      "status": {"argv": ["tmux", "display-message", "-p", "-t", "{target}", "#{pane_id}"],
                 "exit_codes": {"0": "present"}},
      "list": {"argv": ["tmux", "list-panes", "-a", "-F", "#{pane_id}"]},
      "capture_env": "TMUX_PANE"
    },
    "cmux": {
      "wake": [["cmux", "send", "--surface", "{target}", "{message}\\n"]],
      "backslash_to_slash": true,
      "capture_env": "CMUX_SURFACE_ID"
    },
    "cmux-notify": {
      "wake": [["cmux", "notify", "--title", "Another", "--body", "{message}"]],
      "backslash_to_slash": true
    }
  }
}
```

| Key | Meaning |
|---|---|
| `version` | Must be `1`. |
| `transports.<name>` | A command transport. A name is a lowercase letter, then up to 31 lowercase letters, digits, `_` or `-`. `herdr` and `rex` are refused. |
| `wake` | Required. A list of commands, each a list of strings (the program, then its arguments), run in order. |
| `status` | Optional. `argv` and `exit_codes`, both required: `exit_codes` maps an exit code to one of the [states](#states). |
| `list` | Optional. `argv`. |
| `capture_env` | Optional. The name of an environment variable. When a hook runs with it set, its value is recorded as that session's target on this transport. |
| `backslash_to_slash` | Optional, default false. Replace `\` with `/` in the message, for a tool that unescapes backslash sequences. cmux does: with this on, the only escape left is the `\n` of the example, which submits the line. |
| `confirm_secs` | Optional, default 8, from 0 to 30. How long to keep asking `status` after a wake. |

### How a command is run

- Each command is started directly, as the program and its arguments, **with no shell**. Nothing in a value is ever split, quoted, expanded or globbed.
- `{target}` and `{message}` are replaced inside an argument in one pass, after the list of arguments is fixed. So a value never adds an argument and never splits one, whatever it holds: spaces, quotes, `$(...)`, `;`. Text inside a value that looks like a placeholder is not replaced again. Braces that are not one of the two placeholders are left alone (`#{pane_id}` is tmux's own).
- An argument that begins with a placeholder and comes out starting with `-` would be read as an option. The wake then ends `undelivered` and nothing is run. A target can never start with `-`; a `--message` you type could.
- The same two values are in the command's environment as `ANOTHER_TARGET` and `ANOTHER_WAKE_MESSAGE`. A script should read those.
- The message is always the one safe line of [Waking by hand](#waking-by-hand): no control character, no bidirectional override, at most 1,000 characters. The target is 1-200 printable characters and does not start with `-`. Nothing else is given to a command: not the agent's name, not its session id, not a mail body.
- `{message}` has a value in `wake` only, `{target}` in `wake` and `status`. Used anywhere else, the file is refused.
- Deadlines: 10 seconds for each `wake` command, 3 seconds for `status` and for `list`. A command that writes more than 1 MiB is stopped: `status` and `list` then count as no answer, and a `wake` command as `unknown`.

### When the file is refused

The whole file is refused, and no command transport exists until it is fixed, when:

- it is not valid JSON, or `version` is not `1`, or any key is unknown;
- a rule of the table is broken, or a command is empty;
- a placeholder appears in a program name (the first string of a command);
- on Unix, the file is not owned by the user running Another, or is writable by group or others (`chmod go-w` fixes the second).

One more check is a lint and not a security boundary: the file is refused when an argument that holds a placeholder comes directly after `-c`, or after a group of short flags that ends in `c` such as `-lc`. That is how a shell is handed its code, and a mail subject would then run as shell code. The lint knows only that pattern. It cannot see a shell reached another way, and wrapping a command in `ssh` hands the arguments to a remote shell that it cannot see either. If a command needs a shell, make it a script that reads the two environment variables.

Herdr keeps working while the file is refused. The reason is printed by `another transport status`, `another wake`, `another wake list` and `another doctor`, and the daemon logs it once each time the reason changes. The file is read when a command transport is about to be used and once per check by the daemon. There is no cached copy, so a corrected file takes effect at once.

### How answers are read

| Operation | Reading |
|---|---|
| `wake` | The first command cannot start or exits non-zero: `undelivered`. A later command fails, or any command runs past its deadline: `unknown`. All exit 0: delivered. What follows depends on `status`, below. |
| `status` | The exit code is looked up in `exit_codes`. A code that is not there, a deadline or a program that cannot start is `no_answer`. It never reports a session id, a client kind or a directory. |
| `list` | Exit 0, and each non-empty line of output is one session: a target, then optionally a tab and a state word (`working`, `idle`, `blocked`, `present`, `unknown`), then optionally a tab and a session id. With no state word the state is `present`. A non-zero exit or a line of another shape is no answer for the whole list. With no `list` key, sessions of this transport are never observed. |

After a delivered wake:

- With no `status`, with `confirm_secs` 0, or with a `status` that can say neither `working` nor `blocked`, the wake ends `unconfirmed` at once: delivered, and the transport cannot say whether the agent started.
- Otherwise `status` is asked every half second for `confirm_secs`. `working` or `blocked` seen gives `started`. Every answer `idle` gives `not_started`, which is a failed wake and stops the session as for Herdr. Anything else gives `unconfirmed`.

Before the first command, a transport whose `status` can say `blocked` or `gone` asks it once: `blocked` ends the wake as `refused_blocked` and `gone` as `gone`, with nothing typed. The tmux example can say neither, so it runs exactly its two `wake` commands.

### What a command transport cannot do

A command transport's `status` cannot say **who** is in a pane. So:

- **Its sessions are never woken automatically**, by the daemon or by `another wake --all`. `another wake --agent` works.
- With the tmux example, `list` shows whether a pane is still there. A session's row reads `present`, then `gone` once a good list no longer has its pane, and `gone` is shown under `--attention` after 60 seconds, so a vanished agent is noticed. tmux's exit 1 can mean "no such pane" or "no server", so the example leaves it unmapped: a dead tmux server reads `no_answer`, never `gone`.
- tmux reports no session id, so such a row is never matched, not even weakly: it is never taken as alive, never confirmed gone, and its hooks have to keep running for the row to stay (a row quiet for a day is pruned and comes back with the next hook). A wake by hand ends `unconfirmed`: "delivered, not confirmed: tmux cannot report whether the agent started".
- With the cmux example (no `status`, no `list`) a session is recorded and can be woken by hand. It is never observed.
- A script whose `list` prints a session id on each line is matched by session id as Herdr sessions are, and the row follows the session when its target changes. It is still not woken automatically. **Confirming such a session gone is switched off in this build** (`COMMAND_CONFIRMS_GONE` in `crates/another/src/transport/command.rs`), for the reason it is off for Herdr: what a listing shows across the tool's own restart is not known.

Nothing is filled in with a guess.

### Coming from the built-in tmux and cmux adapters

Earlier builds had tmux and cmux built in. They are gone. After the upgrade:

- A stored tmux or cmux target stays stored. Until `transports.json` defines a transport of that name, `another wake --agent` refuses it, types nothing, and says what to set; `another wake list` and `another doctor` mark it, and `another transport status` marks the row an earlier wake to it left. Copy the example above into `~/.another/transports.json` and the same commands run as before. Another writes no config for you.
- A hook inside tmux or cmux records nothing until the transport is configured. Once it is, the session gets a session row, as a Herdr session does, and no longer a stored wake target.
- The cmux target `notify` used to mean `cmux notify`. It has no meaning now and a wake to it is refused. Use the `cmux-notify` transport of the example, and set the target again: `another wake set --agent <agent> --via cmux-notify --target notify` (that transport ignores the target, which still has to be given).

Another only calls these external CLIs as separate processes, with no shell and no linking.

## Herdr plugin

[integrations/herdr](../integrations/herdr) is an optional Herdr plugin. It adds a "wake agents with unread mail" action (which runs `another wake --all`), an activity summary action, and a live activity pane:

```bash
herdr plugin install tamler/another/integrations/herdr
```
