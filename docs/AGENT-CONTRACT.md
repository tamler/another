# another.agent/1

The approved SPEC-B7 contract is checkout, renew and TTL for every temporary
right, under individually minted actors. Another is not a launcher.
The CLI/shared SQLite core is the authority; native callbacks, private
integrations and optional MCP are adapters over it.

## Private enrollment and protocol

`another adapter enroll` creates a local integration grant in a private
directory and prints only its location and participant id. The integration
backend retains the grant; never put it or actor proof in model context,
argv, ordinary environment, logs or an outgoing message.

`another adapter event` reads a typed envelope from stdin:

- Major/minor version, request id and required features.
- Operation and payload, with participant or actor proof where required.
- Actor epoch and exact lease generation/call references for mutations.

Major 1 is required. Unknown required features or incompatible versions refuse.
Optional minor additions are additive. JSON replies include authority time,
request id and typed result/error. A timeout is unknown acknowledgement: query
or retry the same request id, never create a second write. Compatibility business
operations reserve their request id durably before execution; an interrupted
pending acknowledgement refuses instead of executing twice. A retry while the
reservation is younger than 60 seconds is refused as `UNKNOWN_ACK` (still in
flight). An older reservation belongs to an attempt that stopped (a crash or kill)
before recording its outcome: it is settled once as `INTERRUPTED` and never re-run,
because its effect may have committed. Check the business record, then use a new
request id.

File subjects (`checkout` of a file, `edit_begin` targets) are exact absolute
paths. The serving process's folder is not the caller's, so core refuses a
relative path instead of resolving it. `another adapter event` runs in its
caller's folder and makes relative file targets absolute from there before
admission; native hooks resolve from the hook payload's `cwd`.

Inside Another the actor proof travels as a typed value bound to the call, never
as text in an agent-name field, so no event, error, log line, federation event
or database row can carry it. Only its hash is stored.

The integration grant file is created with mode `0600` in a `0700` directory on
Unix, and a grant readable by group or others is refused. On Windows there is no
mode bit and Another adds no ACL code: the grant inherits the ACL of the Another
home, which under the default per-user profile (`%USERPROFILE%\.another`) only
that user and administrators can read. A home set with `ANOTHER_HOME` in a shared
folder is not protected. That is a stated limit.

Roots and dependent children register under private participant authority.
Register/resolve returns canonical identity and private proof before admission.
A public native id, display name, `ANOTHER_AGENT` or `--agent` cannot adopt
or select an actor as mutation authority. Parent/child proofs are distinct.
A public name/id remains usable as a recipient or read-only filter.

## Operations

| Operation | Effect |
|---|---|
| Register / resolve | Mint or resolve an individual root/child under retained participant proof |
| Resume | Retain identity, open a new epoch; old rights remain released |
| Checkout / check | Check conflicts and return stored timing; check creates no right |
| Edit begin / check edit | Atomically admit exact targets; revalidate the existing call without renewal |
| Edit complete | Record the correlated outcome; eligible renewal is exact-target/generation only |
| Renew / release | Authenticate exact owned live references; never revive an expired generation |
| Activity | Separate presence and one idle-episode reminder; no file renewal |
| Session end | Close the exact epoch and dependent scope tree, releasing every right immediately |
| Compatibility | Authenticated file/resource, queue-next/result, mail and request projections |
| Status | Pure actor/lease/history projection with coverage limitations, no proof output |

Generic file/presence checkout uses the common core directly. Business subjects
must use the corresponding authenticated admission/projection operation: a
generic queue checkout cannot bypass item admission or the owner's limit.
Unconverted wake/delivery reservation routes refuse for participating actors
until their actual private carriage exists. They do not fall back to a legacy
name or introduce another ownership deadline.

Compatibility operations replace sender/worker identity with authenticated proof.
They expose common lease metadata with the business projection. Queue/resource
completion or release carries exact current references. Common expiry is
materialized into unfinished queue/request responsibility during writes, without
a caller exemption or extra grace. Resource facts/processes and durable
messages/results/history remain after ownership ends.

## One record and clock

Every temporary subject uses:

`lease_id, generation, owner_actor_id, subject_kind, subject_key,
session_scope_id, session_epoch, last_update_at, ttl_seconds, expires_at,
released_at, evidence_source`.

The one default is **120 seconds**. A positive checkout request is clamped to
**30 seconds through 24 hours**, stored and returned. Renew uses the stored
duration. At `now >= expires_at`, or after release, the right is ineffective
without a sweep. Fresh checkout performs conflict checks and gets a new
generation. Request replay returns the original result without moving its
deadline.

