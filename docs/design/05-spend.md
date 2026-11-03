# Design note 05: Spend ledger

Status: implemented in phase 5. Builds on [03-decisions](03-decisions.md) (approval gate) and [02-push](02-push.md) (events, scope).

## Problem

Agents spend real money and scarce allowances: GPU pods by the hour, API credits, plan resets (such as a Codex full reset), servers and domains. Nothing records who spent what, for which lane, against which budget, or stops an agent from spinning up a pod without asking. The manager needs a burn-down it can read on a phone, and a way to require approval before costly borrowing.

## Model

**Account**: something with a budget.

| Field | Notes |
|---|---|
| `name` | Unique, e.g. `claude-api` |
| `kind` | `gpu_pod`, `api_credits`, `plan_reset`, `server` or `domain` |
| `unit` | Free text: `USD`, `credits`, `resets`, `hours` |
| `budget` | Optional; per period, in `unit` |
| `period` | `monthly` (the default) or `none` (lifetime) |
| `reset_day` | 1–28 (default 1). The monthly period runs from that day in one month to the same day the next month, in UTC. |
| `cost_per_hour` | Optional default for leases |
| `requires_approval` | Borrowing creates a decision instead of a lease |
| `approver` | Who approves (default: the decider from note 03) |
| `project` | Optional. With a project, the account belongs to it. Without one, it is shared, like an API key used everywhere. |

Accounts are created and changed locally only, through the CLI or local MCP. Remote tokens can't create or change them.

**Entry**: an amount spent in the account's unit, at a time, by an agent (optional), for a lane (optional), with a note.
- The entry's project is the lane's project, else the account's, else the agent's.
- `source` is `manual`, or `lease` when the entry comes from a lease.

**Lease**: an agent using a costed account by the hour.
- Fields: owner (agent), lane, `cost_per_hour` (from the argument, else the account), TTL (default 1 h, 1 min–24 h).
- The lease is renewed by `spend_lease renew` or the agent's session heartbeat, like holds and resource leases.
- Ending it (release, or lapsing at its expiry) writes one `lease` entry for `hours × cost_per_hour`.
- While a lease is open, its cost so far is reported as **accrued**.
- Statuses: `pending_approval`, `active`, `ended`, `denied`.

**Approval gate:** borrowing from an account with `requires_approval` does not start the lease.
1. It creates a pending lease and asks a **decision** (note 03) of the approver: "Approve codex@ml borrowing gpu-a100 at 2.10 USD/h for 4 h (about 8.40 USD)?", with options `approve` and `deny`. The decision's project comes from the lane, else the account, else the agent.
2. Answering `approve` starts the lease. Its clock starts then, not at the request.
3. `deny`, expiry or cancelling marks it `denied`.
4. Either way, the decision's answer mail tells the agent the outcome.

## Reports

`Store::spend_report(query, scope)` returns, for the chosen period:
- **Per account:**
  - `spent` (entries) and `accrued` (open leases);
  - `budget`, `remaining` and `used_pct`;
  - days elapsed and total, and `projected` (straight-line extrapolation to the period end);
  - `daily_left`: the remaining budget divided by the days left;
  - `status`: `ok`, `warning` (projected or used at 80% or more) or `over`.
- **Totals by lane and by agent**, per account, since units differ.
- **Burn-down markdown** that fits on a phone, one line per account:
  ```
  claude-api  142.10 / 200 USD  71%  ███████░░░  day 21/30 → 203 (over)
  ```

**Periods:**
- `current` (the default) is each account's own current period, since accounts can reset on different days.
- `previous` is the period before that.
- `YYYY-MM` is the period starting on the account's reset day in that month.

**Thresholds:** when an entry pushes an account across 80% or 100% of its budget in the current period, `spend.budget_threshold` is emitted. The webhook filter `budget` covers it, along with `spend.approval_requested`.

## Surfaces

**CLI:**
- `another spend` (the report: `--period`, `--by lane|agent|account`, `--json`)
- `another spend account add|list|update|archive`
- `another spend record`
- `another spend borrow` / `release` / `leases`

**MCP:**
- `spend_report`, `spend_accounts`, `spend_record`, `spend_borrow`, `spend_release`
- `spend_account_upsert` (local only)

**HTTP:** `GET /spend?period=current` (behind `--auth`, audited).

**Events:** `spend.account_created`, `account_updated`, `recorded`, `lease_started`, `lease_ended`, `lease_denied`, `approval_requested` and `budget_threshold`.

## Scope

Accounts with a project are visible to a token only if the project matches its globs. Shared accounts (no project) are visible, but only for **entries and leases attributed to the token's projects**. Their figures are then marked `partial: true`, and they never include other projects' spend.

Scoped tokens get **no budget figures** for shared accounts, only what their own projects spent: `remaining` and `projected` would reveal how much everyone else spent. A token's `spend_record` and `spend_borrow` must act as its own agent, and the entry's project must be within its globs.

## Migration

Migration 10, additive: the `spend_accounts`, `spend_entries` and `spend_leases` tables.

## Out of scope

- Pulling real usage from provider APIs. Agents or the person record entries; nothing calls a provider. This project has no hosted dependencies and hard-codes no real accounts.
- Currency conversion: each account has one unit.
- Linking leases to the resource registry (`resource_borrow`). See NEEDS DECISION in the PR.
- Stopping or reaping anything when a budget runs out. Reaping always needs a person.
