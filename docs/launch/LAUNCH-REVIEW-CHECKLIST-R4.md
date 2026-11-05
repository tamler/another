# Another launch review checklist — draft

Use with the dated [round-four audit](SEO-AUDIT-R4.md) and existing [launch order](ORDER.md). This is a review aid for a later authorized operator. Unchecked gates remain open; this file does not authorize a deployment, release, submission, account action or provider/DNS change. It supplies no ready-to-post channel copy.

## Source handoff

- [ ] Re-read the current heads of [#59](https://github.com/tamler/another/pull/59), [#63](https://github.com/tamler/another/pull/63) and [#64](https://github.com/tamler/another/pull/64), and the separate content follow-up. Record base/head SHA, review disposition and actual check results. Earlier PR numbers, hashes and performance results are historical after the rewrite; never merge or push old-history commits.
- [ ] Confirm the approved source set and rerun `sh site/build.sh`. Inspect exit status and output. A green local build proves the nine-route preview package, not that production serves it. This audit does not resolve outstanding integration or merge choices.
- [ ] Confirm each install example uses `--locked`, each demo explicitly scopes its identities, and the comparison points upstream. Check audit A1/A3/A4 against the final content, including any sibling-lane revisions. Preserve unsigned/checksum and federation/advisory-hold limitations.
- [ ] Refresh present-tense links to recreated PRs without rewriting historical test evidence. Reconcile social-image alt text against the actual card, including the compare page (A5/A6).

## Live acceptance after a separately authorized update

- [ ] Record the deployment identifier and approved source SHA. Fetch each of the nine intended canonical routes; require 200, expected content and matching canonical/social metadata. Verify the four guide URLs individually before linking them in any launch material (A2).
- [ ] Confirm home/docs discovery links, sitemap and llms.txt describe the same deployed route set. Fetch every listed route and both social images; check PNG signatures/dimensions and a deep missing URL's real 404/noindex behavior. A stale five-route deployment is not a nine-route acceptance result.
- [ ] Inspect served install snippets for `--locked` and run the approved first-use instructions in fresh isolated state on the claimed minimum Rust version and relevant platforms. Record commands, toolchain, exit codes and limitations. A fetched HTML snippet is not an install test; earlier run logs are not a new acceptance run.
- [ ] Compare effective response headers and robots.txt with the intended provider policy. Record managed crawler blocks and alternate-host behavior explicitly. Refer policy/redirect choices to the operator; do not change settings or DNS merely to make source and wire bytes match.
- [ ] Record browser, accessibility and real-client checks actually performed for the final content. Mark unrun checks and pending human acceptance plainly; do not carry earlier Lighthouse scores forward as new evidence.

## Submission and discovery gates

- [ ] Keep all channel files as drafts. Before any later authorized submission, a human checks current channel rules, their existing account's eligibility, truthful maker affiliation and availability to respond. No vote solicitation, coordinated engagement, new accounts or scheduled posting follows from this checklist.
- [ ] Follow [Show HN preparation](show-hn.md) and [DEV preparation](devto.md): final eligible text must meet their human-authorship restrictions. Do not paste or lightly rewrite generated promotional text, or use a model to rewrite a human draft into a restricted-channel submission/comment. Those files are preparation notes, not publication-ready drafts.
- [ ] Apply the other channels' own gates in [ORDER.md](ORDER.md), including the existing deferrals. A factual outline does not establish posting eligibility. No submission is required to complete this audit.
- [ ] For later search verification, use the [human instructions](search-console.md) only with separate authorization and existing access. This lane makes no Search Console/Bing change or DNS/TXT addition. Record actual verification and selected-canonical results if later obtained; a sitemap/llms file or submission never guarantees indexing.

The audit's delivered state is a reviewed documentation PR with observed gaps and explicit limits. Live correction, release acceptance and public distribution remain separate actions requiring their own authorization and evidence.
