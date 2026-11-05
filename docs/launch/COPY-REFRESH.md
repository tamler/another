# Launch copy refresh — drafts only

Nothing here has been posted, scheduled or submitted, and no account was created. A human decides whether any of it goes out, using an account that exposes no personal name. [ORDER.md](ORDER.md) and each channel's own file still set the gates.

## What changed

- **The opening now names the problem.** The earlier drafts open by listing features. These open with the homepage's line about copy-pasting between agents, so the post and the page it links to say the same thing.
- **Federation stays out of the lead.** On `main`, a tunnelled hub without `--auth` exposes `/activity`. Mention federation only with its limits, and don't promote it until the owner decides on PR #53 ([READINESS.md](READINESS.md)).
- **Links go to pages that exist.** Every link below returned 200 on production on 2026-11-05. Guide links remain out until the guides are deployed.

## Core message

- **Line:** From one agent to another.
- **Problem:** Without a shared inbox, you're the one copy-pasting messages between agents.
- **What it is:** A local inbox, activity feed and resource registry for Claude Code, Codex, Grok CLI and other MCP clients.
- **What it isn't:** It doesn't start or manage agents. You choose their tasks and review their work.

## X, Bluesky and Mastodon

Each post is under 280 characters.

Launch post:

Without a shared inbox, you're the one copy-pasting messages between agents.

Another is a local inbox for Claude Code, Codex, Grok CLI and other MCP clients. Apache-2.0, local SQLite, no product telemetry.

First handoff: https://toanother.one/docs/getting-started/

Follow-up:

Another doesn't start or manage agents. One session sends a request; another claims it, replies in the thread and acknowledges the mail. You still choose the tasks and review the work.

Compared with orchestrators: https://toanother.one/compare/

## r/mcp

The rules in [reddit-r-mcp.md](reddit-r-mcp.md) still apply: a human rewrites this in their own voice and checks the rules and flair first.

Title: Another: a local MCP inbox so you stop copy-pasting between coding agents

Affiliation: this is an announcement on behalf of the Another project.

If you run more than one coding agent, you probably relay messages between them by hand. Another gives them a shared local inbox over MCP and a Rust CLI: send a request, claim it, reply in a thread and record an acknowledgement.

Hooks announce waiting mail at session start and on prompts; the MCP tools read and send it. State is SQLite on your machine, with no signup or product telemetry. Apache-2.0.

You can try the exchange in a shell before adding MCP to a client: https://toanother.one/docs/getting-started/
Source: https://github.com/tamler/another

It doesn't start agents or run a workflow. Path holds are advisory, and reaping needs human confirmation on the owning node.

The most useful feedback is a handoff that failed or a setup step that was unclear. Please leave private mail bodies, tokens and personal paths out of reports.

## r/ClaudeCode weekly showcase

Comment only, in the current showcase thread, after a human rewrite. See [reddit-r-ClaudeCode.md](reddit-r-ClaudeCode.md).

Sharing Another, for when you're relaying messages between Claude Code and another coding session by hand. Affiliation: posting on behalf of the Another project.

It's a local inbox, activity feed and resource registry over MCP and a Rust CLI. Hooks announce waiting mail at session start and on prompts, and a client such as Codex can reply through the same inbox. Apache-2.0, local SQLite, no product telemetry.

Preview the hook changes first: `another hook install --client claude --dry-run`. Installing changes user-global config and backs up the files it modifies. Another doesn't run agents or guarantee they read mail immediately.

First handoff: https://toanother.one/docs/getting-started/

## Discord

For an eligible showcase channel only, after reading its rules. See [discord.md](discord.md).

Affiliation: this is from the Another project. It's an Apache-2.0 local inbox and resource registry that lets coding agents you already run hand work to each other, over MCP and a Rust CLI. First handoff: https://toanother.one/docs/getting-started/ Source: https://github.com/tamler/another

## Directories and Product Hunt

Short description, under 160 characters:

A local inbox, activity feed and resource registry for the coding agents you already run. MCP tools, hooks and a Rust CLI. Apache-2.0.

Product Hunt description, under 260 characters (the eligibility decision in [product-hunt.md](product-hunt.md) still comes first):

Stop copy-pasting messages between coding agents. Another gives the clients you already run mail, an activity feed, path holds and a resource registry through hooks, MCP and a CLI. Local SQLite, Apache-2.0, no product telemetry.

## Channels with no copy here

These channels refuse or restrict model-written copy, so a human writes from the preparation notes or the channel is skipped:

- Hacker News: [show-hn.md](show-hn.md)
- DEV: [devto.md](devto.md)
- Lobsters: [lobsters.md](lobsters.md)
- r/LocalLLaMA: [reddit-r-LocalLLaMA.md](reddit-r-LocalLLaMA.md)
- This Week in Rust: [rust-community.md](rust-community.md)

## Claims

Supported by the repository:

- Apache-2.0, no paid tier, no product telemetry. The telemetry claim was checked in source, without a traffic capture.
- Local SQLite, with no account needed.
- Loopback only unless `--auth` and `--allow-remote-bind` are both given.
- Reaping needs human confirmation on the owning node and is never remote.

Don't claim:

- That any client has been tested end to end. Claude Code, Codex, Grok CLI and Cursor loading the hooks and MCP config has not been run ([READINESS.md](READINESS.md)). A human handoff between two clients is needed before saying more than the docs do.
- Prebuilt binaries or a one-line installer. No release exists; until one does, install means Cargo or a source build.
- Benchmarks, adoption or production use. None is recorded.
- That path holds stop anything. They signal intent.
- That the federation hub can't read mail. It can read the bodies it relays.

## Before anything goes out

- The owner has decided what ships and whether v0.1.0 is released.
- If the PR stack has merged, recheck this copy against the new `main`. It describes milestones 1–6.
- Open every link in the post on production.
- Run the install command in a fresh environment.
- Recheck the channel's rules on the day.
- Don't ask for stars, votes or reposts.
