# Design note 03: Human decisions

Status: implemented in phase 3. Builds on [01-lanes](01-lanes.md) and [02-push](02-push.md).

## Problem

Agents hit questions only a person can answer, such as "ship behind a flag or wait for review?". Today they mail the manager in free text, and the answer comes back the same way, so nothing records that a question is pending, what the choices were, who decided, or what happens if nobody answers. The manager bot needs structured questions it can put on a phone and answer with one tap. The asking agent needs the answer in its inbox, and needs to be woken.

## Model

A **decision** is a request of kind `decision` addressed to a human.

| Field | Notes |
|---|---|
| `question` | 1–2000 chars |
| `options` | 2–6 `{value, label, description?}`. Values are short tokens (`ship`, `wait`), unique; labels are what the phone shows. |
| `allow_free_text` | Whether an answer may be text instead of, or as well as, an option. When off, a **short note** (≤ 280 chars) may still accompany a chosen option, never replace it (maintainer's decision). |
| `context_links` | URLs or ids (PRs, CI runs, lanes) for the person deciding |
| `deadline` | Optional; an RFC 3339 time or a duration (`2h`) |
| `on_timeout` | What happens at the deadline: `choose_default` (needs `default`, one of the option values), `cancel`, or `keep_open` (the default: one `decision.overdue` reminder, then keep waiting) |
| `human` | Who is asked. Defaults to the `decider` **node setting** (`another config decider NAME`), else `owner`. There is no environment variable (maintainer's decision): the setting lives in the node database, so every process on the node agrees. |
| `project` | Required, because token scope is checked against it. Taken from the argument, else the asking agent's project, else the lane's. It is resolved and scope-checked **before** any lane lookup. |
| `lane` | Optional. While the decision is open, the lane is `needs_decision`. Looked up **within the decision's project only** (id or name): a lane elsewhere, out of scope or missing is the same `unknown lane` error. |
| status | `open`, `answered`, `expired`, `cancelled` |
| answer | `value` and/or `text`, `answered_by` (the person), `relayed_by` (the agent that called the API, if any), `answered_at` |

### Reusing mail and requests

- **The human is an agent.** The human is registered on demand as an agent with the new program `human`, with no project. That gives them an address (`owner`) and an inbox: `another inbox --agent owner` lists what is waiting.
- **Asking** sends a message from the asking agent to the human. The subject is `[decision] <question>`, and the body lists the options, links and deadline. It also writes a `requests` row of kind `decision`, so `another requests` shows the decision next to other work.
  - The row is written directly, without a `request.created` event, so webhooks fire once, on `decision.created`.
  - Its `due_at` stays empty, because the deadline belongs to the decision, not to the stale-request sweep.
- **Answering** is a reply in the same thread, from the human to the asking agent, with high importance:
  - subject `[decision] <option label>`;
  - the body has the value, the label, any free text, and who answered or relayed it.
  - Because it is mail, the answer reaches the agent through the hooks it already uses (the digest at the next prompt or session start).
  - If the agent has a wake target (`another wake set`), the answering CLI or server also runs `another wake` for it.
- The request row is closed with the decision: `done` when answered, `cancelled` when it expires or is cancelled.
- `claim_request` and `another request …` refuse kind-`decision` requests with `conflict`, pointing to the decision commands, so the two can't drift apart.
- **Lane link:** asking with a lane moves an open lane to `needs_decision`. When the lane's last open decision closes, and the lane is still `needs_decision`, it moves back to `working`.

### Expiry

The existing sweeper (every 30 s in `serve`, and before most writes) handles deadlines. Its mail is sent by the reserved **system agent `another`** (registered on first use; the name can't be registered by anyone else), never in the person's name (maintainer's decision):
- `choose_default`: status `expired`, the answer is the default with `answered_by: "timeout"`, and the asking agent is mailed "no answer by the deadline; default X applies".
- `cancel`: status `expired` with no answer; the asking agent is mailed "no answer by the deadline; decision cancelled".
- `keep_open`: one `decision.overdue` event and a reminder in the human's inbox; it stays open.

## Surfaces

**MCP:**
- `decision_ask`: the asking agent.
- `decision_list`: open by default.
- `decision_show`
- `decision_answer`: the manager relays a person's answer.
- `decision_cancel`: the asking agent withdraws a question.

**HTTP**, for a phone shortcut or a bot without MCP:
- `GET /decisions?status=open`
- `GET /decisions/{id}`
- `POST /decisions/{id}/answer` with body `{"value": "...", "text": "...", "answered_by": "..."}`

**CLI**, the person's local interface:
- `another decision ask …`
- `another decision list`
- `another decision show <id> [--markdown]`
- `another decision answer <id> <value> [--text …] [--as NAME]`
- `another decision cancel <id>`

**Events**, all with `project_id`, so scope filtering and webhooks work. They carry ids and status only, never the question, option labels, answer text or cancellation reason (those stay in the table and the mail, whose subjects are content-free too):

| Event | Fires when |
|---|---|
| `decision.created` | a decision is asked |
| `decision.answered` | someone answers |
| `decision.expired` | the deadline passes with `choose_default` or `cancel` |
| `decision.overdue` | the deadline passes with `keep_open` |
| `decision.cancelled` | the asking agent withdraws it |

**Webhook filters:**
- `needs_decision` now covers `decision.created` and `decision.overdue`.
- `request_created` covers `decision.created`.
- `request_answered` covers `decision.answered` and `decision.expired`.

**Markdown:** `decision_markdown` renders a decision for a phone: the question, numbered options with their values, links, the deadline, and the answer command.

## Security

- **Remote callers** (`--auth`):
  - `decision_ask` must act as the token's agent, in a project within the token's globs.
  - `decision_list` and `decision_show` only cover decisions in the token's projects.
  - `decision_answer` requires the decision's project to be in scope.
  - The token's agent is recorded as `relayed_by`, so a relayed answer can always be traced to the token that sent it. `answered_by` is what the relaying agent says, which is a claim, not proof of identity.
  - **No self-approval:** the relaying agent may not be the asking agent, and must be a manager bot (`grok_bot`) or a person (`human`). MCP `decision_answer` always names its relaying agent; only the local CLI (the person at the machine) answers without one. The same rule protects spend approvals.
  - `decision_list` filters (`project`, `lane`) resolve within the caller's scope: hidden and missing give the same `unknown project` / `unknown lane` error.
  - `decision_cancel` is limited to the asking agent.
- The HTTP endpoints follow the same rules, and everything is audited.
- Answer text is stored and mailed to the asking agent, never placed in a wake message (wake messages only list subjects).

## Migration

Migration 8, additive:
- a `decisions` table;
- a `requests.kind` column, `TEXT NOT NULL DEFAULT 'task'`.

## Out of scope

- Several people per decision, or quorum.
- Push to the human directly. The manager bot does that, triggered by the `needs_decision` webhook.
- Editing a decision after it is asked: cancel it and ask again.
