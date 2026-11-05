# Good first issues — repository drafts

Authorized destination: tamler/another only. Before creation, check all existing issues for duplicate work. Labels already exist: `documentation`, `good first issue`. These are bounded documentation follow-ups, not product defects or promises. Published issue URLs are recorded in the lane STATUS/DONE, not invented here.

Note for the issue owner: #26 and #27 remain newcomer work; no glossary or ambiguity walkthrough is implemented here. The launch/polish branch adds a checksum recipe to the install page that covers #28's platforms, filename assumptions and failure behavior, for both the per-archive .sha256 files and the release-preparation stack's additional SHA256SUMS. It differs from #28's text in one way: it compares digests instead of using `-c`, because `sha256sum -c` on macOS can exit 0 without checking anything. Decide whether to close #28 once that reaches `main`, or keep it open for a newcomer to extend. This note does not edit the live issue or close it.

## 1. Docs: add a glossary for requests, path holds and resource leases

Body:

A new contributor can help explain three distinct coordination concepts without changing Rust behavior.

Add a short glossary to the repository docs and link it from README's Concepts section. Explain a mail acknowledgement versus request ownership, a path hold versus a resource lease, and what TTL expiry means in each case. Use the current README, docs/RESOURCES.md and docs/DECISIONS.md as authority; do not copy competitor descriptions.

Acceptance:
- Each term links to its detailed existing guide.
- Advisory holds, node-local requests and human-approved owning-node cleanup remain explicit.
- Examples contain no secrets or personal paths.
- Local Markdown links resolve. If site content changes, sh site/build.sh passes.
- No Rust, schema, release or deployment changes.

## 2. Docs: add an ambiguous-agent-address troubleshooting walkthrough

Body:

Document how to recognize an ambiguous bare agent name when the same name exists in two projects, and how to address the intended agent with name@project.

Use a fresh temporary ANOTHER_HOME and temporary demo repositories. Record the actual CLI version/commit and exact commands/output, including the error and the corrected inbox/send command. Keep IDs and results from one continuous fixture; identify it as fixture data. Do not alter normal client hooks or a user's real node.

Acceptance:
- Reproduce the ambiguity and show a working qualified address in one continuous isolated run.
- Explain that names are unique per project; do not imply name@project resolves every cross-node ambiguity.
- Link the walkthrough from the relevant CLI/concept docs.
- Redact personal paths and secrets; no invented output.
- No Rust behavior changes; run site/build.sh if site pages change.

## 3. Docs: add a release-checksum verification recipe

Body:

Add a short documentation recipe showing how to verify Another release archive SHA-256 files on macOS and Linux, using shasum -a 256 -c and sha256sum -c respectively. Check the current release workflow's filename format.

Until archives are actually published, use a clearly labeled local fixture archive/checksum pair. Demonstrate both matching and tampered-file failure with actual command exit statuses. Do not download or execute an unverified binary and do not fabricate release URLs.

Acceptance:
- Both platform commands and checksum-file filename assumptions are explained.
- A failed checksum explicitly stops installation.
- Verification establishes byte integrity against the chosen checksum, not a claim of publisher identity or a signature.
- Link from installation/release documentation; keep source installation available.
- No workflow, Rust or release publishing changes.
