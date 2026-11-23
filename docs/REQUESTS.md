# Mail and requests

Messages, threads, read/acknowledgement state, request history and results are
durable facts. Temporary responsibility and delivery/wake reservations are
leases; an acknowledgement is not ownership or completion.

## Actor authority

A sender/worker is a contract-minted root or child actor. An integration carries
private actor proof and epoch through `another adapter event`, outside model
context, argv, ordinary environment and logs. Names and `--agent` can identify
recipients or filters; they cannot authorize the sender or worker.

Authenticated compatibility send replaces sender identity with proof. It can
create a request and set an optional business due time. Request claim/update
uses the same private channel. Release/completion validates the current exact
claim reference; a late callback cannot act on a replacement generation.
The creator's cancellation authority remains distinct from worker ownership.

## Claim timing

One checkout policy applies to file, presence, resource, queue/request and
reservation subjects: **120 seconds by default**, positive requested duration
clamped to **30 seconds through 24 hours**, stored and returned. Renew names an
exact live reference and uses its stored duration.

Request responsibility expires at its common deadline without another grace or
a sweep. Due dates and reminder scheduling are business clocks, never another
ownership timer. A business stale observation cannot grant takeover while a
different actor's common lease remains live. Presence, mail, prompts, process
liveness and file work cannot renew the request claim.

Observed authenticated session end immediately releases all responsibility in
that epoch and its dependent scopes. There is no keep-resource exception or
dormant countdown. Missing end signals rely on ordinary TTL. Messages, read/ack
state, history and results are retained.

## Explicit views and notices

```sh
another inbox --agent ACTOR --unread
another requests
another thread THREAD
another adapter status
```

Use actual public addresses/ids returned by the integration when selecting a
view. Requested status is not a keepalive or an actor credential. Ambiguous
shell identity does not prevent read-only status.

Routine hooks do not inject inbox/queue/wake digests. Their only context notices
are actual conflicts and the one unused/stale reminder. The idle threshold is
the default TTL; the reminder does not renew/release a request or send a wake.
Read-only projections do not claim notifications, acknowledge reminders or
alter ownership.

Business notices about a completed/released request remain durable mail.
A future transport's temporary reservation must use the same common lease and
actual private actor context; unconverted reservation routes refuse for
participating actors. A durable delivered/sent marker is not an ownership clock.

## Legacy compatibility

The historical name-based CLI/MCP catalogue is available only through explicit
`--unguarded-repair`, outside guarded participation. It prints that scope
exit, cannot select a minted actor by name, and is never used by installed
hooks or as an error fallback. Existing experimental remote clients cannot
mutate new actor state with old bearer/name identity alone.

Legacy business due/reminder/history data survives migration, while old current
work claims are not adopted. [Contract](AGENT-CONTRACT.md), [queue](QUEUE.md),
[hooks](HOOKS.md).
