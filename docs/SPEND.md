# Spend ledger

Track what agents spend (GPU pods, API credits, plan resets, servers, domains) against budgets, per lane and per agent, with an approval step before costly borrowing.

- **Nothing connects to a provider.** Agents or people record entries, and no accounts are built in. The accounts below are only examples to copy.
- Design: [design/05-spend.md](design/05-spend.md).

## Accounts

An account has a kind (`gpu_pod`, `api_credits`, `plan_reset`, `server`, `domain`), a unit, an optional budget per period, and a monthly period that resets on a day you choose (`--reset-day`, 1–28, UTC).

```bash
# Example: Claude API credits, $200 a month, shared by every project
another spend account add claude-api --kind api_credits --unit USD --budget 200

# Example: Codex full resets, 2 per month, resetting on the 15th
another spend account add codex-resets --kind plan_reset --unit resets --budget 2 --reset-day 15

# Example: an hourly GPU pod for one project, which needs approval before anyone borrows it
another spend account add gpu-a100 --kind gpu_pod --unit USD --budget 150 \
  --cost-per-hour 2.10 --requires-approval --approver owner -p ~/Projects/ml

# Example: a domain with a yearly cost, tracked as a lifetime budget
another spend account add example-domain --kind domain --unit USD --budget 15 --period none

another spend account list
another spend account update claude-api --budget 250
another spend account archive example-domain
```

- **Shared or per project:** accounts without `-p` are shared; an account with `-p` belongs to that project.
- **Local only:** accounts are configured on the machine itself, through the CLI or a local MCP client. Remote tokens can't create or change them.

## Recording spend

```bash
another spend record --agent claude@shop --account claude-api --amount 12.40 --lane billing --note "eval batch"
another spend record --agent codex@shop --account codex-resets --amount 1
```

MCP: `spend_record` with `{agent, account, amount, lane?, project?, note?}`.

Each entry is attributed to a project: the lane's, else the account's, else the agent's.

**Budget events:** when an entry pushes an account past **80%** and then **100%** of its budget for the period, `spend.budget_threshold` is emitted. To get it on the phone, subscribe with the webhook filter `budget`.

## Hourly leases and approval

For things billed by the hour, borrow a lease, then release it when done:

```bash
another spend borrow --agent codex@ml --account gpu-a100 --lane finetune --hours 4 --note "LoRA run"
another spend leases
another spend release <lease-id> --agent codex@ml
```

MCP: `spend_borrow`, `spend_release`, `spend_leases`.

- **Cost:** a lease costs `cost_per_hour` × hours. The rate is the account's unless you pass `--cost-per-hour`.
- **Renewal and expiry:** the agent's heartbeat (its prompt hook) renews the lease, and it expires after its TTL (default 1 h) if not renewed.
- **Session end** (the SessionEnd hook, or `another session end`) closes the agent's spend leases: active ones are charged up to then, and pending ones are withdrawn. `--keep-resources` keeps them, renewed along with the resource leases.
- **When it ends**, its cost is written as one entry, or one per period if it ran across a monthly reset (each period is charged for its overlap). While it runs, the cost so far shows as **accrued** in reports.
- **Nothing is stopped or reaped** when a lease ends or a budget runs out. That always needs a person.

**Approval:** if the account has `--requires-approval`, borrowing does not start the lease.
1. It asks a **[decision](HUMAN-DECISIONS.md)** of the approver (the account's `--approver`, else the node's decider: `another config decider alex`): "Approve codex@ml borrowing gpu-a100 at 2.1 USD/h for 4 h (about 8.4 USD)? Purpose: LoRA run", with options **approve** and **deny**.
2. The manager bot hears about it through the `needs_decision` and `budget` webhooks.
3. When the person answers, the manager relays it, for example `POST /decisions/{id}/answer {"value":"approve"}`.
4. If approved, the lease starts then; its clock starts at approval, not at the request.
5. `deny`, or withdrawing the decision, marks the lease denied.
6. Either way, the answer reaches the agent's inbox.
7. **Withdrawing:** `another spend release <lease-id>` on a pending lease withdraws its decision too. The approver is told no answer is needed, the lane leaves `needs_decision`, and `spend.lease_withdrawn` is emitted.
8. **No self-approval:** the agent that borrowed can't relay the answer, even if it is a manager bot. Another manager bot or the person must.

## Paying for registered resources

A [registered resource](RESOURCES.md), such as a rented GPU pod, can be charged to an account with a `cost_per_hour`:

```bash
another spend link gpu-a100 gpu:pod-a100
another spend unlink gpu:pod-a100
```

- Borrowing a linked resource (`another borrow`, MCP `resource_borrow`), or re-registering it to adopt it once it has lapsed (`resource_register`), opens a spend lease for the agent, behind the same approval gate.
- If the account requires approval, the borrow comes back as `pending_approval` with a `decision_id`, and no resource lease yet. Once approved, the resource lease is taken and the spend lease starts.
- The spend lease is renewed along with the resource lease. It ends when the resource lease ends (release, expiry, stop or session end), and is charged up to that moment. `spend release` on it is refused while you still hold the resource: release the resource instead.
- Linking is local only, and only resources registered on this node can be linked.

## Reports and burn-down

```bash
another spend                        # current period, every account
another spend --period previous      # or --period 2026-10
another spend --lane billing         # or --agent codex@shop, --account claude-api
another --json spend                 # the full report
```

```
**Spend**
- `claude-api` 142.10 / 200 USD  71%  ███████░░░  day 21/30 → 203 (over pace)
- `codex-resets` 1 / 2 resets  50%  █████░░░░░  day 7/30 → 4.29 (over pace)
- `gpu-a100` 8.40 / 150 USD  6%  █░░░░░░░░░  day 21/30 → 12 · 1 lease(s) open
Top lanes: billing 96.20 USD (claude-api), finetune 8.40 USD (gpu-a100)
Top agents: claude@shop 120 USD (claude-api), codex@ml 8.40 USD (gpu-a100)
```

**Per account**, the report gives:
- `spent` and `accrued`;
- `budget`, `remaining` and `used_pct`;
- `days_elapsed` / `days_total`;
- `projected`: a straight-line estimate for the period end;
- `daily_left`: the budget left per remaining day;
- `status`: `ok`, `warning` (80% used, or on pace to exceed), `over` or `no_budget`.

**Totals** come `by_lane` and `by_agent`. Units differ between accounts, so totals are per account.

**Where to get it:**
- MCP: `spend_report` (with `markdown`)
- HTTP: `GET /spend?period=current`

## Remote access

With `another serve --auth`:

- **Project accounts** are visible only when their project matches the token's globs. Another project's account (or lease) gets the same `not_found` as one that doesn't exist.
- **Shared accounts** are visible and marked `partial`. `spent`, `accrued` and the lane and agent totals count only the token's own projects. `budget`, `remaining`, `used_pct`, `projected` and `status` are for the whole account, read-only, so a bot can tell how much is left without seeing who spent it.
- **Writes:**
  - `spend_record`, `spend_borrow` and `spend_release` must act as the token's agent and be attributed to a project in its globs (name a lane or a project). A named project, lane or agent (including the report's `--agent` / `agent` filter) is looked up among what the token may see, so a hidden one is the same `not_found` as a missing one.
  - `spend_account_upsert` is refused.
- Every call is written to the audit log.
