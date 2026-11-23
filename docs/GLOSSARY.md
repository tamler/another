# Glossary

## Identity and authority

- **Node**: a local SQLite/WAL home and signing identity. The default is
  `~/.another`; development/tests must use a disposable `ANOTHER_HOME`.
- **Participant**: an enrolled integration with a private grant. It can register
  logical roots/children under the open `another.agent/1` contract.
- **Actor**: an individually minted root `agent@session` or child
  `child.agent@session`. Its public name/id is not a credential.
- **Proof**: the private actor capability and epoch transported by the integration,
  outside model context, argv, ordinary environment and logs.
- **Session scope/epoch**: lifecycle grouping and incarnation, not ownership.
  Root end includes dependent children. Old callbacks cannot close a resumed
  epoch. Stop/end-of-turn alone is not session end.
- **Project/checkout**: a registered directory and the working directory where
  paths live. Distinct worktrees/clones are distinct file targets. Historical
  `name@project` addresses remain legacy labels, not actor authority.

## Temporary rights

- **Lease**: the one checkout/renew/TTL record for file intent, in-flight edits,
  presence, resource responsibility, queue/request claims and wake/delivery
  reservations. The record carries actor, subject, scope/epoch, generation,
  last update, stored TTL, expiry, release and evidence.
- **Default/requested duration**: 120 seconds by default; a positive request is
  clamped to 30 seconds through 24 hours and stored/returned at checkout.
  Renewal uses that duration. There are no per-kind ownership clocks.
- **Generation**: a fresh checkout's reference. A late completion, renew or release
  cannot revive or release a replacement generation.
- **Expiry**: at `now >= expires_at`, the right is ineffective and nonblocking.
  No sweep, process exit, orphan cleanup or extra grace is required.
- **Path hold / admitted operation**: an actor's exact-file intent and its operation
  references share one lease. Pre protects an attempt, not proof of a write.
  Completion renews only exact admitted still-live generations.
- **Presence**: a separate lease. It cannot broadcast renewal to files or work.
- **Unused/stale**: no attributed activity for the default TTL. The next hook/view
  can advise release without changing a lease. Idle registrations do not count
  toward shell ambiguity.
- **Session release**: observed authenticated end closes the epoch and releases
  all its rights/dependent scopes immediately. There is no keep-resource or
  dormant-countdown exception. A missed end signal relies on ordinary TTL.

## Durable work and history

- **Message/thread/read/ack**: durable correspondence and receipts. Acknowledgement
  does not mean ownership or completion.
- **Request**: a direct ask with durable status/history and an optional business
  due time. Worker responsibility is a common lease; due time is not its TTL.
- **Queue item**: proposed, ready, claimed or done. Only the owner/delegated
  architect admits; scope, done condition, approver and admission time are recorded.
- **Claim**: temporary actor responsibility under a common lease, distinct from
  durable approval, handoff and result records.
- **Limit**: the owner's maximum of claimed unfinished items for an actor.
  The default is one; zero gives no seat.
- **Result**: a durable summary plus a PR, commit or file pointer.
- **Owner list**: `another needs`, with owner decisions and permitted delegate
  actions. A row does not authorize wider work or wakes.
- **Stage/activity**: last-observed evidence, not an area lock or lease refresh.

## Integration and operational limits

- **Admission-time gap**: a native edit can outlive its Pre lease. TTL does not stop
  OS IO or prove hunk authorship. Reacquire/reconcile before further writes.
- **Private call carriage**: actual per-agent descriptor/channel or tested call
  association. Ancestry can identify a session, never a shared-process child.
  A shell without matched/refreshed context refuses.
- **Cursor coverage**: live 2026.11.22-e9042b2 payload mapping, unguarded;
  shell/CLI mutation remains NO_ACTOR without tested private carriage.
- **Legacy repair**: explicit `--unguarded-repair` operator scope exit for
  historical records; never an automatic fail-open path or actor credential.
- **Sweep**: materialize expiry/history. It does not decide when rights stop.
- **Reap**: human-approved physical cleanup. Its 600-second physical grace is not
  an ownership timer and cannot block a fresh checkout.
- **Security/business/IO/history clocks**: credential expiry, due dates, backoff,
  probe budgets and history windows. None can prolong work ownership.
- **Evidence**: source inspection, automated tests, native runtime/device proof,
  independent review and human acceptance are separate claims.

See [the contract](AGENT-CONTRACT.md), [hooks](HOOKS.md), [holds](HOLDS.md),
[queue](QUEUE.md), [resources](RESOURCES.md) and [mail/requests](REQUESTS.md).
