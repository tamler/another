# Examples

These are small multi-agent setups that you can run on one machine. Each script:
- works in a throwaway data directory, so your own `~/.another` is never touched;
- prints every `another` command it runs, along with that command's output;
- checks the outcome at the end and exits non-zero if something is wrong.

CI runs each script against the current build (`crates/another/tests/examples.rs`).

| Script | Shows |
|---|---|
| [two-agent-handoff.sh](two-agent-handoff.sh) | Two agents on one machine. alice takes a hold on the file she is changing and asks bob for a review as a claimable request. bob claims it, acknowledges it, replies in the thread and completes it. alice then reads the reply and the "Done:" notice, acknowledges both and releases her hold. The script ends by checking that both inboxes are empty. |
| [federation-hub-two-spokes.sh](federation-hub-two-spokes.sh) | A hub and two spokes (laptop, runner), as three data directories on one machine. The hub issues one invite per spoke with different permissions. Each spoke joins and syncs. alice on the laptop mails bob on the runner with `bob@@runner`, and bob replies. alice and bob are plain names, so the send and reply use the explicit operator repair mode (`--unguarded-repair`); participating agents use the private `another.agent/1` channel. |

Run one with the `another` binary on your `PATH`:

```bash
sh examples/two-agent-handoff.sh
```

Or run one against a build from this checkout:

```bash
cargo build -p another
ANOTHER_BIN=target/debug/another sh examples/federation-hub-two-spokes.sh
```

The federation example starts a hub with `another serve --federation` on a loopback port the system picks, with wake delivery off. The listener serves only `/fed/*` and `/health`; the script discovers its port from the federation-only startup line. It stops that hub, by its process id, when the script exits. On real machines each node runs on its own host, and spokes reach the hub through a tunnel; see [docs/FEDERATION.md](../docs/FEDERATION.md).

A group-queue worker-pool example will follow once group queues are on `main`.
