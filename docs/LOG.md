# Exporting the log

Every change Another makes is recorded as an event in its database: a message sent, a hold taken or released, a request claimed, an agent registered. Remote calls made with a token are also recorded, in a separate audit log. `another events` shows the most recent events. `another log export` writes the whole log, or part of it, for another tool to read.

```bash
another log export > events.jsonl
```

The output is JSON Lines: one object per line, in log order (by `seq`, the order events were stored), and nothing else on stdout. That is nearly always oldest first, but not always: after the clock steps back, a later event carries an earlier `ts`, and a peer's events are stored when they arrive, after newer local ones. Log order is what lets the cursor below continue without gaps. Each event line has `seq`, `event_id`, `node_id`, `ts` (RFC 3339, UTC), `actor`, `kind`, `payload` (the event's data), `prev_id` and `sig`. A payload that is not valid JSON is kept as text, with `"payload_raw": true`.

When it finishes, the export prints a line on stderr naming the last event:

```text
another: exported 4210 event(s), last event_id 01M7JEK7Y65QNYRTZN0JYEDTGE
```

Pass that id to `--since` and the next export starts right after it, with nothing missed and nothing repeated:

```bash
another log export --since 01M7JEK7Y65QNYRTZN0JYEDTGE >> events.jsonl
```

## Options

| Option | Meaning |
|---|---|
| `--since <when>` | A duration (`30m`, `2h`, `1d`, `1w`), an RFC 3339 time, or an event id to continue after |
| `--until <time>` | Up to and including this RFC 3339 time |
| `--kind <kind>` | Only these kinds, such as `message.sent` or `hold.acquired` (repeat or comma-separate) |
| `--node local\|all` | This node's own events (the default), or peers' events stored here too |
| `--include-foreign-bodies` | With `--node all`, keep the bodies of messages written on other nodes. Without it, a peer's message body is `null` and `body_redacted_bytes` gives its size, as on the dashboard (D41) |
| `--limit <n>` | Stop after this many lines |
| `--audit` | Export the audit log of remote calls instead. Its lines have `audit_id`, `ts`, `token_id`, `agent`, `operation`, `outcome`, `detail` and `remote`, and `--since` also takes an audit id |

## What it does not do

- **It writes nothing.** It reads the database without creating, migrating or changing it, and records no event of its own. A missing database or a schema from another build is an error.
- **It does not remove anything.** The event log and the audit log keep growing. A retention policy is a separate, open proposal.

## Privacy

The export contains everything in the log, including the subjects and bodies of messages written on this node and the agent names in the audit log. Bodies of messages from other nodes are redacted unless you pass `--include-foreign-bodies`; their subjects are not. It goes only to stdout; Another writes no file. Treat an export file like the database it came from.

When `--kind` matches nothing, the stderr line says which of the kinds this log has never held, so a misspelling does not look like an empty log.
