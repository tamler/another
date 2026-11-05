# Human decisions

Agents sometimes need a person to choose: ship or wait, Postgres or SQLite, spend the money or not. A **decision** is a structured question with 2 to 6 options. The person answers with one tap on their phone (through the manager bot), over HTTP, or from the CLI. The answer lands in the asking agent's inbox as a reply in the same thread, and the agent is woken if it has a wake target.

Design: [design/03-decisions.md](design/03-decisions.md).

## Asking (agents)

MCP:

```json
{
  "tool": "decision_ask",
  "arguments": {
    "agent": "claude@shop",
    "question": "Ship the refactor behind a flag, or wait for review?",
    "options": [
      {"value": "ship", "label": "Ship behind a flag"},
      {"value": "wait", "label": "Wait for review", "description": "Reviewer is back Monday"}
    ],
    "allow_free_text": true,
    "context_links": ["https://github.com/example/shop/pull/7"],
    "deadline": "4h",
    "on_timeout": "choose_default",
    "default": "wait",
    "lane": "api-refactor"
  }
}
```

CLI:

```bash
another decision ask --agent claude@shop -q "Ship behind a flag, or wait for review?" \
  --option ship="Ship behind a flag" --option wait="Wait for review" \
  --link https://github.com/example/shop/pull/7 --deadline 4h --on-timeout choose_default --default wait
```

**Fields:**
- `options`: 2 to 6. `value` is a single word that comes back as the answer; `label` is what the person sees.
- `human`: who is asked. Defaults to the node setting `another config decider NAME`, else `owner`. The person is registered on demand as an agent with program `human`, so they have an inbox: `another inbox --agent owner`.
- `project`: required for scoping. It is taken from the argument, else the asking agent's project, else the lane's.
- `lane`: looked up within the decision's project only; a lane in another project is `unknown lane`.
- `lane`: the lane shows `needs_decision` until the decision is answered, then returns to `working`.

**When the deadline passes** (the automatic mail comes from the system agent `another`, not from the person):

| `on_timeout` | What happens |
|---|---|
| `keep_open` (default) | One `decision.overdue` event and a reminder to the person; it keeps waiting |
| `choose_default` | The `default` option applies (`answered_by: timeout`); the agent is mailed |
| `cancel` | The decision is cancelled; the agent is mailed to proceed without it |

To withdraw a question, the asking agent calls `decision_cancel` or `another decision cancel <id> --agent …`.

## Answering (the person, through the manager)

The manager bot hears about new decisions through the `needs_decision` webhook (or `events_poll`) and shows the markdown on the phone:

```
**Decision for owner** (open) from claude@shop · shop · lane api-refactor

Ship the refactor behind a flag, or wait for review?

1. Ship behind a flag (`ship`)
2. Wait for review (`wait`) — Reviewer is back Monday
Free text is OK too.
- https://github.com/example/shop/pull/7

Deadline 2026-11-03T18:00:00.000Z, then default wait.

Reply with a number or value. CLI: `another decision answer 01JD… <value>`
```

The manager then relays the answer. Any of these works:

- **Notes:** a short note (up to 280 characters) can go with a chosen option even when free text is off.
- **MCP:** `decision_answer` with `{"decision": "01JD…", "value": "1", "answered_by": "owner"}`. The value can be the option's value or its number.
- **HTTP:** `POST /decisions/{id}/answer` with `{"value": "ship", "text": "…", "answered_by": "owner"}`.
- **CLI**, on the machine itself: `another decision answer 01JD… ship [--text "…"] [--as owner]`.

**What gets recorded:**
- `answered_by`: the person (default: the person asked).
- `relayed_by`: the agent that called the API. Remote callers are always recorded as their token's agent and cannot set it to anyone else. **Only a manager bot (`grok_bot`) or a person (`human`) may relay, and never the agent that asked**: an agent can't approve its own question, whoever it claims answered. These roles are set by the person on the machine (`another agent register NAME --program grok_bot`); `register_agent` over MCP or a hook can't give an agent one, even under a brand-new name, and a refresh keeps the role it has.
- `answered_at`

The asking agent receives a high-importance reply, `[decision] answered: ship (<id>)`, with the value, the label, any text, and who answered and relayed it in the body. Decision mail subjects carry ids, names and values only (they show in the activity feed); the question, labels and text are in the bodies. The request closes, and the lane is released.

Borrowing from a [spend account](SPEND.md) that requires approval asks a decision with the options `approve` and `deny`. Answering `approve` starts the lease.

## Reading

| Surface | Read |
|---|---|
| MCP | `decision_list` (open by default; `status`, `all`, `project`, `lane`, `human`), `decision_show` (with `markdown`) |
| HTTP | `GET /decisions?status=open`, `GET /decisions/{id}` |
| CLI | `another decision list [--all] [--for owner]`, `another decision show <id> [--markdown]` |

Decisions also appear in `another requests` as requests of kind `decision`. `another request claim|complete|cancel` refuses them, so use the decision commands.

## Remote access

With `another serve --auth`:
- A token only sees and answers decisions in projects matching its globs.
- `decision_ask` must act as the token's agent.
- `decision_cancel` is limited to the asking agent.
- Every call is written to the audit log.

**The person** a decision is sent to (`human`, else the `decider` setting) must be a person: a name that belongs to a coding agent is refused (`invalid`), so a decision never lands in an agent's inbox. Register people with `another agent register NAME --program human`, or let the first decision register them. A person registered on demand like that can receive decisions but **can't relay answers** until it is confirmed on the machine with `another agent register NAME --program human`. Otherwise an agent could invent an addressee and answer its own question through it.

**Answer values** match exactly first, then ignoring case, then by option number.

## Events and webhooks

| Event | Webhook filters |
|---|---|
| `decision.created` | `needs_decision`, `request_created` |
| `decision.overdue` | `needs_decision` |
| `decision.answered` | `request_answered` |
| `decision.expired` | `request_answered` |
| `decision.cancelled` | `all` |

Events carry ids and status only (`decision_id`, `project_id`, `lane_id`, `asker_id`, `human_id`, the option count, deadline and timeout rule). The question and option labels stay in the decision and its mail. `decision.cancelled` only says whether a reason was given (`has_reason`); the reason itself stays in the mail and the request's note. A token sees only decisions in its projects: any other decision id gets the same `not_found` as an unknown one, for show, answer and cancel.
