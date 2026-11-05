# A spend budget with lease approval

A shared GPU account has a monthly budget and requires approval. Borrowing it starts an hourly lease only after the person says yes.

```bash
sh examples/spend-approval/run.sh
```

What happens:

1. **The account.** `another spend account add gpu --kind gpu_pod --unit USD --budget 50 --cost-per-hour 2.5 --requires-approval`. The approver is the node's decider, `jacob`.
2. **Spend so far.** `another spend record` books 30 USD, so the report shows 20 left.
3. **Borrow.** `another spend borrow --hours 3` returns a lease in `pending_approval` and a decision for `jacob` with the options `approve` and `deny`.
4. **Approve.** `another decision answer <id> approve` starts the lease (`active`), and the agent is mailed.
5. **Deny.** A second, bigger request is denied with a note. Its lease never becomes active and costs nothing.
6. **Release.** `another spend release` ends the approved lease and records its cost. `another spend` prints the phone-sized burn-down.

The gate holds against token callers: a remote borrower can never approve its own lease. On one machine, every local process acts as that machine, so locally the gate is a convention (see docs/security-review-v0.1.0.md).
