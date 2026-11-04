# SEO round two — preview source and factual corrections

This branch adds source for four task guides. Their canonical URLs describe intended public destinations; they have not been deployed in this round. No rank, traffic, client adoption or human acceptance is claimed.

| Search task | Source page | Authority and behavior |
|---|---|---|
| Coordinate Claude Code and Codex in one repo | site/guides/claude-code-codex/index.html | docs/HOOKS.md; explicit project registration, lifecycle digests and a review handoff between independently run clients |
| Agent-to-agent messaging over MCP | site/guides/mcp-agent-messaging/index.html | crates/another/src/mcp.rs and crates/another-core/src/mail.rs; fetch does not mark read, acknowledgements and request ownership are separate |
| Avoid agents editing the same files | site/guides/avoid-agent-file-conflicts/index.html | CLI HoldCmd and core holds.rs; advisory scopes, conflict exit 3, all-or-nothing acquisition, renewal and release |
| Local agent inbox | site/guides/local-agent-inbox/index.html | README, CLI and mail store; a temporary-home exchange, no always-on daemon for CLI mail, explicit storage/privacy boundaries |

Home and docs link to each guide; guides link to relevant peers. The sitemap and llms.txt cover all nine indexable routes. The checker still excludes the noindex 404 from canonical coverage and rejects executable scripts or external assets. Git install commands must include --locked so the documented Rust version uses the tested dependency lockfile.

## PR29 factual review dispositions

| Item | Disposition in this branch |
|---|---|
| R1: unlocked install on Rust 1.88 | ADDRESSED in source. Both commands used cargo +1.88 and source 60617866de8f42e2a02fc50f13a919abac23f328. Unlocked install exited 101 because uuid 1.29 requires Rust 1.89; --locked exited 0 and installed successfully in a fresh temporary root. Public/source-install examples now include --locked; no production redeploy was authorized here |
| R2: duplicate D36 and stack conflicts | PARTIAL. Website decision moved to namespace W1, avoiding product D36/D37 collisions. Reconciliation with the separately changing release stack is NOT ADDRESSED: this PR must remain based on launch/seo, and no other stack was merged/rebased/imported |
| R3: DEV AI promotional copy | ADDRESSED. Generated promotional tutorial removed from DEV preparation; its primary AI guidelines are cited. The separately disclosed Hashnode draft no longer implies DEV cross-posting eligibility |
| R4: HN generated/AI-edited text | ADDRESSED. Generated submission title/comment removed; the file contains human-maker preparation notes and cites general HN rules as well as Show HN rules |
| R5: stale orchestrator fork | ADDRESSED. Compare and sources link to OrchestratorInc/agent-orchestrator; GitHub's fork/parent metadata was checked |
| R6: list/community eligibility | ADDRESSED. wong2 uses a submission form, not PRs; awesome-rust eligibility is not established; Lobsters age/history and human-authorship gates are explicit. Its current tag list marks vibecoding inactive, so that tag is excluded |
| Related factual nits | Project registration matches scoped examples; recorded shop fixture is identified; unsigned/checksum limits are explicit; unsupported crates.io availability wording is removed; category copy and social-card alt text are corrected; official MCP Discord scope is documented |
| Release notes and issue #28 integration | Historical milestone outline is marked non-authoritative for the evolving release stack. The existing newcomer checksum issue is not implemented or edited; its owner must reconcile the selected manifest format before acceptance |

## Human decisions retained

- D1: Existing copyright, published commit identity and unrelated stack defaults need owner decisions. No copyright-holder change, history rewrite or Rust default change is made here. New public content uses the Another brand.
- D2: Whether launch notes and review assets should reach main remains a merge-scope decision. Existing work is preserved; this branch adds no screenshot archive to Git history.
- D3: The independent review reports Cloudflare-managed crawler rules differing from the local robots.txt. The checker validates generated source output only. It does not certify production policy; Cloudflare settings and DNS are unchanged.

No replacement glossary, ambiguous-address walkthrough or checksum recipe is written: issues #26–#28 remain available to newcomers. No issue was closed or reassigned. Newer release-stack features such as its installer and doctor are not advertised as present in this checkout; integration remains with that stack's owner.

## Verification boundaries

Executed command fixtures use fresh temporary node state. A real stdio MCP exchange checks send/fetch/read-only acknowledgement/full acknowledgement/request claim/completion. Hold checks assert conflict status and an empty grant set for a rejected batch. These checks do not establish real Claude Code/Codex UI trust or human setup acceptance.

The static build validates 20 public files, ten HTML documents and nine canonical routes. HTTP and Lighthouse results, exact commit and draft-PR URL are recorded in the external lane handoff. No production deployment, community post, account creation, tag, merge, spend, DNS or MX change is part of this round.
