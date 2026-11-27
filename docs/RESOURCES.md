# Resource registry

Find a registered model/dev server, port, GPU, container or long job before
starting a duplicate. Physical resource facts and processes are durable
observations. Temporary owner/borrower responsibility is a lease.

## Lease policy

Every subject uses checkout, renew and TTL: **120 seconds by default**, with a
positive requested duration clamped to **30 seconds through 24 hours**. The
chosen duration is stored and returned and renew uses that duration.
Repeated checkout is not automatic renewal. There are no resource-specific
defaults, process keepalives or independent ownership grace periods.

An authenticated integration uses the private `another.agent/1` compatibility
operations to register, borrow and release resources, with minted actor proof.
The compatibility rows project the common lease deadline. Exact release
references prevent late callbacks from releasing a replacement generation.
Names and `--agent` are selectors, not authority.

Presence, a session heartbeat, working on another file or a live process cannot
renew resource responsibility. Explicit renewal affirms an exact owned subject.
At expiry, rights cease without waiting for a sweep or physical process exit.
An expired resource can be checked out after the ordinary conflict check; orphan
or recovery status is not another lock.

## Session end

Authenticated session end releases every owner/borrower lease of that session
and its dependent children immediately, along with file, presence, queue/request
and reservation subjects. There is no `--keep` exception or dormant countdown.
A server may physically continue running, but that does not preserve ownership.
A genuinely independent session needs explicit scoped registration/evidence;
a background label or different native id is not proof of independence.

## Inspecting resources

```sh
another resources
another orphans --no-scan
another doctor
another adapter status
```

Status distinguishes presence, lease freshness, released/expired responsibility
and the physical process. Read-only projections must not renew, wake or send mail.
Actual probe and IO budgets are diagnostics, not ownership clocks.

Resource kind/name/id, endpoint, port, PID, purpose, sharing mode and metadata
remain resource facts. A shared resource can have an owner and borrowers; an
exclusive resource refuses borrowing. All temporary references use the same
duration rule.

Repeat `--meta KEY=VALUE` for each metadata entry. Like `send --field`, keys are
trimmed and must be nonempty and unique after trimming. A missing `=`, an empty
key or a duplicate key is a `usage` error (exit 2), and nothing is registered.
Metadata keys are stored in sorted order rather than argument order. Values keep
their whitespace and any further `=` characters; an empty value is allowed. Legacy foreign events are historical/informational and cannot
adopt a new local actor's rights.

## Physical cleanup

Only a human may approve reaping, and never remotely. The existing
`reap.rs DEFAULT_GRACE_SECONDS = 600` is physical cleanup policy only. It cannot
extend a lease, postpone session release or block checkout after expiry.
Do not infer that a lease stops filesystem IO or a server process.

The historical register/borrow/release/MCP catalogue requires explicit
`--unguarded-repair` and remains outside guarded participation. It prints that
scope exit. The default MCP identity/lease facade refuses mutation without a
private per-actor connection binding. [Contract](AGENT-CONTRACT.md).
