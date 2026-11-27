# Actor and lease status

`another status --json` and `another adapter status` read the current database without migrating, registering, sweeping, acknowledging, sending or renewing anything. Initialize or explicitly migrate with `another init` first. Status never prints private proofs.

Actors show canonical identity, native binding, parent, session scope and epoch, closure, last activity and unused/stale state. After 120 seconds without attributed activity, status computes the idle release reminder without acknowledging it. Longer requested leases keep their own deadlines.

Leases show owner, subject, generation, selected TTL, last update, deadline and release reason. The common default is 120 seconds; positive requested durations are clamped to 30 seconds through 24 hours. Expired rights are nonblocking without a sweep. Session end releases all session and dependent rights immediately. Presence and reminders never renew files.

Operations show admitted exact lease references and durable completion evidence. An unresolved operation is not a second lock after its backing lease expires. Admission-time hooks cannot stop a native write delayed beyond expiry; reconcile file versions before continuing.

Cursor reports the live 2026.11.22-e9042b2 map, unguarded, with CLI mutations `NO_ACTOR` until tested caller carriage exists. No native adapter is advertised as guarded-finite. Per-client latency/runtime acceptance remains separate.

`another status --repo [PATH]` selects file evidence under a directory, its actors and their scope parents. PATH defaults to the current directory. It includes those actors' other rights for lifecycle context and only matching file operations. An actor with no file evidence there is not assigned to that repository by its name. Omit the filter to see every node actor and lease.
