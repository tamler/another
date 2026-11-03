# Changelog

All notable changes to Another. Unreleased work is listed under the next version.

## Unreleased

### Added
- **Lanes** ([design](docs/design/01-lanes.md), [guide](docs/LANES.md)):
  - Named units of work with a charter, lead agent, project, status (`planned`, `working`, `blocked`, `needs_decision`, `done`, `abandoned`), linked threads, requests, holds and messages, and structured reports (summary, progress, links, artifacts) plus a DONE report with outcome and PR link.
  - `another lane create|list|show|update|report|done|close|export` and MCP tools `lane_create`, `lane_list`, `lane_show`, `lane_update`, `lane_report`, `lane_close`.
  - Reports are mailed to the lane's counterpart in the lane thread.
  - `lane export` writes the old `.lanes`-style STATUS and DONE markdown.
  - Opt-in Stop hook (`another hook install --lane-reports`) for automatic reports from Claude Code, Codex, Grok and Cursor.
  - Remote callers only see and change lanes in their token's project globs. Lanes outside them look like unknown lanes (`not_found`), and their events are left out of the remote activity feed.
  - Lane names are unique per project; address lanes as `name` or `name@project`.
  - Lane events carry ids, names, project and status only (no charter, report text, progress or links). Links must belong to the lane's project.
  - The Stop hook redacts likely secrets (`ano_`, `sk-`, `ghp_`, long hex/base64, …) from the agent's last message before storing it.
- CI also runs on pull requests that target non-`main` branches, so stacked PRs are checked.

### Database
- Migration 6 adds the `lanes`, `lane_links` and `lane_reports` tables. It is additive; existing databases upgrade in place.
