# Another SEO and launch audit — round four

Snapshot: public GET capture began 5 November 2026 at 12:13 Manila (04:13 UTC), followed by link and draft checks in the same audit round. Read-only public HTTP audit and source review from rewritten `launch/seo3` commit `35898005cc8d1054cb1368f572c7270f5c6a17b5`. Findings describe that snapshot; a preview-source fix is not a deployed fix.

This round adds this report and the [launch review checklist](LAUNCH-REVIEW-CHECKLIST-R4.md). Existing site pages, channel drafts and build/check files are unchanged to avoid overlapping the separate content lane. No deployment, submission, provider setting or identity/history change is authorized by these documents.

## Current draft stack

GitHub returned these PRs as OPEN and DRAFT. The branches form a stack; none is a release or deployment certificate.

| Draft | Base | Reviewed head | Scope inspected |
|---|---|---|---|
| [#59](https://github.com/tamler/another/pull/59) | main | `acff7cfe0f97a9c1c4cd1f743a104955b31bb1b2` | Five-page site, build/check, launch drafts, contribution files; body and complete changed-file inventory |
| [#63](https://github.com/tamler/another/pull/63) | launch/seo | `55855738bdd1192af2e1d77a7400c567780c7d5b` | Four guides and factual corrections; body, file inventory and resulting site/launch source |
| [#64](https://github.com/tamler/another/pull/64) | launch/seo2 | `35898005cc8d1054cb1368f572c7270f5c6a17b5` | Five-document reconciliation; body, file inventory and resulting source |

PR descriptions' earlier Rust, browser, Lighthouse and reviewer results are historical reports, not checks repeated by this audit. Earlier PR numbers and commit IDs inside [SEO2.md](SEO2.md), [SEO3.md](SEO3.md) and PR bodies predate the rewrite; use current branch/head metadata for new work. Do not merge or push old-history commits to recover an earlier reference.

## Remaining gaps

| ID / priority | Concrete evidence | Consequence and next evidence required |
|---|---|---|
| A1 — launch gate, live install copy | [Home](https://toanother.one/) contains two Git install snippets without `--locked`; [Docs](https://toanother.one/docs/), [Getting started](https://toanother.one/docs/getting-started/) and [Install](https://toanother.one/docs/install/) each contain one. They still advertise Rust 1.88+. [Install source](../../site/docs/install/index.html), line 40, and the other preview pages already use `--locked`; [check.py](../../site/check.py), lines 92–94, enforces locked Git installs. | Live visitors bypass the repository lockfile and can resolve dependencies outside the tested MSRV set. The paired failure/success install experiment in SEO2 is historical, not rerun here. A separately authorized production update and a fresh locked-install run on the claimed toolchain are needed before calling first use verified. |
| A2 — guide-promotion gate, deployment gap | All four guide URLs below return HTTP 404 with the custom noindex page. The live [sitemap](https://toanother.one/sitemap.xml) and [llms.txt](https://toanother.one/llms.txt) describe five routes; preview [sitemap source](../../site/sitemap.xml) and [llms source](../../site/llms.txt) describe nine. Preview home/docs link all four guides. | Do not promote the guide URLs or submit the nine-route sitemap as deployed. The live five-route sitemap is internally consistent, and no broken guide navigation was found on the live five pages. A later authorized update must include guide files, discovery links, sitemap and llms together, then verify all nine responses. |
| A3 — medium, live comparison destination | [Live compare](https://toanother.one/compare/) links Agent Orchestrator to `github.com/useagent/ao`; the URL returns 200. GitHub's [repository metadata](https://api.github.com/repos/useagent/ao) reports `fork: true`, parent `OrchestratorInc/agent-orchestrator`. [Compare source](../../site/compare/index.html), line 53, already links the parent. | This is a misleading destination, not an HTTP-broken link. Check the upstream destination in the later deployed response. No competitor implementation was inspected or copied. |
| A4 — medium, live demo scope | [Live Docs](https://toanother.one/docs/), mail example, says to run in the project directory but registers bare `alice` and `bob` without `--project`. The recorded inbox immediately below uses `bob@shop`. [Docs source](../../site/docs/index.html), lines 80–90, supplies explicit project flags and explains the shared project root. | Readers can create project-less identities rather than reproduce the illustrated project scope. Publish the existing correction only through a separately authorized update, and verify the example in isolated state; no real-client setup is tested here. |
| A5 — medium, current-versus-historical references | [README source](../../README.md), line 100, directs current readiness work to [closed PR #17](https://github.com/tamler/another/pull/17). Its recreated current reference is [#50](https://github.com/tamler/another/pull/50). SEO3 and #64 also contain historical pre-rewrite IDs. | Readers should not treat a closed old PR or an old merge simulation as current integration evidence. Refresh present-tense navigation in the content lane and preserve historical evidence as historical. This audit neither answers merge questions nor reviews readiness threads. |
| A6 — low, social-image wording | Both live PNGs are 1200×630 and byte-identical. Visual inspection of the downloaded card reads “Local inbox for coding agents”; all five live pages' OG/Twitter alt tags say “A shared inbox for coding agents.” Most preview pages have corrected this, but [compare source](../../site/compare/index.html), lines 18 and 23, still has the older wording. | Align the remaining source alt text in the content lane and verify served tags after a later authorized update. The current alt text still describes the image's purpose; this is a consistency fix, not a failed image fetch or launch blocker. |

Guide URLs checked individually, each returning 404:

- [Claude Code + Codex](https://toanother.one/guides/claude-code-codex/)
- [MCP messaging](https://toanother.one/guides/mcp-agent-messaging/)
- [File holds](https://toanother.one/guides/avoid-agent-file-conflicts/)
- [Local inbox](https://toanother.one/guides/local-agent-inbox/)

## Operator decisions left unchanged

- **Crawler policy:** live [robots.txt](https://toanother.one/robots.txt) includes a Cloudflare-managed block before the repository's four-line file. It declares `search=yes,ai-train=no,use=reference` and specific disallows including GPTBot, ClaudeBot and Baiduspider. The wildcard allows crawling; Googlebot and Bingbot are not specifically disallowed in this snapshot. The local checker validates source output, not the provider's effective policy. Confirm the intended policy separately; llms.txt does not override robots rules or guarantee AI discovery. No setting or DNS change was made.
- **Alternate hosts:** [www](https://www.toanother.one/) and the [Pages project host](https://another-site-review.pages.dev/) return 200 at their own URLs, with canonical `https://toanother.one/`, rather than redirecting. A canonical is present; redirect consolidation is an optional operator decision, not evidence of a missing canonical. No host/domain configuration was changed.
- **Release and submission acceptance:** the public GitHub release list returned no releases. Existing copy appropriately describes archives as future availability. This audit does not certify an installer, tag, release, current client integrations, account eligibility or submission text. The existing [launch order](ORDER.md) remains a recommendation, not authorization.

## Checks that passed

- Fresh GETs returned 200 for all five original canonical pages. Each has one h1, a unique title and description, a matching self-canonical and OG URL, OG/Twitter titles/images/card metadata, and parseable SoftwareApplication/SoftwareSourceCode JSON-LD. No executable scripts were present in those saved responses. This is metadata inspection, not rich-result eligibility or indexing evidence.
- All 73 same-origin href/src occurrences and their anchors on those five pages resolve among the fetched responses. All 18 unique external link targets returned 200 to HEAD requests; that proves HTTP resolution, not the correctness of every destination. The repository's `Safety defaults` heading was also checked on current main for its fragment link.
- The live sitemap contains exactly the five live canonical routes. Live llms.txt lists those routes and repository guides without advertising the four unavailable guides.
- Both social PNGs, CSS, favicon and font return 200. The PNG headers report 1200×630; the downloaded card was visually inspected. A deep missing route returns the custom 404 with `noindex` and no canonical. Direct `/404.html` returns 200, which is separate from the missing-route status.
- `sh site/build.sh` exited 0 on this source: 20 allowed public files, 10 HTML pages, 9 canonical/metadata/JSON-LD records, internal links/anchors, sitemap/robots/llms, PNG dimensions and static-asset restrictions passed. This gate does not check remote links or served provider changes.

## Evidence limits

The lane's STATUS/DONE and private evidence packet retain timestamped response bodies/headers, HTTP summaries and PR metadata. Search-tool output for the home page was a ten-month-old unrelated cache and was excluded; fresh HTTP responses establish the live findings above. This is a bounded snapshot, not continuous monitoring.

No search-console access, selected-canonical/indexing result, browser layout/accessibility run, new Lighthouse run, Rust installation, release archive, live MCP client or human acceptance is claimed. Existing channel-rule checks are inherited preparation, not newly certified account eligibility; recheck policies before any separately authorized human submission. No posting, account creation, production mutation, deployment, DNS/MX or identity/history rewrite occurred.
