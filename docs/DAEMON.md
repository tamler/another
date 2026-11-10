# The background daemon

`another serve` is the local daemon. Besides serving MCP and the dashboard on loopback, it does the work nobody else is around to do:

- sweeps expired holds and overdue requests every 30 seconds;
- wakes idle Herdr agents when mail arrives that no hook has announced yet ("push", below);
- every 10 minutes, drops captured wake targets that can no longer be reached.

Hooks work without it. Without the daemon, holds still expire when the next command looks at them, and mail still reaches a session at its next tool call or prompt; an idle agent just is not woken.

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

## Elsewhere

`another daemon` only manages launchd. On Linux, run `another serve` under your service manager. A systemd user unit along these lines works; it is an example, not something Another's tests exercise:

```ini
[Unit]
Description=Another local daemon

[Service]
ExecStart=%h/.local/bin/another serve --bind 127.0.0.1:7466
Environment=ANOTHER_HOME=%h/.another
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now another.service
```

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
