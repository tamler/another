# Two peer agents, one repository: lanes and holds

`claude` and `codex` work in the same repository, and nobody manages them. Each one creates and leads a lane, holds its own directory, and they sort out a conflict between themselves.

```bash
sh examples/peer-lanes/run.sh
```

What happens:

1. **Lanes.** `claude` creates `checkout` and `codex` creates `search`, each with a charter, each led by its creator (`another lane create`).
2. **Holds.** Each agent holds its own directory. A hold takes the lane its agent leads, so `holds --by-lane` shows one group per lane.
3. **A cross-lane conflict.** `codex` tries to hold `src/checkout/cart.rs`. It is refused with exit code 3 and nothing is granted. `claude`, who leads the holding lane, gets a conflict mail from the system agent.
4. **Peers resolve it.** `codex` mails a request. `claude` reports progress and releases its directory hold, keeping only the file it still needs. `codex` then holds `cart.rs` for the `search` lane (`--lane search`).
5. **DONE.** Both lanes close with `lane done` (outcome and PR link), and `lane show --markdown` prints the old STATUS-file view.

With the pre-edit hook installed (`another hook install --pre-edit`), Claude Code and Codex make the same hold check automatically before every edit.
