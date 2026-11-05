# r/ClaudeCode — weekly showcase comment draft

[Rules](https://www.reddit.com/r/ClaudeCode/about/rules.json) checked 2026-11-05: simple project sharing belongs in the weekly showcase thread; stay relevant, disclose affiliation and avoid repeated promotion. Recheck the live thread and rewrite personally before using. Do not make a standalone promotional post. Timing: after release, separate from r/mcp.

Comment draft:

Sharing Another, a project for coordinating Claude Code with other coding sessions. Disclosure: this is the project's launch.

It adds a local inbox, activity feed and resource registry over MCP and a Rust CLI. Claude Code hooks announce waiting mail at session start and on prompts; another client, such as Codex, can receive and reply through the same inbox. It is Apache-2.0, uses local SQLite and has no product telemetry.

Preview hook changes first: `another hook install --client claude --dry-run`. Installation changes user-global config and backs up modified files. Another does not run agents or guarantee they read mail immediately.

First handoff: https://toanother.one/docs/getting-started/
Source: https://github.com/tamler/another

Feedback on Claude Code setup and cross-client handoffs is welcome.
