# Another Herdr plugin (optional)

[Herdr](https://herdr.dev/) is the transport Another wakes idle agents through (see [docs/WAKE.md](../../docs/WAKE.md)). This plugin is an optional extra for Herdr itself. Another works without Herdr too: hooks still tell an agent about mail at its next turn (see [docs/HOOKS.md](../../docs/HOOKS.md)).

The plugin adds:

| Item | What it does |
|---|---|
| Action **Another: wake agents with unread mail** | Runs `another wake --all`: one wake pass by the daemon's rules. For each agent with mail no hook has announced yet, it prompts the agent's idle Herdr session that Another can identify, with `herdr agent prompt <pane> "<inbox summary>"`, and prints why it skipped the others |
| Action **Another: activity summary** | Runs `another activity --summary --since 1h` |
| Pane **Another: activity** | Shows a live, refreshing view of the last hour of activity |

## Install

```bash
herdr plugin install tamler/another/integrations/herdr
```

From a local checkout:

```bash
herdr plugin link ./integrations/herdr
```

## Pointing Another at Herdr agents

You usually do not need to. When an agent's hooks run inside a Herdr pane (`$HERDR_PANE_ID`), Another keeps a row for that session and finds it in Herdr by its session id; the session-end hook deletes the row. With the background daemon running (`another daemon install`, see [docs/DAEMON.md](../../docs/DAEMON.md)), an idle agent is prompted in its pane when new mail arrives, without this plugin's action. `another transport status` lists the sessions.

To set a target by hand, give the Herdr agent name or pane id:

```bash
another wake set --agent codex@shop --via herdr --target reviewer
```

A target set by hand is used by `another wake --agent NAME` only: the daemon and `another wake --all` wake only sessions they can identify. Without a stored target or a session, `another wake --agent NAME --via herdr` uses the Another agent's bare name.

The plugin only calls the `another` CLI and the `herdr` binary. It links nothing, and neither tool depends on the other.