An actor/file has one backing lease shared by retained intent and admitted
operations. Pre on a live file does not renew it. Successful completion, or a
confirmed targeted execution failure, can renew only the exact admitted
still-live generations. Denial, missing Post, unknown/empty target sets,
another file, read/search, stage, process liveness and presence cannot renew it.
Completed or expired call receipts are history, not permission to execute again.

Presence is a separate lease. After the default TTL without attributed activity,
the actor is unused/stale. The next attributable hook advises:

> You were inactive. Release what you no longer need; re-check expired leases before writing.

Evaluate the prior episode before new activity. The reminder neither extends nor
releases rights and sends no wake. A pure status view does not acknowledge it.
Long requested leases retain their actual deadlines.

## Lifecycle and IO limits

Observed authenticated session end closes admission and releases all scoped
rights in one transaction, including dependent children whose own end never
arrived. A child end closes its branch, not parent/siblings. A remapped id or
background label proves no independent lifetime. No dormant countdown or
keep-resource flag delays release. Missing signals rely only on TTL.

Resume requires retained proof and a fresh epoch. Old end/completion callbacks
cannot touch a replacement epoch. A late outcome can remain history without
restoring a right. Native adapters retain the epoch proof they received; they
cannot infer a resumed epoch from public ids or silently adopt it after cache
loss. Private callback carriage must establish a resumed binding explicitly.

Pre is admission-time only. TTL does not stop OS writes or guarantee final
dispatch/permission revalidation. A native operation may outlive its lease.
Reacquire/reconcile file/base/diff versions before further writes; ownership does
not prove hunk authorship. Pre's 300+2700 ms and Post's 50 ms waits are IO budgets,
not ownership clocks. Physical reap grace (600 seconds) is cleanup only.

## Reference profiles and coverage

Claude/Codex use shared root session ids and child `agent_id`. A stock Claude
root without child indicators resolves to root; known child discriminator loss
or contradiction refuses. Grok child-start and remapped sessions bind dependent
child scopes. SubagentStop/Stop alone never closes the session.

Cursor remains installed, quiet and unguarded. The live
`2026.11.22-e9042b2` map has root session/conversation equality, child remapping
to the Task child UUID, parent `tool_output.agentId` and child-transcript
corroboration. No tool-hook `agent_id` or unobserved subagent-start/stop event
is invented. Permission denial is not execution. The observed late interrupt
failure after sessionEnd cannot revive rights; `is_interrupt` is untrusted.
Unstable `hook_ppid` is not shell proof. Cursor shell/CLI mutation stays
`NO_ACTOR`.

Stock shell calls are refused until actual per-call private carriage or a tested
ancestry/call association exists. Ancestry can identify a session, not a shared
child. A returning actor's matched Pre/private callback must refresh its context
before mutation. Only concurrent live actors count toward handle-less ambiguity;
idle registrations are not extra candidates. No match means refusal, not a sole
recent actor fallback. Herdr/Pi callback/carriage support needs actual evidence.

The default MCP catalogue has only identity and lease facade tools. An unbound
connection reports/refuses `NO_ACTOR`; no model-selected id substitutes for a
private connection context. The historical broad catalogue is an explicitly
selected legacy repair surface, not a default integration.

## Cutover, legacy repair and evidence

Migration uses verified backup plus a transactional schema/catalog step.
Unknown/newer schemas refuse. Quiesce participating writers; upgrade the binary,
daemon and integrations together. Existing legacy current claims are disposed,
not adopted by name; durable facts are retained. Schema/decision numbers are
assigned by the architect at merge, not reserved by this lane.

`--unguarded-repair` is an explicit visible operator scope exit for historical
records and observations. Its named labels cannot mutate a minted actor. It is
never an automatic fallback or an installed hook option. The rule is enforced in
the core library, not only at the CLI: a store handle refuses every mutation
selected by a plain name (`NO_ACTOR`) unless it is the explicit repair handle
(`Store::unguarded_repair`), which the CLI, MCP server and hook runner take only
under `--unguarded-repair`. Repair-mode holds are pre-contract records, not
contract leases. An actor's file lease from edit admission is not checked against
repair-mode holds in either direction; only an actor hold taken through the
compatibility `hold` operation shares the holds table with them and conflicts.
That gap is why repair is a visible exit from guarded participation, never a
mode to run alongside participating actors.
Read filters and wake/delivery bookkeeping still select by name; those grant no
right, and wake/delivery carriage is M-M work. Legacy repair fixtures
test that compatibility surface, not guarded participation.

Manual-clock core tests, CLI/MCP tests and native payload fixtures are separate
from live client/device proof, measured latency, independent implementation
review and human acceptance. This first PR does not claim guarded-finite native
conformance, a tested stock shell route, publication or release acceptance.
