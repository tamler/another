# The background daemon

Current participating policy: contract-minted actors authorize actions; names and transport labels are selectors only. All rights use a 120-second default TTL with requested bounds of 30 seconds through 24 hours. Observed session end releases all session and dependent rights immediately. Presence, idle reminders, status and transport observations renew no file leases. Routine hooks are quiet except for conflicts and one idle reminder. Cursor remains unguarded. The older operational examples below describe operator compatibility, not participating authority; see [AGENT-CONTRACT.md](AGENT-CONTRACT.md).

`another serve` is the local daemon. Besides serving MCP and the dashboard on loopback, it does the work nobody else is around to do:

- sweeps expired holds and overdue requests every 30 seconds, and releases queue claims with no sign of life for the grace period ([QUEUE.md](QUEUE.md#when-a-claim-is-released-automatically));
- asks each transport in use (Herdr) what it can see every 30 seconds, and records it per agent session ("The sweep", below);
- wakes an idle agent session when mail arrives that no hook has announced yet ("Waking idle agents", below);
- every 10 minutes, drops session rows and captured wake targets that have aged out.

Waking through a transport is a core part of Another, and hooks are the fallback everywhere (D84). Hooks work without the daemon: holds still expire when the next command looks at them, and mail still reaches a session at its next tool call or prompt. Without the daemon an idle agent is not woken, no session is checked, and `another transport status` says so.

On every supported platform, `another daemon status` reports `healthy` only when a successful `/health` response names the same `node_id` as the current home's database, opened read-only. Another node, a missing node id, malformed JSON or an unreadable database reports `healthy no`, with a detail. A failed connection reports that `/health` could not be reached; a non-numeric or non-loopback bind reports that the address was not checked. The check contacts only a numeric loopback address, follows no redirects and reads at most 16 KiB. `another doctor` checks launchd on macOS and the systemd user unit on Linux, reading the installed definition's `ANOTHER_HOME` on both platforms to flag another home, and reusing its server check's result without another network call. Doctor warns when a response names a different node, but still accepts a successful response with no readable node id or no local database identity; its `healthy` row does not guarantee identity in those cases.

## Run it in the background (macOS)

```bash
another daemon install
```

This writes a per-user LaunchAgent, `~/Library/LaunchAgents/one.toanother.another.plist`, and loads it. launchd then runs `another serve --bind 127.0.0.1:7466`:

- loopback only (a non-loopback `--bind` is refused);
- started at login and restarted whenever it exits, including after `launchctl kickstart -k`;
- with `ANOTHER_HOME` and your current `PATH` recorded, so it uses the same database and finds `herdr` and the programs of your command transports (`tmux`, `cmux`) the way your shell does (launchd's own PATH has no `~/.local/bin` or Homebrew);
- logging to `$ANOTHER_HOME/logs/serve.log`.

```bash
another daemon status
```

Status compares the loaded launchd job's recorded `ANOTHER_HOME` with the current home (`--home` or `ANOTHER_HOME`). A label serving a different home reports `loaded no`, no PID and `healthy no`, with a `detail` explaining that it belongs to a different home. If the job's home cannot be read, the detail says its home could not be verified and status likewise does not attribute it to this home. JSON includes the same `detail` when present. The file and log paths still describe the current home's configuration. Status changes nothing; installing again still replaces the label as described under Upgrading.

```bash
another daemon restart
```

```bash
another daemon uninstall
```

`--dry-run` shows the plist path and the `launchctl` commands without running them, `--no-load` writes the plist without loading it, and `--bind` picks another loopback port. The daemon stops cleanly on SIGTERM, which is what launchd sends.

## Run it in the background (Linux)

The same commands manage a systemd **user** unit, `~/.config/systemd/user/one.toanother.another.service` (under `$XDG_CONFIG_HOME/systemd/user` when that is set). Install writes it, then runs `systemctl --user daemon-reload`, `systemctl --user enable` and `systemctl --user restart` on it. The unit runs `another serve --bind 127.0.0.1:7466`:

- loopback only;
- started with your user manager (`WantedBy=default.target`) and restarted 10 seconds after any exit (`Restart=always`);
- with `ANOTHER_HOME` and your current `PATH` recorded;
- logging to `$ANOTHER_HOME/logs/serve.log` (`StandardOutput=append:`, which needs systemd 240 or newer).

`another daemon status` reads `systemctl --user show` for whether the unit is loaded and enabled and for its main PID, and reads `ANOTHER_HOME` from the installed unit. A foreign or unverifiable home reports `loaded no`, no PID and `healthy no`, with a detail; a matching home is checked through `GET /health`. `restart` is `systemctl --user restart`. `uninstall` disables and stops the unit, deletes the file and reloads the manager. If disabling fails, it deletes the file only when `systemctl --user show` confirms the unit is stopped and not enabled; when the manager cannot be asked, or the unit is still running, it stops with an error and keeps the file, so you can run it again. `--dry-run` and `--no-load` work as on macOS. systemd stops the daemon with SIGTERM.

Your user manager, and so the daemon, runs while you have a session. To keep it running after you log out, and to start it at boot, turn on lingering for your user; Another does not do this for you:

```bash
loginctl enable-linger "$USER"
```

Over SSH or in a container without a user manager, `systemctl --user` cannot connect, and `another daemon install` says so. There, run `another serve` under the service manager you have.

## Run it in the background (Windows)

On Windows the same commands manage a Task Scheduler task named `one.toanother.another`. Install writes its definition to `$ANOTHER_HOME\daemon\one.toanother.another.xml`, registers it for your user with `schtasks /Create /XML`, and starts it with `schtasks /Run`. The task:

- runs `another --home <ANOTHER_HOME> serve --bind 127.0.0.1:7466 --log-file <ANOTHER_HOME>\logs\serve.log` (Task Scheduler keeps no output, so `serve` writes its own log);
- starts when you log on, as you, without elevation, and has no time limit;
- inherits your environment, so `PATH` is whatever your logon session has.

`status` reports the task as loaded when the task list from `schtasks /Query` contains it, and checks `GET /health`; Task Scheduler gives no process id. `schtasks /End` returns before the server has exited, so `restart` and `uninstall` both wait, up to 10 seconds, until the task shows stopped and the daemon's address is free: Another binds it itself for a moment, because a hung server can fail `GET /health` and still hold the port. `restart` then runs the task again. `uninstall` then deletes the task and removes the definition. Deleting a task does not stop a server it started, so if the task is still running, its state cannot be read, or the address is still in use, both stop with an error: `restart` does not run the task, and `uninstall` deletes nothing and keeps the definition. It reads "running" from the task's last result, `267009` while it runs, which unlike the status text is the same in every system language. If the delete fails, it removes the definition only when the task list confirms the task is not there.

Two limits on Windows. Task Scheduler restarts the task when it fails, but it is not documented to restart a server that started and later exited, so keep-alive is weaker than on macOS and Linux: `another daemon status` shows `healthy: no` when it is down, and `another daemon restart` brings it back. And Another's tests register the task on a hosted CI runner, which runs as an administrator; registering it as a standard user is expected to work but has not been checked.

## Upgrading

A running `another` keeps the code it started with, so after installing a new binary restart everything that was already running.

1. **The launchd daemon (macOS).** `another daemon restart` relaunches the binary path recorded in the plist at install time, not whatever `another` is on your `PATH` now. If the new build is at the same path, restart:

   ```bash
   another daemon restart
   ```

   If it is at a different path (say `cargo install` put it in `~/.cargo/bin` but the plist names `~/.local/bin/another`), run the new binary's install instead, which rewrites the plist and reloads it. Run it with the same `ANOTHER_HOME` (or `--home`), `--label` and `--bind` as the original install. Install records `ANOTHER_HOME` and `PATH` from the shell you run it in, so from a plain shell it would point the daemon at a different database, or, with another label, add a second daemon beside the old one:

   ```bash
   another daemon install
   ```

   The plist (`~/Library/LaunchAgents/<label>.plist`, `one.toanother.another.plist` by default) shows what the old install recorded: the binary is the first `ProgramArguments` entry, the bind follows `--bind`, and `ANOTHER_HOME` and `PATH` are under `EnvironmentVariables`.

2. **The systemd and Task Scheduler daemons (Linux, Windows).** The same rule applies: `another daemon restart` relaunches the binary recorded at install time (the unit's `ExecStart`, or the task's `<Command>`), and the new binary's `another daemon install` rewrites the definition when the path changed. A daemon you run under some other service manager needs that manager's restart, and a check that it names the new binary.

3. **Every other long-running server.** That covers each client that runs `another serve --stdio` as its MCP server (restart the client or reconnect the server), a remote server started with `another remote up` or `another serve --auth`, and any `another serve` started by hand. Stop each one and start it again from the new binary.

Hooks start a fresh process on every call and need nothing.

Skipping this is unsafe when a release changes how repository keys and checkout ids are cached (the `project_repos` table, see [HOLDS.md](HOLDS.md#holds-and-checkouts)). Recent builds do: cached keys carry a `v4:` prefix. The new binary treats an older-format row as never computed and recomputes it. An old process does not understand the new keys, rewrites the rows it touches under the older rules, and then compares keys of two formats. They never match, so it can miss holds inside this checkout (it fails open). Once every long-running `another` is restarted, the keys settle in one format.

## Elsewhere

On any other system, or a Linux without systemd, run `another serve --bind 127.0.0.1:7466` under the service manager you have, with `ANOTHER_HOME` set and a restart policy.

## The sweep

Every 30 seconds, for each transport that has at least one session row made by a hook, the daemon runs the transport's `list` once (`herdr agent list`) and records the answer for every such row: its state, whether the transport recognised the session (by its session id, or by the weak match for a pane that reports none), and the pane it is in now. Nothing else observes a session. So with no mail waiting, the daemon still starts one `list` process per transport in use every 30 seconds. With no session row it starts none.

From what the sweep records come the session's shown state, the reasons under `another transport status --attention` (a `blocked` agent at once, a `gone` session after 60 seconds, `unknown` or `no_answer` after 10 minutes), the end of a stop when the agent is seen `working` or `blocked`, and the time each session was last seen alive. [WAKE.md](WAKE.md) has the rules, the session rows, and the confirmed-gone signal, which this build keeps switched off for Herdr.

The sweep writes no event in steady state, whatever the agents do.

## Waking idle agents

Mail reaches a working agent through its `post-tool` hook at the next tool call ([HOOKS.md](HOOKS.md)). An agent sitting at its prompt makes no tool calls, so the daemon wakes it instead:

1. Every 2 seconds (`--wake-secs`) it looks, for each agent with a session row, for unread mail that no channel has announced yet. A pass with nothing new starts no process.
2. New mail waits 2 seconds to settle, so a burst of messages becomes one wake.
3. It picks the agent's session to wake: the `idle` one (Herdr's `idle` or `done`) whose hooks ran last, even while a sibling session of the same agent is working. Sessions that are stopped after a wake that did not start work, were woken less than 60 seconds ago, or are in any other state are left out. If no session remains and one is `working`, the mail is left to that session's `post-tool` hook and the daemon looks again in 5 seconds. A session that vanished or cannot be asked is the sweep's business alone.
4. It asks Herdr about that pane now with `herdr agent get <pane>`, and goes on only if the answer is `idle` and identifies the session. Any other answer is written nowhere, and that session is not asked again for 30 seconds.
5. Only after Herdr has answered, and in one transaction, it reads the session's row again, reserves it for this wake and claims the mail. A hook that announced the mail meanwhile wins and no prompt is sent. A stop or a wake that another pass recorded meanwhile is seen, so two passes that overlap (two daemons on one home, or `another wake --all` beside the daemon) do not both type into one pane.
6. It runs `herdr agent prompt` and records the outcome as a `transport.wake` event and on the session row, in one transaction with the claim.

A wake has seven possible outcomes ([WAKE.md](WAKE.md#what-a-wake-can-end-as)). `started` and `unconfirmed` keep the claim. Every other outcome gives the claim back, so the hooks announce the mail at the agent's next turn. A wake that was delivered but did not start work (`not_started`), or whose answer could not be read (`unknown`), is a recorded failure and **stops** that session: a line that was or may have been typed is never typed again by the daemon. A session that could not be reached at all (`undelivered`) is tried once a minute, and after three in a row once per 10 minutes.

The gap after a wake is 60 seconds per session, not per agent name: after a claim was given back, another idle session of the same agent may be woken for the same mail inside that minute, so one message is typed at most once per session.

The identity check and the prompt are two Herdr calls. A pane that changes hands in the milliseconds between them is typed into.

If the daemon stops between typing and recording, or the record cannot be written, the claim stays taken and nothing is retyped; that mail is announced by no channel and stays unread in the inbox.

Every transport command has a deadline: 3 seconds for `herdr agent get` and `herdr agent list`, 12 seconds for a prompt (Herdr itself is given 8 seconds). Each command runs in its own process group, and its stdout and stderr are captured, 1 MiB in total; more than that ends the call. A command that runs past its deadline, or is still running when the daemon is told to stop, gets SIGTERM for its whole group (so a process it started stops too), then a hard stop half a second later. Once a command has exited, its output is collected for at most a quarter of a second more, so a background process it left holding the output cannot stall the daemon either. A wake pass stops between agents when a sweep is due, so failing wakes cannot starve the sweep. A stuck transport can neither hold up the other agents nor a clean shutdown.

Two limits remain, and both need a transport that misbehaves. A background process that outlives its command and keeps the output open is no longer waited for, but until it exits it still costs the daemon two small threads and two file descriptors. A process that moves itself into a new session (`setsid`) leaves the command's group, so a timeout does not stop it. Catching that would mean tracking every process a transport starts.

The wake line carries subjects and counts, never bodies. Herdr stays headless: Another only calls `herdr agent list`, `herdr agent get` and `herdr agent prompt`, never bare `herdr`, and never focuses a pane.

The sweep and the wake pass run only on a loopback server without `--auth`, so a second server published for remote peers never wakes the same agent twice. `another serve --no-wake` turns off every transport call the daemon makes. The daemon wakes only sessions it can identify: a target set by hand, a bare Herdr agent name, and a session in a command transport such as tmux or cmux are never woken automatically; `another wake --agent` still reaches them on demand ([WAKE.md](WAKE.md)). For a command transport that has a `list` command the sweep runs it too, so a vanished tmux pane is noticed. The daemon reads `transports.json` once per sweep and logs a refused file once each time the reason changes.

## Session rows and wake targets

A hook that runs inside Herdr (`$HERDR_PANE_ID`), or inside a command transport (the variable its `capture_env` names, such as `$TMUX_PANE`), gives its session a row; the session-end hook deletes it. When multiplexers are nested, the transport whose name is `$TERM_PROGRAM` wins, else Herdr, else the first in the order `transports.json` lists them. A hook no longer stores a pane as the agent's wake target. To drop what sessions that never ended cleanly left behind:

```bash
another wake prune --dry-run
```

```bash
another wake prune
```

Pruning asks no transport. A session row goes when it was confirmed gone more than a day ago, or when its hooks have been quiet for a day and the transport did not show the session inside that day. A row that only holds the outcome of a wake by hand goes a day after that wake. A tmux or cmux pane that an earlier build captured as a wake target goes when its agent ended its session or has not been seen for a day. Targets set with `another wake set` are never pruned.

## Counting what happened

```bash
another stats --since 48h
```

```bash
another --json stats --since 48h
```

`another stats` counts this node's coordination evidence from the event log for a period (bare seconds such as `3600`, a duration such as `30m`, `48h` or `2d`, an RFC 3339 time, or an event id; the default is `24h`):

| Count | From |
|---|---|
| Edits stopped by a hold, and distinct collisions (agent, holder, path) | `hold.blocked` events, written by the edit guard with both identities, the paths and the projects |
| Holds taken by the edit hook | `hold.acquired` events marked `auto` |
| Mid-task notices, notices at a prompt | `push.notified` events, with the message ids announced |
| Herdr wakes sent and failed | `transport.wake` events written by the daemon, with the agent, session, pane, outcome and message ids. Outcome `started` counts as sent; `not_started`, `undelivered` and `unknown` count as failed; the other outcomes count as neither. Wakes by hand and by `another wake --all` are not counted. `push.woke` events from earlier builds still count. |
| Messages pushed, and how many were then read, acknowledged or answered | Pushed message ids, checked against later reads, acknowledgements and replies by the same agent |
| Requests created, claimed and completed | `request.*` events |

The JSON form adds the 50 most recent stopped edits and wakes. Each line traces back to events you can read with `another events` or `another activity`.
