# Controlled edits

Status: unfinished development branch, not released. Independent review, integrated client runs and native Windows evidence are pending. A compiled target or synthetic MCP client does not establish native client support.

The boundary is cooperating local sessions. A hostile process running as the same operating-system user is outside it. Session identifiers and client tool-call IDs identify and correlate; they do not authenticate a caller. Another does not exclude a writer that ignores its protocol.

## Route and inputs

The client owns a dedicated `another serve --stdio` process, configured with MCP server name `another`. `controlled_edit` is absent from HTTP, including unauthenticated loopback and authenticated remote connections. Its typed arguments are `project` (exact registered absolute root), `path` (one literal relative file), `expected_sha256` (lowercase digest of the complete preimage) and `replacement` (complete replacement text).

Use the exact session root without symlink components. Relative names must be ASCII, with no dot or parent components, empty components, control characters, surrounding component whitespace, backslashes, colons or glob metacharacters. Missing leaves, nested repositories, symlinks, Windows reparse points, hard links and ambiguous case spellings are refused. Creation, deletion and multi-file calls are unsupported. Each complete preimage and replacement is bounded to 1 MiB; the hook's complete JSON envelope must also fit its 1 MiB input limit.

Controlled literals conservatively conflict with case aliases in hold patterns, including character classes, even where volume case behavior is unknown. This can refuse a case-distinct edit on a case-sensitive volume. The existing advisory matcher retains its earlier rules.

The native pre-edit wrapper denies native edit tools before invoking Another. A separate pre-tool hook matches exactly `mcp__another__controlled_edit`. It resolves the already-started session, checks the target and creates one new pending hold. A node-local unique keyed call digest prevents reuse across sessions, and a canonical operation digest binds the tool, project identity, path, preimage and replacement. Admission never replaces or renews an older manual hold.

The server requires matching protocol call metadata and a pinned client version. Candidate shapes are Claude Code 2.1.325 (`claudecode/toolUseId`) and Codex 0.169.0 (`callId`). Earlier isolated probes observed these shapes; the integrated route is not yet accepted for either client. Absent, ambiguous, unsupported, mismatched and repeated calls refuse. Claim must begin within 120 seconds of admission. A longer permission prompt requires inspection and a fresh call, not replay.

## Confirmation and recovery

Resolution walks component by component from a retained root. Before writing, an atomic claim records the observed file identity and content digests. The editor checks the full preimage, writes through that same handle and syncs it. Cooperating controlled calls also use a file lock or Windows sharing restrictions.

After writing, a second checked resolution must observe the same object, the retained root must match the registered project path, and bytes read through the original handle must equal the replacement. Only then may the server record `confirmed` and renew that operation's exact hold. A PostToolUse assertion cannot confirm a controlled edit.

These are observations during resolution. They do not prove namespace membership at the separate claim-commit instant, attachment between observations, stable pathname membership afterwards or prevention of later aliases. A detected mismatch becomes `unknown`.

Schema v10 preserves v9, its admission key, signed events, holds and admissions. Existing homes require the fenced offline backup and upgrade; do not mix versions. The operator's writer fence remains required and does not cover an unrelated restore or older binary automatically.

`pending` becomes `claimed` once before I/O. Partial writes, failed syncs and failed post-write checks become terminal `unknown`. A crash leaves an unconfirmed claim; when its holds lapse, sweep changes it to `unknown`, never to confirmed or retryable pending. Unclaimed admissions expire without writing. Unknown outcomes stay permanently and appear in `another status` (first 1000 IDs plus total count). Call-key records remain to prevent replay after hold expiry. Never retry automatically, infer rollback, or promote unknown by rereading a pathname after restart.

The holds of pending, claimed and unknown operations are excluded from ordinary renewal, release and adoption. Unknown coverage keeps its original deadline; it cannot become an indefinitely renewed blocker through heartbeat.

## Platform and privacy evidence

Windows code retains ancestor and main-file handles, refuses reparse points and non-private home/sidecar ACLs, creates protected private directories, and publishes flushed fences and staging backups with write-through moves. SQLite sidecars remain in the pinned private home; this is not a custom SQLite VFS. Cross-compilation does not establish Windows runtime or power-loss behavior. The native launcher uses PowerShell.

Call metadata, raw session IDs and file contents do not enter admission events or audit. The executable suppresses rmcp request logging even when `ANOTHER_LOG` requests debug output. Historical signed events retain the previously documented legacy raw-session-ID limitation.
