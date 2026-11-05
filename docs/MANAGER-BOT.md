# Running a manager bot on Another

This guide is for a bot (Grok Bot, a Slack or Telegram bot, a cron job) that oversees coding agents for one person. It covers:
- giving it a token;
- subscribing to the right webhooks;
- which MCP tools to call, and when.

Everything here works over `another serve --auth` behind a tunnel ([REMOTE.md](REMOTE.md)).

## 1. Token and server

```bash
# A token that acts as the manager, limited to the projects it manages
another token create --agent manager --project ~/Projects/shop --project ~/Projects/ml
# Serve with auth on loopback, then expose it with `tailscale serve` or a tunnel
another serve --auth --bind 127.0.0.1:7467
```

**The token's globs bound everything the bot sees:** lanes, events, decisions, holds, spend and the daily summary. Events from other projects never reach it. For shared spend accounts, it sees only its projects' share of the spending, plus the whole account's remaining budget, read-only ([SPEND.md](SPEND.md#remote-access)).

## 2. Subscribe once

One webhook covers what the person should hear about:

```json
{ "tool": "webhook_subscribe",
  "arguments": { "url": "https://bot.example.com/another",
                 "filters": ["needs_decision", "lane_status", "request_answered", "hold_conflict", "budget"] } }
```

- **Store the `secret`** from the response, and verify `X-Another-Signature` on every delivery ([PUSH.md](PUSH.md#verifying-signatures)).
- **Branch on `X-Another-Event` and `filter`:**

| Event | Why it arrived | What the bot does |
|---|---|---|
| `decision.created`, `decision.overdue` | `needs_decision` | Call `decision_show`, and send its `markdown` to the person with one button per option |
| `lane.status_changed` to `blocked` / `needs_decision` | `lane_status`, `needs_decision` | Call `lane_show` and send the latest report |
| `lane.closed` | `lane_status` | Post "done", with the outcome and PR link |
| `decision.answered`, `decision.expired`, `request.completed` | `request_answered` | Usually nothing, or a short ack |
| `hold.conflict`, `lane.hold_conflict` | `hold_conflict` | Call `holds_by_lane`, and tell the leads or ask the person who yields |
| `spend.budget_threshold`, `spend.approval_requested` | `budget` | Send the alert; an approval request is also a decision |
| `resource.lease_expiring` | `lease_expiring` (optional) | Remind the owner |

**Without a public URL**, use `events_poll` (MCP long-poll, up to 30 s) or `GET /events/stream` (SSE) instead. Both apply the same scoping.

## 3. Relay the person's answers

When the person taps an option:

```json
{ "tool": "decision_answer",
  "arguments": { "decision": "01JD…", "value": "2", "answered_by": "owner", "text": "optional note" } }
```

or `POST /decisions/{id}/answer`.

- **`value`** can be the option's value or its number.
- **Who is recorded:** the bot is recorded as `relayed_by`, and `answered_by` names the person.
- **Delivery:** the asking agent gets the answer in its inbox, and is woken if it has a wake target.
- **Spend approvals** are decisions with `approve` and `deny`. Approving starts the lease.

## 4. Run lanes

| Moment | Tool |
|---|---|
| New piece of work | `lane_create` (name, project, lead, charter) |
| Checking in | `lane_list`, `lane_show` (reports, links, live holds) |
| Changing course | `lane_update` (status, charter, lead, links), or `lane_report` to the lead (mailed in the lane thread) |
| Who is touching what | `holds_by_lane` |
| Done or dropped | `lane_close` (agents usually close their own with `lane_report kind=done`) |

## 5. Daily digest

Once a day, or when the person asks "what's up?":

```json
{ "tool": "daily_summary", "arguments": { "since": "24h" } }
```

Post the `markdown` field as-is; it is written to fit a phone screen. For a plain HTTP bot, `GET /summary?format=markdown` returns the same text. The `attention` list is what needs the person:
- open decisions;
- stuck lanes;
- budgets over, or on pace to exceed;
- pending approvals;
- stale requests;
- dead webhooks.

If `truncated` is true, a section hit the 20,000-event cap and its counts may be low, or more than 500 decisions are open and the list shows the oldest 500 (`open_total` stays complete). People (program `human`) are listed under `people_active`, apart from `agents_active`.

**Or let the node send it.** The person can turn on a daily rollup on the machine: `another summary schedule --daily 08:30 --to mail:owner` (mail from the `another` system agent), or `--to webhook`. The webhook form goes only to webhooks created locally for all projects with the `daily_summary` filter, because it covers the whole node; a token's webhooks never receive it. It is sent while `another serve` runs. Pulling `daily_summary` keeps working either way.

## 6. Spend

| Need | Tool |
|---|---|
| Budget status | `spend_report` (with `markdown`), or `GET /spend` |
| Record a cost the bot knows about | `spend_record` (attributed to a lane or project) |

Accounts are set up by the person on the machine (`another spend account add …`). The bot can't create them.

## Agents' side (for their instructions)

Put this in each project's `AGENTS.md` or `CLAUDE.md`:

- Ask with `decision_ask` (2–6 options; set `lane`). Keep working on something else; the answer arrives as mail.
- Report progress with `lane_report`. With `another hook install --lane-reports` this happens automatically.
- Hold files before editing, or install the pre-edit hook (`another hook install --pre-edit`).
- Borrow costed resources with `spend_borrow`, release them with `spend_release`, and record other costs with `spend_record`.
