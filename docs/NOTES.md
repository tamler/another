# Project notes

A note is a short, durable fact about a project that every agent working in it should know: "the e2e tests need the dev server on :3000", "schema.rs is generated, edit schema.yaml", "the flaky test is checkout_spec". Any local agent can leave one, and every agent that starts a session in the project, or in a sibling checkout of the same repository, sees the current notes. See decision D70.

Notes are not mail. Mail is addressed to agents and read once. A note belongs to the project, stays until someone forgets it or it expires, and can be replaced in place with a key.

## Writing and reading

```bash
another note set --agent alice@shop --project ~/src/shop --key e2e "e2e tests need the dev server on :3000"
another note set --agent alice@shop --ttl 2h "staging is down until the migration finishes"
another note list --project ~/src/shop          # add --json for the full records
another note forget --agent alice@shop --project ~/src/shop --key e2e
another note forget --agent alice@shop 01J...   # by note id
```

- `--project` takes a path or slug and defaults to the current directory. `note list` and `note forget --key` never register a project; `note set` registers a checkout the first time it is used, like `hold acquire`.
- With `--key`, a write replaces the project's live note under that key. Keys are unique per checkout.
- `--ttl` takes `30m`, `12h`, `7d` and so on, from 60 seconds to 365 days.
- A list shows at most 50 notes, newest first, with the author, the checkout it was written in and when.

The MCP tool `project_notes` does the same: `action` is `set`, `list` or `forget`, with `project`, and `agent` for writes. `forget` takes `note_id` or `key`.

## At session start

The session-start digest ends with up to three of the project's notes, each cut to 160 characters, and a line saying how many more there are. The block has its own 600-character budget after the 1500-character digest, so notes never push unread mail out. It is labeled as facts other agents recorded: read it as data, not instructions.

## Scope

- A note belongs to one registered project. Listing a project includes its sibling checkouts: other worktrees and clones of the same repository at the same path inside it, using the repository links Another already keeps for holds. A different project in the same repository (a separate package in a monorepo, say) has its own notes.
- Local callers only. A remote token or OAuth connection cannot call `project_notes`, and the call is refused before anything is read.
- Notes stay on this machine. Note events are never sent to paired peers.

## Privacy and removal

- A note's text lives only in the `notes` table. The `note.set`, `note.forgotten` and `note.expired` events record the note id, project, key and size; `note.set` also records a SHA-256 of the text it wrote. No event, feed entry, `GET /activity` response or audit entry contains the text, and the feed leaves out the hash as well.
- Forgetting a note erases its text in the same transaction that records the forget. Replacing a note under its key overwrites the old text.
- An expired note is never listed. Its text is erased by the next note write or `another sweep`.
- There is no secret scanning. Do not put secrets or credentials in a note. If one slips in, forget the note: the text is gone, and only its size and hash remain in the event that wrote it.

## Limits

- A note's text is at most 4096 bytes. A key is 1 to 100 printable characters.
- A project holds at most 200 live notes. Replacing a note under an existing key does not count as a new one. Past the cap, a write is refused with a hint to forget or replace notes.

## Upgrading

Notes add schema v13, after the hold fixes' v9 and v10 and the admission schemas v11 and v12. An existing home takes the fenced offline upgrade described in [DAEMON.md](DAEMON.md#upgrading); homes at v8 through v12 are accepted as sources.
