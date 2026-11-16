# The background daemon

`another serve` is the local daemon. Besides serving MCP and the dashboard on loopback, it does the work nobody else is around to do:

- sweeps expired holds and overdue requests every 30 seconds;
- wakes idle Herdr agents when mail arrives that no hook has announced yet ("push", below);
- every 10 minutes, drops captured wake targets that can no longer be reached.

Hooks work without it. Without the daemon, holds still expire when the next command looks at them, and mail still reaches a session at its next tool call or prompt; an idle agent just is not woken.

The daemon's server has no `--auth`, so every account on this machine can reach it, not only yours. On a shared machine read [Who can reach an unauthenticated loopback server](REMOTE.md#who-can-reach-an-unauthenticated-loopback-server) before installing it.

## Run it in the background (macOS)

```bash
another daemon install
```

This writes a per-user LaunchAgent, `~/Library/LaunchAgents/one.toanother.another.plist`, and loads it. launchd then runs `another serve --bind 127.0.0.1:7466`:

- loopback only (a non-loopback `--bind` is refused);
- started at login and restarted whenever it exits, including after `launchctl kickstart -k`;
- with `ANOTHER_HOME` and your current `PATH` recorded, so it uses the same database and finds `herdr`, `tmux` and `cmux` the way your shell does (launchd's own PATH has no `~/.local/bin` or Homebrew);
- logging to `$ANOTHER_HOME/logs/serve.log`.

```bash
another daemon status
```

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

`another daemon status` reads `systemctl --user show` for whether the unit is loaded and enabled and for its main PID, then checks `GET /health`. `restart` is `systemctl --user restart`. `uninstall` disables and stops the unit, deletes the file and reloads the manager. If disabling fails, it deletes the file only when `systemctl --user show` confirms the unit is stopped and not enabled; when the manager cannot be asked, or the unit is still running, it stops with an error and keeps the file, so you can run it again. `--dry-run` and `--no-load` work as on macOS. systemd stops the daemon with SIGTERM.

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

Skipping this is unsafe when a release changes how repository keys are cached (the `project_repos` table, see [HOLDS.md](HOLDS.md#holds-across-worktrees)). The release after #103 does: keys now carry a `v2:` prefix. The new binary treats an old key as never computed and recomputes it. An old process does not understand the new keys, rewrites the ones it touches in the old form, and then compares keys of two formats. They never match, so it misses holds in sibling worktrees and clones and lets those edits through (it fails open). The new binary never misses one: it recomputes the old keys on its next check. Once no old process is left, the keys settle in the new form.

## Elsewhere

On any other system, or a Linux without systemd, run `another serve --bind 127.0.0.1:7466` under the service manager you have, with `ANOTHER_HOME` set and a restart policy.

## Push: waking idle agents

Mail reaches a working agent through its `post-tool` hook at the next tool call ([HOOKS.md](HOOKS.md)). An agent sitting at its prompt makes no tool calls, so the daemon wakes it instead:

1. Every 2 seconds (`--wake-secs`), for each agent with a Herdr wake target, it checks for unread mail newer than what was already announced. With nothing new, it starts no process at all.
2. New mail waits 2 seconds to settle, so a burst of messages becomes one wake.
3. It asks Herdr about the pane with `herdr agent get <pane>`:
   - `idle` or `done`: it runs `herdr agent prompt <pane> "<inbox summary>"` and marks the mail announced;
   - `working`: it leaves the mail to the `post-tool` hook and looks again in 5 seconds;
   - `blocked` (waiting on a person) or unknown: it does not interrupt;
   - no such pane, or no agent in it: an automatically captured target is dropped.
4. It claims the mail only after Herdr has answered, so a hook that announced it meanwhile wins and no prompt is sent. A wake that cannot be delivered gives the claim back.
5. After a wake it leaves that agent alone for 60 seconds.

Every adapter command has a deadline: 3 seconds for `herdr agent get` (and the tmux probe), 10 seconds for a prompt. Each command runs in its own process group. A command that runs past its deadline, or is still running when the daemon is told to stop, gets SIGTERM for its whole group (so a process it started stops too), then a hard stop half a second later. Once a command has exited, its output is collected for at most a quarter of a second more, so a background process it left holding the output cannot stall the daemon either. A stuck adapter can neither hold up the other agents nor a clean shutdown.

Two limits remain, and both need an adapter that misbehaves. A background process that outlives its command and keeps the output open is no longer waited for, but until it exits it still costs the daemon one small thread and one file descriptor. A process that moves itself into a new session (`setsid`) leaves the command's group, so a timeout does not stop it. Catching that would mean tracking every process an adapter starts.

The summary carries subjects and counts, never bodies. Herdr stays headless: Another only calls `herdr agent get` and `herdr agent prompt`, never bare `herdr`, and never focuses a pane.

Push runs only on a loopback server without `--auth`, so a second server published for remote peers never wakes the same agent twice. Turn it off with `another serve --no-wake`. tmux and cmux targets are not woken automatically, because neither can say whether the agent is busy; `another wake` still reaches them on demand ([WAKE.md](WAKE.md)).

## Wake targets

Session-start hooks record the pane an agent runs in: `$HERDR_PANE_ID` in Herdr, `$TMUX_PANE` in tmux, `$CMUX_SURFACE_ID` in cmux. When multiplexers are nested, `TERM_PROGRAM` names the innermost one. The session-end hook forgets the capture if it still points at that pane. To drop captures left behind by sessions that never ended cleanly:

```bash
another wake prune --dry-run
```

```bash
another wake prune
```

An automatic target goes when its adapter says the pane is gone (Herdr and tmux are asked; cmux is not), or when its agent ended its session or has not been seen for a day. Targets set with `another wake set` are never pruned.

## Counting what happened

```bash
another stats --since 48h
```

```bash
another --json stats --since 48h
```

`another stats` counts this node's coordination evidence from the event log for a period (a duration such as `30m`, `48h` or `2d`, an RFC 3339 time, or an event id; the default is `24h`):

| Count | From |
|---|---|
| Edits stopped by a hold, and distinct collisions (agent, holder, path) | `hold.blocked` events, written by the edit guard with both identities, the paths and the projects |
| Holds taken by the edit hook | `hold.acquired` events marked `auto` |
| Mid-task notices, notices at a prompt | `push.notified` events, with the message ids announced |
| Herdr wakes sent and failed | `push.woke` events, with the agent, pane, outcome and message ids |
| Messages pushed, and how many were then read, acknowledged or answered | Pushed message ids, checked against later reads, acknowledgements and replies by the same agent |
| Requests created, claimed and completed | `request.*` events |

The JSON form adds the 50 most recent stopped edits and wakes. Each line traces back to events you can read with `another events` or `another activity`.
