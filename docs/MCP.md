# MCP tools

Generated from the server's own `tools/list` by `scripts/gen-docs.py`; do not edit by hand. CI fails when this file is out of date or a tool has no description. Remote (token) callers can use a subset; see [REMOTE.md](REMOTE.md).

| Tool | What it does |
|---|---|
| [`acknowledge`](#acknowledge) | Mark delivered messages read and (by default) acknowledged |
| [`activity_feed`](#activity_feed) | Read the activity feed: who is doing what (mail, requests, holds, sessions), oldest first |
| [`activity_summarize`](#activity_summarize) | Summarize recent activity (default: last hour): event counts per category, active agents, request and hold totals, highlights, and a short text digest |
| [`agent_kick`](#agent_kick) | Remove an agent now (manager agents only): revoke all its tokens, release its holds and leases, and tell its lanes (a lane it led gets a new lead) |
| [`claim_request`](#claim_request) | Change a request's ownership: claim (default), release, complete, or cancel (creator only) |
| [`daily_summary`](#daily_summary) | Daily summary (default: last 24h): what needs a person (open decisions, stuck lanes, budgets, approvals, stale requests, dead webhooks), lanes moved and closed, decisions, requests, holds and conflicts, spend, active agents |
| [`decision_answer`](#decision_answer) | Answer a decision on a person's behalf (the manager relays what they chose) |
| [`decision_ask`](#decision_ask) | Ask a person a structured question (2-6 options, optional free text, context links, deadline and what happens if nobody answers) |
| [`decision_cancel`](#decision_cancel) | Withdraw a decision you asked (it no longer needs an answer) |
| [`decision_list`](#decision_list) | List decisions (open ones by default), newest first |
| [`decision_show`](#decision_show) | Show one decision, with a short phone-readable markdown rendering |
| [`ensure_project`](#ensure_project) | Register a project by its absolute path (idempotent) |
| [`events_poll`](#events_poll) | Wait for new activity (long-poll): returns as soon as at least one event is available, or after timeout_seconds (at most 30) |
| [`fetch_inbox`](#fetch_inbox) | Fetch an agent's inbox: messages (unread by default), counters, and open requests addressed to or owned by the agent |
| [`heartbeat`](#heartbeat) | Session heartbeat: marks the agent alive, renews its holds, and returns unread/request counts |
| [`hold_paths`](#hold_paths) | Advisory path holds (leases on files/globs within a project) |
| [`holds_by_lane`](#holds_by_lane) | Live path holds grouped by lane: who holds what for which lane |
| [`lane_close`](#lane_close) | Close a lane as done (default) or abandoned, without a report |
| [`lane_create`](#lane_create) | Create a lane: a named unit of work with a charter, a lead agent and a project |
| [`lane_list`](#lane_list) | List lanes (open ones by default), newest activity first |
| [`lane_report`](#lane_report) | Post a structured lane report (summary, progress, status, links, artifacts) |
| [`lane_show`](#lane_show) | Show one lane: charter, status, lead, links and the latest reports |
| [`lane_update`](#lane_update) | Update a lane's status, charter or lead, and link threads, requests, holds or messages to it |
| [`orphans_list`](#orphans_list) | List orphan candidates: registered resources whose leases lapsed, and (by default) listening processes that no registered resource accounts for |
| [`register_agent`](#register_agent) | Register or update an agent identity and mark it active |
| [`resource_borrow`](#resource_borrow) | Borrow a shared resource (reference-counted lease with TTL) |
| [`resource_find`](#resource_find) | Find registered resources before starting a server or job |
| [`resource_heartbeat`](#resource_heartbeat) | Renew your resource leases (all, or the listed ones) |
| [`resource_reap`](#resource_reap) | Request a reap |
| [`resource_register`](#resource_register) | Register a running resource (model server, dev server, port, GPU, container, long job) and take its owner lease |
| [`resource_release`](#resource_release) | Release your lease on a resource (ownership passes to the oldest borrower) |
| [`send_message`](#send_message) | Send a message to other agents, or reply in a thread with reply_to |
| [`spend_account_upsert`](#spend_account_upsert) | Create or update a spend account (local callers only): kind, unit, monthly budget with reset day, hourly rate, approval rule, project |
| [`spend_accounts`](#spend_accounts) | List spend accounts (budgets, units, rates, approval rules) |
| [`spend_borrow`](#spend_borrow) | Start using a costed account by the hour (e.g |
| [`spend_leases`](#spend_leases) | List spend leases (open ones unless all=true) |
| [`spend_record`](#spend_record) | Record an amount spent against an account (for a lane or project) |
| [`spend_release`](#spend_release) | End your spend lease (its cost is recorded) or withdraw a pending one |
| [`spend_report`](#spend_report) | Spend report: per account spent, accrued (open leases), budget, remaining, projected and status for the period, plus totals by lane and agent and a short burn-down in markdown |
| [`threads`](#threads) | Read a whole thread by thread_id, or list the threads an agent is part of |
| [`webhook_list`](#webhook_list) | List webhook subscriptions (remote callers see their token's only) |
| [`webhook_remove`](#webhook_remove) | Remove a webhook subscription; pending deliveries are dropped |
| [`webhook_subscribe`](#webhook_subscribe) | Subscribe a URL to events (HMAC-SHA256 signed POSTs with retries) |

## acknowledge

Mark delivered messages read and (by default) acknowledged.

| Argument | Type | Required | Description |
|---|---|---|---|
| `ack` | boolean, optional |  | true: mark read and acknowledged (default); false: only mark read. |
| `agent` | string | yes |  |
| `message_ids` | array | yes |  |

## activity_feed

Read the activity feed: who is doing what (mail, requests, holds, sessions), oldest first. Items have one-line summaries and ids, never message bodies. Pass next_since back as since to page forward.

| Argument | Type | Required | Description |
|---|---|---|---|
| `kinds` | array |  | Only these categories. |
| `limit` | integer, optional |  | Maximum items (default 50, max 500). |
| `since` | string, optional |  | Start point: duration (30m, 2h, 1d), RFC 3339 time, or an event id cursor. Omit for the most recent items. |

## activity_summarize

Summarize recent activity (default: last hour): event counts per category, active agents, request and hold totals, highlights, and a short text digest.

| Argument | Type | Required | Description |
|---|---|---|---|
| `since` | string, optional |  | Window start: duration (30m, 2h, 1d), RFC 3339 time, or event id. Default 1h. |

## agent_kick

Remove an agent now (manager agents only): revoke all its tokens, release its holds and leases, and tell its lanes (a lane it led gets a new lead). Remote managers can only kick agents in their token's projects; people can't be kicked.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes | The acting manager agent (program grok_bot; remote callers: the token's agent). |
| `reason` | string, optional |  | Why (kept on the agent's membership record, not in events). |
| `target` | string | yes | The agent to remove (name or name@project). |

## claim_request

Change a request's ownership: claim (default), release, complete, or cancel (creator only). Stale requests may be claimed by anyone.

| Argument | Type | Required | Description |
|---|---|---|---|
| `action` | object |  | claim (default) \| release \| complete \| cancel. |
| `agent` | string | yes |  |
| `due_in_seconds` | integer, optional |  | On claim: new due time, in seconds from now. |
| `note` | string, optional |  | Completion or cancellation note; on completion it is mailed to the requester. |
| `request_id` | string | yes |  |

## daily_summary

Daily summary (default: last 24h): what needs a person (open decisions, stuck lanes, budgets, approvals, stale requests, dead webhooks), lanes moved and closed, decisions, requests, holds and conflicts, spend, active agents. JSON plus a short phone-readable `markdown`.

| Argument | Type | Required | Description |
|---|---|---|---|
| `since` | string, optional |  | Duration like `24h` (default), an RFC 3339 time, or an event id. |

## decision_answer

Answer a decision on a person's behalf (the manager relays what they chose). Records who answered and who relayed it, mails the answer to the asking agent and wakes it if it has a wake target.

| Argument | Type | Required | Description |
|---|---|---|---|
| `answered_by` | string, optional |  | The person who decided (default: the person asked). |
| `decision` | string | yes | Decision id (or its request id). |
| `relayed_by` | string | yes | The agent relaying the answer; remote callers: always the token's agent. |
| `text` | string, optional |  | Free text (only if the decision allows it). |
| `value` | string, optional |  | The chosen option value (or its 1-based number). |

## decision_ask

Ask a person a structured question (2-6 options, optional free text, context links, deadline and what happens if nobody answers). The answer arrives in your inbox as a reply in the same thread. With a lane, the lane shows needs_decision until it is answered.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes | The asking agent. |
| `allow_free_text` | boolean |  | Accept a free-text answer as well as the options. |
| `context_links` | array |  | URLs or ids that help decide (PR, CI run, lane). |
| `deadline` | string, optional |  | Duration like `2h` or an RFC 3339 time. |
| `default` | string, optional |  | The option value applied by choose_default. |
| `human` | string, optional |  | Who to ask (default: the node's decider, `jacob` unless configured). |
| `lane` | string, optional |  | Lane id or name: it shows needs_decision until this is answered. |
| `on_timeout` | object |  | At the deadline: choose_default (needs `default`), cancel, or keep_open (default). |
| `options` | array | yes | 2 to 6 options: `value` is a short token (`ship`), `label` what the person sees. |
| `project` | string, optional |  | Project path or slug (default: the lane's, else yours). |
| `question` | string | yes | The question, in a sentence or two. |

## decision_cancel

Withdraw a decision you asked (it no longer needs an answer).

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes | The asking agent. |
| `decision` | string | yes |  |
| `reason` | string, optional |  |  |

## decision_list

List decisions (open ones by default), newest first.

| Argument | Type | Required | Description |
|---|---|---|---|
| `all` | boolean |  | Every status. |
| `human` | string, optional |  |  |
| `lane` | string, optional |  |  |
| `limit` | integer, optional |  |  |
| `project` | string, optional |  |  |
| `status` | object |  | open (default), answered, expired, cancelled. |

## decision_show

Show one decision, with a short phone-readable markdown rendering.

| Argument | Type | Required | Description |
|---|---|---|---|
| `decision` | string | yes | Decision id (or its request id). |

## ensure_project

Register a project by its absolute path (idempotent). Returns its slug.

| Argument | Type | Required | Description |
|---|---|---|---|
| `path` | string | yes | Absolute path of the project (repository) root. |

## events_poll

Wait for new activity (long-poll): returns as soon as at least one event is available, or after timeout_seconds (at most 30). Pass next_since back as since. Items are one-line summaries with ids, filtered to what your token may see.

| Argument | Type | Required | Description |
|---|---|---|---|
| `kinds` | array |  | Only these categories (empty = all). |
| `since` | string, optional |  | Cursor (`next_since` from a previous call), a duration like `15m`, or an RFC 3339 time. Omit to wait for events from now on. |
| `timeout_seconds` | integer, optional |  | Seconds to wait for at least one event (default 25, at most 30). |

## fetch_inbox

Fetch an agent's inbox: messages (unread by default), counters, and open requests addressed to or owned by the agent.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `limit` | integer, optional |  | Maximum messages to return (default 20). |
| `unread_only` | boolean, optional |  | Only unread messages (default true). |
| `urgent_only` | boolean |  | Only high/urgent messages. |

## heartbeat

Session heartbeat: marks the agent alive, renews its holds, and returns unread/request counts. With end=true, releases all holds and marks the agent offline.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `end` | boolean |  | Session is ending: release all holds and mark the agent offline. |
| `keep_resources` | boolean |  | With end: keep resource leases (for servers meant to outlive the session). |
| `start` | boolean |  | Session is starting: records a session.started event. |

## hold_paths

Advisory path holds (leases on files/globs within a project). acquire is all-or-nothing and reports conflicts; check reports conflicts without acquiring; renew, release and list manage your holds.

| Argument | Type | Required | Description |
|---|---|---|---|
| `action` | object |  | acquire (default) \| check \| renew \| release \| list. |
| `agent` | string | yes |  |
| `exclusive` | boolean, optional |  | Exclusive holds (default) conflict with any overlapping hold; shared ones only with exclusive. |
| `hold_ids` | array |  | Renew/release only these holds (default: all of the agent's holds). |
| `lane` | string, optional |  | Lane id or name the holds are for (acquire). Default: the one open lane you lead in this project. Overlaps with holds in the same lane do not block (they are warnings). |
| `paths` | array |  | Paths or globs relative to the project root (acquire/check). |
| `project` | string, optional |  | Project absolute path or slug (required for acquire/check; filters list). |
| `reason` | string, optional |  |  |
| `ttl_seconds` | integer, optional |  | Lease length in seconds (default 1800, clamped to 60..14400). |

## holds_by_lane

Live path holds grouped by lane: who holds what for which lane.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string, optional |  | Only this agent's holds. |
| `project` | string, optional |  | Project absolute path or slug. |

## lane_close

Close a lane as done (default) or abandoned, without a report.

| Argument | Type | Required | Description |
|---|---|---|---|
| `abandon` | boolean |  | Close as abandoned instead of done. |
| `agent` | string | yes |  |
| `lane` | string | yes | Lane id or name. |
| `outcome` | string, optional |  |  |

## lane_create

Create a lane: a named unit of work with a charter, a lead agent and a project. Status starts at planned.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes | Acting agent (the creator, usually the manager). |
| `charter` | string |  | The brief: what to do, constraints, definition of done. |
| `lead` | string | yes | Agent that leads the work. |
| `name` | string | yes | 1-64 chars: letters, digits, '-', '_', '.'; unique among open lanes. |
| `project` | string | yes | Project absolute path or slug (required). |
| `status` | object |  | Initial status (default planned). |

## lane_list

List lanes (open ones by default), newest activity first.

| Argument | Type | Required | Description |
|---|---|---|---|
| `include_closed` | boolean |  | Include done and abandoned lanes. |
| `lead` | string, optional |  |  |
| `project` | string, optional |  | Project absolute path or slug. |
| `status` | object |  |  |

## lane_report

Post a structured lane report (summary, progress, status, links, artifacts). kind=done closes the lane with an outcome and optional PR link. The report is also mailed to the lane's counterpart (creator or lead) in the lane thread.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `artifacts` | array |  | Files or URLs produced. |
| `kind` | object |  | status (default) or done. A done report closes the lane. |
| `lane` | string | yes | Lane id or name. |
| `links` | array |  | URLs or ids (CI runs, threads, issues). |
| `outcome` | string, optional |  | Done reports: what was delivered (required for done). |
| `pr_url` | string, optional |  | Done reports: pull request URL. |
| `progress` | integer, optional |  | Percent complete, 0-100. |
| `status` | object |  | Optional new open status, e.g. blocked or needs_decision. |
| `summary` | string | yes | What happened (first line is used as the mail subject). |

## lane_show

Show one lane: charter, status, lead, links and the latest reports.

| Argument | Type | Required | Description |
|---|---|---|---|
| `lane` | string | yes | Lane id or name. |

## lane_update

Update a lane's status, charter or lead, and link threads, requests, holds or messages to it.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `charter` | string, optional |  |  |
| `lane` | string | yes | Lane id or name. |
| `lead` | string, optional |  |  |
| `link_holds` | array |  |  |
| `link_messages` | array |  |  |
| `link_requests` | array |  |  |
| `link_threads` | array |  |  |
| `status` | object |  | New open status (planned, working, blocked, needs_decision); finish with lane_report kind=done or lane_close. |

## orphans_list

List orphan candidates: registered resources whose leases lapsed, and (by default) listening processes that no registered resource accounts for.

| Argument | Type | Required | Description |
|---|---|---|---|
| `scan` | boolean, optional |  | Also scan for listening processes no registered resource accounts for (default true). |

## register_agent

Register or update an agent identity and mark it active. Call at session start.

| Argument | Type | Required | Description |
|---|---|---|---|
| `description` | string, optional |  | Short description of what this agent is working on. |
| `model` | string, optional |  | Model identifier, if known. |
| `name` | string | yes | Agent name, unique on this machine (letters, digits, '-', '_', '.'). |
| `program` | object | yes | The program running this agent. |
| `project` | string, optional |  | Absolute project path; lets the same name exist per project as `name@project`. |

## resource_borrow

Borrow a shared resource (reference-counted lease with TTL). Exclusive resources refuse; borrowing a lapsed resource adopts it; borrowing again renews.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `resource` | string | yes | Resource id, or `kind:name` such as `model_server:ollama@11434`. |
| `ttl_seconds` | integer, optional |  |  |

## resource_find

Find registered resources before starting a server or job. Returns live and lapsed resources with owner, leases (refcount), endpoint and status.

| Argument | Type | Required | Description |
|---|---|---|---|
| `include_released` | boolean |  |  |
| `kind` | object |  |  |
| `local_only` | boolean |  | Only resources owned by this node (default includes other nodes' resources). |
| `project` | string, optional |  | Project absolute path or slug. |
| `query` | string, optional |  | Case-insensitive substring of name, endpoint or purpose (e.g. `ollama`, `5173`). |

## resource_heartbeat

Renew your resource leases (all, or the listed ones).

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `resources` | array |  | Renew only these (ids or `kind:name`); default: all of the agent's leases. |

## resource_reap

Request a reap. This never terminates anything: it returns the reap plan and safety checks, records the request in the activity feed, and gives the command a human must run to confirm.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `grace_seconds` | integer, optional |  | Grace period to evaluate against (default 600 seconds). |
| `reason` | string, optional |  | Why it should be reaped (shown to the human). |
| `target` | string | yes | Resource id, `kind:name`, or `pid:N` for an unregistered listener. |

## resource_register

Register a running resource (model server, dev server, port, GPU, container, long job) and take its owner lease. Call resource_find first: if it is already registered, borrow it instead of starting a duplicate.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `endpoint` | string, optional |  | How to reach it, e.g. `http://127.0.0.1:11434`. |
| `exclusive` | boolean |  | Exclusive resources cannot be borrowed by others (default: shared). |
| `kind` | object | yes |  |
| `metadata` | any |  | Free-form object: model, backend (metal\|mlx\|cpu\|cuda), estimated_bytes, cmdline, ... |
| `name` | string | yes | Identity on this machine, e.g. `ollama@11434`, `vite@5173`, `tcp:127.0.0.1:5432`, `metal:0`. |
| `pid` | integer, optional |  | Process id (enables orphan matching and human-approved reaping). |
| `port` | integer, optional |  | Listening port (derived from name/endpoint when omitted). |
| `project` | string, optional |  | Project absolute path or slug. |
| `purpose` | string, optional |  |  |
| `ttl_seconds` | integer, optional |  | Lease length in seconds (default 1800, clamped to 60..86400); renew with resource_heartbeat. |

## resource_release

Release your lease on a resource (ownership passes to the oldest borrower). Owners pass stopped=true after shutting it down.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `resource` | string | yes | Resource id, or `kind:name`. |
| `stopped` | boolean |  | Owner only: the resource has been shut down; end every lease and mark it released. |

## send_message

Send a message to other agents, or reply in a thread with reply_to. Set create_request to open a claimable request.

| Argument | Type | Required | Description |
|---|---|---|---|
| `ack_required` | boolean |  | Ask recipients to explicitly acknowledge. |
| `body` | string | yes |  |
| `cc` | array |  |  |
| `create_request` | boolean |  | Also open a request (owned work item) that recipients can claim. |
| `due_in_seconds` | integer, optional |  | For requests: mark stale if not done within this many seconds. |
| `from` | string | yes | Sender: your agent name (or `name@project`). |
| `importance` | object |  | low \| normal \| high \| urgent (default normal). |
| `reply_all` | boolean |  | When replying without explicit recipients, reply to everyone on the original. |
| `reply_to` | string, optional |  | Message id to reply to; the reply joins its thread. |
| `subject` | string, optional |  | Required for new messages; replies default to "Re: <original subject>". |
| `to` | array |  | Recipients. Optional when replying (defaults to the original sender). |

## spend_account_upsert

Create or update a spend account (local callers only): kind, unit, monthly budget with reset day, hourly rate, approval rule, project.

| Argument | Type | Required | Description |
|---|---|---|---|
| `approver` | string, optional |  |  |
| `budget` | number, optional |  |  |
| `cost_per_hour` | number, optional |  |  |
| `description` | string, optional |  |  |
| `kind` | object |  | gpu_pod, api_credits, plan_reset, server, domain (required to create). |
| `name` | string | yes | Account name (created if new, else updated). |
| `period` | object |  | monthly (default) or none. |
| `project` | string, optional |  | Project path or slug; omit for a shared account. |
| `requires_approval` | boolean, optional |  |  |
| `reset_day` | integer, optional |  | 1-28 (default 1). |
| `unit` | string, optional |  | USD, credits, resets, hours ... (required to create). |

## spend_accounts

List spend accounts (budgets, units, rates, approval rules).

## spend_borrow

Start using a costed account by the hour (e.g. a GPU pod). If the account requires approval, this asks a person (a decision) and the lease starts only when approved; check decision_show / your inbox. Renewed by heartbeat; release it when done.

| Argument | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes |  |
| `agent` | string | yes |  |
| `cost_per_hour` | number, optional |  | Rate per hour (default: the account's). |
| `hours` | number, optional |  | Expected hours, shown in the approval question. |
| `lane` | string, optional |  |  |
| `note` | string, optional |  | What it is for. |
| `ttl_seconds` | integer, optional |  | Lease length, renewed by heartbeat (default 3600). |

## spend_leases

List spend leases (open ones unless all=true).

| Argument | Type | Required | Description |
|---|---|---|---|
| `all` | boolean |  | Include ended and denied leases. |

## spend_record

Record an amount spent against an account (for a lane or project).

| Argument | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account name. |
| `agent` | string | yes | The agent that spent it. |
| `amount` | number | yes | In the account's unit (USD, credits, resets, ...). |
| `lane` | string, optional |  |  |
| `note` | string, optional |  |  |
| `project` | string, optional |  |  |

## spend_release

End your spend lease (its cost is recorded) or withdraw a pending one.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string | yes |  |
| `lease_id` | string | yes |  |

## spend_report

Spend report: per account spent, accrued (open leases), budget, remaining, projected and status for the period, plus totals by lane and agent and a short burn-down in markdown.

| Argument | Type | Required | Description |
|---|---|---|---|
| `account` | string, optional |  |  |
| `agent` | string, optional |  |  |
| `lane` | string, optional |  |  |
| `period` | string, optional |  | current (default), previous, or YYYY-MM. |

## threads

Read a whole thread by thread_id, or list the threads an agent is part of.

| Argument | Type | Required | Description |
|---|---|---|---|
| `agent` | string, optional |  | Otherwise list threads this agent is part of. |
| `limit` | integer, optional |  |  |
| `thread_id` | string, optional |  | Return every message in this thread. |

## webhook_list

List webhook subscriptions (remote callers see their token's only). Secrets are never shown.

## webhook_remove

Remove a webhook subscription; pending deliveries are dropped.

| Argument | Type | Required | Description |
|---|---|---|---|
| `webhook_id` | string | yes |  |

## webhook_subscribe

Subscribe a URL to events (HMAC-SHA256 signed POSTs with retries). Returns the signing secret once. Filters: lane_status, lane_report, needs_decision, request_created, request_answered, hold_conflict, lease_expiring, budget, all.

| Argument | Type | Required | Description |
|---|---|---|---|
| `all_projects` | boolean |  | Every project (local callers only). |
| `description` | string, optional |  |  |
| `filters` | array | yes | Event filters: lane_status, lane_report, needs_decision, request_created, request_answered, hold_conflict, lease_expiring, budget, all. (`daily_summary` is accepted but never delivered to token webhooks: it covers the whole node.) |
| `projects` | array |  | Project path or slug globs. Remote callers default to (and may only narrow) their token's globs. |
| `url` | string | yes | `https://` URL (plain http only to loopback). Redirects are not followed. |
