# Waking idle agents (optional adapters)

Hooks are the primary way agents learn about mail: a digest at session start, a notice after the next tool call, and a notice on the next prompt (see [HOOKS.md](HOOKS.md)). An agent sitting idle at its prompt only sees new mail when something types into its terminal. `another wake` does that through optional adapters, and the background daemon does it by itself for idle Herdr agents (see [DAEMON.md](DAEMON.md)). Nothing in Another requires them.

| Adapter | What it runs |
|---|---|
| `tmux` | `tmux send-keys -t <pane> -l "<message>"`, then `tmux send-keys -t <pane> Enter` |
| `cmux` | `cmux send --surface <surface> "<message>\n"`; with target `notify`, `cmux notify --title Another --body "<message>"` |
| `herdr` | `herdr agent prompt <agent> "<message>"` (uses `$HERDR_BIN_PATH` when set) |

Another only calls these external CLIs as separate processes, with no shell and no linking.

## Targets

Another stores one wake target per agent:

```bash
another wake set --agent codex@shop --via tmux --target %3
```

```bash
another wake list
```

```bash
another wake clear --agent codex@shop
```

When an Another session hook runs inside Herdr (`$HERDR_PANE_ID`), tmux (`$TMUX_PANE`) or cmux (`$CMUX_SURFACE_ID`), it records that pane automatically; with nested multiplexers, `TERM_PROGRAM` picks the innermost. The session-end hook forgets the capture again if it still points at that pane, and `another wake prune` drops captures whose pane is gone or whose agent has left. Automatic captures never replace a target you set by hand, and pruning never removes one. Herdr agents default to the Another agent's bare name when no target is stored.

## Waking

```bash
another wake --agent codex@shop
```

```bash
another wake --all
```

```bash
another wake --agent codex@shop --dry-run
```

- The default message is a one-line inbox summary: unread count, urgent count, open requests and up to three subjects. Message bodies are never included.
- An agent with nothing unread and no open requests is skipped unless you pass `--force`. `--message` replaces the text. The default text names the real commands (the `fetch_inbox`, `send_message` and `acknowledge` tools, or `another inbox`, `another reply`, `another ack`).
- Agents take custom text literally, so name commands that exist. There is no `another mail` command. For example:

```bash
another wake --agent codex@shop --force --message "New mail in Another: run fetch_inbox (or another inbox --agent codex@shop), reply with another reply <message-id>, then another ack <message-id>."
```

- `--all` wakes every agent that has a stored target and something waiting, which makes it suitable for a cron job or a Herdr action.
- `--dry-run` prints the commands without running them.

## Herdr plugin

[integrations/herdr](../integrations/herdr) is an optional Herdr plugin. It adds a "wake agents with unread mail" action, an activity summary action, and a live activity pane:

```bash
herdr plugin install tamler/another/integrations/herdr
```
