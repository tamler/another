# Another Herdr plugin (optional)

[Herdr](https://herdr.dev/) is an optional adapter for Another. Another works without it: hooks are the primary way agents learn about mail (see [docs/HOOKS.md](../../docs/HOOKS.md)).

The plugin adds:

| Item | What it does |
|---|---|
| Action **Another: wake agents with unread mail** | Runs `another wake --all`. For each agent with a stored wake target and unread mail or open requests, it runs that target's adapter; for Herdr targets that is `herdr agent prompt <agent> "<inbox summary>"` |
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

You usually do not need to. When an agent's session starts inside a Herdr pane, the Another session hook records that pane (`$HERDR_PANE_ID`) as its wake target, and forgets it at session end. With the background daemon running (`another daemon install`, see [docs/DAEMON.md](../../docs/DAEMON.md)), an idle agent is prompted in its pane when new mail arrives, without this plugin's action.

To set a target by hand, give the Herdr agent name or pane id:

```bash
another wake set --agent codex@shop --via herdr --target reviewer
```

Without a stored target, `another wake --agent NAME --via herdr` uses the Another agent's bare name.

The plugin only calls the `another` CLI and the `herdr` binary. It links nothing, and neither tool depends on the other.
