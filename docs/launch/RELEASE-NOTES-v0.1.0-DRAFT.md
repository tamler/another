# Another v0.1.0 — release notes draft

Historical milestones 1–6 outline; not the authoritative v0.1.0 release notes. The separately reviewed [release-preparation PR #51](https://github.com/tamler/another/pull/51) includes newer coordination work and must be reconciled by its owner before publication. Do not publish this outline as the final release. No date, archive availability or final CI status is asserted here.

Another is a local-first inbox, activity feed and resource registry for coding agents. It works through a Rust CLI and MCP server, with lifecycle hooks that announce waiting mail. Apache-2.0, no product telemetry and no paid tier.

## Included in the first-release scope

- Threaded agent mail with read/ack state and node-local requests with owners.
- Advisory path holds, an activity feed and session digests.
- Resource registration, borrowing, heartbeats, orphan inspection and human-approved owning-node cleanup.
- Hooks for Claude Code, Codex and Grok CLI, plus best-effort Cursor integration.
- Scoped remote tokens, an audit log and optional signed-event federation.
- Optional tmux, cmux and Herdr wake adapters, plus the Herdr plugin.
- Static setup guides at https://toanother.one/.

## Install

With Rust 1.88 or newer:

```sh
cargo install --locked --git https://github.com/tamler/another another
another init
```

The command above installs the repository version, which can advance beyond this release. For the exact tagged source after publication, use `cargo install --locked --git https://github.com/tamler/another --tag v0.1.0 another`.

This checkout's release workflow produces macOS arm64/x86_64 and static musl Linux arm64/x86_64 archives with per-archive SHA-256 files. The separate release stack keeps those files and adds one SHA256SUMS; use the workflow and release artifacts that actually ship. The [install page recipe](https://toanother.one/docs/install/#verify) handles either file. A checksum comparison establishes integrity against that file, not publisher identity or a signature. Publish archive links only after delivery. Verified macOS downloads may require removing quarantine; remove an old executable before replacing it to avoid stale code-signature caching.

## Limits to keep visible

Path holds are advisory. Hooks run at client lifecycle events. Another does not launch or schedule agents. Reaping requires explicit human confirmation and is never remote. Remote access has no built-in TLS; use an authenticated server behind a suitable tunnel or proxy. The federation hub can read relayed mail, and cross-node requests are not supported.

## Maintainer release gate

Confirm the final product revision, all CI gates and independent hardening reviews. Run fresh archive installs on the target platforms and a real client handoff. Verify checksums and release links. Replace this gate with actual evidence before publishing; the launch-site lane does not certify other lanes' Rust changes or physical-device/client behavior.
