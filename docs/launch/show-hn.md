# Show HN — draft

Gate: tagged release, working install, human copy review and an available maker to discuss it. Primary [Show HN rules](https://news.ycombinator.com/showhn.html) checked 2026-11-05: submit something runnable, use the prefix, avoid vote requests. Timing: first public launch, while someone can answer.

Title: Show HN: Another — a local inbox and resource registry for coding agents

URL: https://github.com/tamler/another

First comment draft:

Another gives coding agents a shared inbox, activity feed, advisory path holds and a registry of resources they can borrow. It is a Rust CLI and MCP server under Apache-2.0.

The problem is the handoff between independent sessions. One agent needs another to review a parser; a third needs to know whether a model server is already running. Another stores that coordination state locally in SQLite. Session hooks announce waiting mail, and MCP tools or CLI commands let agents act on it. It does not launch agents or decide their tasks.

Try it with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/tamler/another another
another init
```

The walkthrough at https://toanother.one/docs/getting-started/ includes a two-agent shell exchange before you connect a client. No account is required. Hook installation is user-global, so preview its plan first.

Optional federation connects machines through a self-hosted hub. The hub can read relayed mail bodies, and requests stay node-local. Path holds are advisory; cleanup requires human confirmation on the resource's owning node.

Feedback on setup friction and handoff failures would be useful, especially from people running different coding clients in the same repository.
