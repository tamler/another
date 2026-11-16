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
- **Nothing a sender chose is typed as a key press.**
  - Before any adapter types or shows the line, control characters in it (line breaks, tabs, escape) become spaces.
  - Each subject is cut to 80 characters, and the line to 1,000.
  - For cmux, which unescapes backslash sequences, a backslash in the text becomes `/`, so only the final `\n` submits the line.
  - `--message` text is cleaned the same way.
  - `send` refuses a subject with control characters, and subjects arriving from another node are stored without them (D75).
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

## Herdr pane status

When a hook runs inside a Herdr pane (`HERDR_ENV=1` with `HERDR_PANE_ID` and `HERDR_BIN_PATH` set), it shows the agent's Another state in that pane. These are the same states as `another board` and the dashboard (D70). Outside Herdr nothing is sent.

```bash
"$HERDR_BIN_PATH" pane report-metadata "$HERDR_PANE_ID" --source another \
  --token another_state=blocked --token "another=blocked: src/a.rs is held by claude@shop" \
  --seq <microseconds> --ttl-ms 1800000
```

- `another_state` is `working`, `idle` or `blocked`, and `another` gives the reason while the agent is blocked. Both tokens appear in `herdr pane get` and `herdr agent list`. Add `$another_state` or `$another` to an Agent sidebar row to see them there.
- These tokens are display-only metadata. Herdr's own integration for the client in that pane (Claude Code, Codex and others) keeps its `idle`/`working`/`blocked` state, notifications, `herdr agent wait` and session restore. Another never sends `report-agent` or `release-agent`, because either would take that authority over; Herdr's guide asks hooks that run next to its integrations to use metadata instead. For the same reason Another reports no resume command, and it has none of its own.
- Reports run in the background through the Herdr CLI, so they also work on Windows. A missing or failing `herdr` is ignored and never slows or fails a hook. Working out the state gets 20 ms per hook; on a store too busy for that, the hook sends nothing and the pane keeps its last state.
- An unchanged state is not sent again; it is refreshed before the tokens expire (30 minutes). The session-end hook clears both tokens.
- The last report for each pane is kept in `$ANOTHER_HOME/hooks/state/herdr-status.sqlite`, a small cache separate from Another's database. A hook decides whether to send and reserves the next `--seq`, `max(now in microseconds, last + 1)`, in one write transaction. Parallel hooks in a pane therefore never repeat a number, and a backward clock step never lowers it. A hook that cannot get that lock within 200 ms skips its report. A pane with no row yet starts above the last number in the per-pane `herdr-<pane>.json` file that earlier builds of this feature kept in the same directory, so upgrading never lowers it either.
- A report whose `herdr` could not be started is sent again by the next hook. Another does not wait for Herdr's answer, so a report Herdr rejects is only repaired by the next change or refresh. If the cache file is deleted while Herdr still remembers a higher number for that pane, reports are ignored until the clock passes it.
