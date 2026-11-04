# Hashnode tutorial — model-assisted draft

Unpublished. Human verification and approval are required, and no account has been created. [Hashnode terms](https://hashnode.com/terms) make the publisher responsible for AI-assisted content and prohibit spam. Recheck current editor guidance before any separately authorized publication.

Proposed title: A local handoff between coding agents over MCP

Disclosure to keep with any approved article: This article was drafted with AI assistance. The publisher must verify its commands and claims against their own run before publication.

Do not cross-post this generated promotional draft to DEV or HN; their restrictions are documented in devto.md and show-hn.md. No firsthand human experience or outcome is invented below.

## Article draft

When one coding agent asks another to review a change, the handoff can disappear into separate terminal sessions. Another gives those sessions a local place to leave messages and requests. It is a Rust CLI and MCP server under Apache-2.0.

This walkthrough uses two demo identities in one repository. It shows the handoff through the CLI first, so you can inspect the state before connecting an agent client. It is an expected workflow, not a claim that the reader's environment has been tested.

## Install and initialize

With Rust 1.88 or newer:

```sh
cargo install --locked --git https://github.com/tamler/another another
another init
```

Run from the repository where the agents will work. State defaults to `~/.another`; `ANOTHER_HOME` selects another directory. For an isolated experiment, point it at a fresh temporary directory and keep the setting consistent across commands.

## Make the handoff visible

```sh
another agent register alice --program claude --project .
another agent register bob --program codex --project .
another send --from alice --to bob --subject "Review the parser" --body "Please check the error paths." --request
another inbox --agent bob
another activity --summary
```

Bob's inbox should show an unread message and an open request. A request can have an owner; an acknowledgement records receipt of a message. Those are different states. Agent names are unique per project, so use `name@project` when a bare name becomes ambiguous.

## Connect your clients

Hooks announce mail at client lifecycle events. Preview their changes before installing:

```sh
another hook install --client all --dry-run
```

If the plan is right, remove `--dry-run`. This changes user-global client configs, backs up modified files and preserves existing hooks. Codex requires reviewing new hooks under `/hooks`; Cursor support is best effort.

The MCP command is `another serve --stdio`. Add it using your client's supported configuration format and restart or reconnect as that client requires. Hooks supply a digest; MCP tools read and send mail. The [getting-started guide](https://toanother.one/docs/getting-started/) contains a configuration example.

## Know the boundary

Another does not spawn agents or schedule a team. Path holds signal intent; they do not lock Git. The resource registry can help agents borrow an existing server, but it is not a GPU scheduler. Cleanup needs human confirmation on the owning node.

Everything can stay on one machine. Optional federation uses a self-hosted hub that can read relayed mail bodies; requests stay node-local. There is no product telemetry or paid tier.

Source and complete CLI reference: https://github.com/tamler/another

## Timing and provenance

Recommend publication after genuine setup feedback and a fresh isolated run. If this is later republished somewhere that permits the content, use the actual first article URL as canonical where supported; do not invent one before publication. Suggested tags: MCP, Open Source, Rust.
