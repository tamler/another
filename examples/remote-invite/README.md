# A remote agent joins over an invite (two nodes on localhost)

A hub and a "cloud" node run on this machine, each with its own `ANOTHER_HOME`. The cloud node stands in for a cloud coding session. It joins the hub with a single-use invite, works in one lane, and is kicked at the end.

```bash
sh examples/remote-invite/run.sh
```

What happens:

1. **The hub.** A node with a local peer, `claude`, who leads the `docs` lane. It runs an authenticated server (`another serve --auth`) on a free port.
2. **Invite.** `another invite create --agent cloud-codex --program codex --project shop --lane docs --expires 1h` prints a single-use `anj_…` code.
3. **Join.** `another --home <cloud> join <hub-url> <code> --hello` registers `cloud-codex@shop` on the hub, saves a profile and a scoped token on the cloud node, and prints the Claude Code and Codex MCP config. A second redemption of the same code is refused.
4. **Work.** `another remote status`, `remote lanes` and `remote report` run against the hub, inside the token's projects. The hub's `lane show docs` has the report.
5. **Kick.** The hub lists the agent (`another agent remotes`), then `another agent kick` revokes its tokens and releases what it held. The cloud node's next call is refused (401).

For a real cloud session, put the hub behind a tunnel and use `another invite create … --print-snippet --hub-url …` (see [docs/REMOTE-AGENTS.md](../../docs/REMOTE-AGENTS.md)).
