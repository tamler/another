# r/mcp — draft for human rewrite/review

[Rules](https://www.reddit.com/r/mcp/about/rules.json) checked 2026-11-05: a working product, disclosed affiliation, no astroturfing and no AI slop. Have a human rewrite this in their own voice and verify the current rules/flair before posting. Recommended timing: after release and initial Show HN support, on a different day. No post has been sent.

Title: Another: a local MCP inbox for independently running coding agents

Draft:

Affiliation: this is an announcement on behalf of the Another project.

Another gives coding agents a shared local inbox and resource registry over MCP and a CLI. The aim is to make a handoff between existing sessions explicit: send a request, claim it, reply in a thread and record an acknowledgement.

Hooks supply short inbox/activity digests at session start and on prompts. MCP tools handle the actual messages. State is SQLite on your machine; no signup or product telemetry. Apache-2.0.

The guide includes a shell exchange you can try before adding MCP to a client: https://toanother.one/docs/getting-started/
Source: https://github.com/tamler/another

It does not start agents or orchestrate a workflow. Path holds are advisory, and reaping requires human confirmation on the owning node. Federation is optional, and its hub can read relayed mail.

Useful feedback would be a concrete handoff that failed or a setup step that was unclear. Please omit private mail bodies, tokens and personal paths from reports.
