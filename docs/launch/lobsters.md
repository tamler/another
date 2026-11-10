# Lobsters — optional technical submission draft

[About and guidelines](https://lobste.rs/about) checked 2026-11-05. Computing substance matters; a launch announcement alone is a weak fit. Use only an existing eligible account. Do not request invitations, create an account or solicit votes. Before submission, inspect current tags, duplicate links and self-promotion rules. Timing: after real technical feedback exists.

Title: Another: local agent mail, advisory holds and resource leases in Rust

URL: https://github.com/tamler/another

Suggested tags, only if present and fitting: rust, programming, release.

Comment draft:

Disclosure: this submission is about Another's own project.

Another stores coordination between independently run coding agents in local SQLite: threaded mail, requests with owners, expiring path holds and resource leases. A CLI and MCP server expose the same local state, while lifecycle hooks provide short digests.

The design choices worth discussing are advisory leases rather than workspace locks, explicit human confirmation for owning-node cleanup, and optional signed-event federation. The hub can read relayed mail bodies, and requests are node-local.

The source and design decisions are linked in the repository. If a release announcement is not substantive enough under the current guidelines, skip this submission and write a technical design article after real operational evidence is available.
