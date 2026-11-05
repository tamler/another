# Launch preparation round four

Source and draft changes only, stacked on `launch/seo3`. Nothing here was deployed, posted or submitted. Audit date: 2026-11-05.

## Fixed in source

| Gap | Fix |
|---|---|
| No `/guides/` page, although every guide's breadcrumb said "/ guides" | New hub with canonical, sitemap and llms entries |
| Guides absent from the main nav; getting started, install and compare had no link to them | Guides added to the main nav on every page, including the 404, with a "Task guides" link on those three pages |
| Guide breadcrumb was plain text, with no structured trail | Breadcrumb links to the hub; `BreadcrumbList` JSON-LD on the hub and guides |
| Three guide titles at 58 to 66 characters, with mixed separators | All three shortened and given one format; every title is now at most 60 characters |

`site/check.py` now enforces each of these: the hub link on every page, every guide linked from the hub, the breadcrumb trail, and the title length.

## Checked, no change needed

- **Documented CLI commands:** ran the first exchange, hold, Claude Code + Codex, local inbox and install-check blocks against this branch's binary in temporary homes. All worked, including `hold check` exiting 3 on a conflict. The real hook install ran only inside a temporary `HOME`.
- **MCP guide arguments:** `send_message` and `fetch_inbox`, with the guide's arguments, succeeded over `serve --stdio`. The remaining MCP steps (`acknowledge`, `claim_request`) were not run.
- **External links:** 20 of 22 return 200. The other two are loopback examples in command output.
- **Descriptions, canonicals, social tags, heading structure:** pass the checker. Four descriptions are 160 to 165 characters and may be truncated in results; left as is.
- **`lastmod`:** not added. Hand-kept dates go stale, and nothing here generates them.

## Needs the owner: outside source

| Finding (production, 2026-11-05) | Effect | Action |
|---|---|---|
| All four guide routes return 404, and live `llms.txt` and sitemap omit them | The guides cannot rank or be linked until deployed | Deploy decision. Do not link `/guides/` in channel posts before then |
| `https://www.toanother.one/` returns 200 rather than redirecting to the apex | Duplicate host; the canonical tags mitigate but don't remove it | Redirect rule, needing DNS/Pages authorization |
| Cloudflare's managed robots content blocks AI crawlers including ClaudeBot and GPTBot, and sets `ai-train=no` | The source `robots.txt` isn't what's served. Those crawlers are told not to fetch `llms.txt` either | Existing decision D3: choose the crawler policy |

## New drafts

[mcp-directories.md](mcp-directories.md), [rust-community.md](rust-community.md) and [bluesky-mastodon.md](bluesky-mastodon.md). Their sources are in [SOURCES.md](SOURCES.md).

## Limits

No Lighthouse, screen-reader or real-device check. Layout was checked by eye at 1440px and at a 390px iframe in headless Chrome, for the home, hub and getting-started pages. No search-engine result or ranking effect is claimed. The rendering of `BreadcrumbList` in search results was not tested.
