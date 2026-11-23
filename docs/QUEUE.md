# The work queue

Another's single-node work queue records proposed, ready, claimed and done
items. Every agreed item has a scope id and done condition. Proposing does not
admit an item. The owner, or an explicitly delegated architect, admits it and
the record retains who admitted it and when.

## Admission and claims

The owner still acts explicitly at a terminal with `--owner` and a typed yes.
An owner-set limit controls each actor's claimed, unfinished items; the default
is one and zero means no seat. Changing a limit does not release work. Security
and data-loss items wait for the owner personally.

A private `another.agent/1` integration uses the authenticated compatibility
`next` operation to claim the first eligible ready item. Selection, limit and
claim checks are atomic. A proposed item is never claimable. The reply includes
the business item and actual common lease metadata.

Responsibility is a common lease: **120 seconds by default**, requested checkout
duration clamped to **30 seconds through 24 hours**, stored and returned.
Renewal names an exact live lease/generation and uses its stored duration.
A name, `ANOTHER_AGENT`, `--agent`, a pane or a registration is not authority.

## Release, expiry and results

Observed authenticated session end releases that epoch and its dependent scope
tree immediately, returning unfinished claims to ready with history retained.
TTL is the backstop for a missing signal. There is no separate queue grace,
caller exemption, name-wide heartbeat keepalive or dormant countdown for
participating claims. Expiry is effective at the stored deadline without a
sweep; materialization and owner notices do not extend it.

File activity, presence, mail, process liveness and stage observations do not
renew a queue claim. A long task requests a longer checkout explicitly or
authenticates an exact subject renewal. An old result/release must carry the
current claim reference; it cannot act on a replacement generation.

A result stores a summary and one pull-request, commit or file pointer on the
item and in its history. Handoffs and releases remain durable facts. Ending a
session never deletes an earlier result. The owner can inspect claimed history,
approval, notes and earlier handoffs after responsibility has ended.

## Explicit views

```sh
another queue list
another queue show Q12
another queue audit
another needs
another adapter status
```

These views are requests for information. They are not keepalives or permission
to write. The old hook ready/held queue digests are removed; routine hooks only
emit conflicts and the idle release reminder. The unused/stale threshold is the
one default TTL, 120 seconds. Reminder delivery never extends a claim.

Owner configuration and admission remain separate from actor work authority.
A recipient/reservation name may resolve a target; it cannot authorize that
target as the sender or worker. Read-only status remains available when the
calling shell has no provable actor.

## Compatibility and upgrade

The private facade projects authenticated queue claims into the existing item
and history tables, backed by common leases. Legacy name-based claims are not
adopted at schema cutover. Migration is backed up and transactional; unknown or
newer schemas refuse. Quiesce writers and upgrade integrations together.

Historical operator commands and experimental clients can be used only through
an explicit `--unguarded-repair` scope exit. Legacy settings/diagnostics are
not clocks for participating claims. Installed hooks never select repair mode,
and an internal error never authorizes it. The default MCP catalogue has only
identity/lease facade tools and refuses mutation without private per-actor
binding. [Contract and coverage](AGENT-CONTRACT.md).
