# `another doctor`

`another doctor` checks an installation in one read-only pass. It prints one line per check and, under anything that is not `ok`, the command that fixes it.

```text
$ another doctor
ok    binary    another 0.1.0 at /Users/me/.local/bin/another (the one on PATH)
ok    home      /Users/me/.another (0700)
warn  database  schema v8, this build migrates it to v10
                fix: another status (any normal command migrates it)
ok    hooks     installed for claude, codex, grok, cursor
ok    server    answering on 127.0.0.1:7466
ok    daemon    one.toanother.another loaded, pid 969, healthy
skip  peers     not checked until the database check passes
```

`--json` prints `{"ok": bool, "checks": [{"name", "status", "detail", "fix"}]}`. `--bind HOST:PORT` checks a server on another loopback port.

## What it checks

| Check | Looks at | Fails when | Warns when |
|---|---|---|---|
| `binary` | This binary's version and path, and the first `another` in the absolute entries of `PATH` | never | `PATH` finds a different `another`, or none |
| `home` | `--home`, `$ANOTHER_HOME` or `~/.another` | it is missing or not a directory | on Unix, its mode is not `0700` |
| `database` | `another.db`, opened read-only: the schema version against this build's, SQLite's `quick_check`, and the local node | the file is missing, not a database or damaged; the schema is newer than this build; the schema version is negative (not an another database, or edited by hand) | the schema is older (a normal command migrates it); the database stayed locked for 2 s |
| `hooks` | Each client's hook config (Claude Code, Codex, Grok, Cursor), and the wrapper script each one runs | never | a config runs a wrapper that is missing or not executable, or that belongs to another home |
| `server` | One `GET /health` on a loopback address: `--bind`, else the daemon's, else `127.0.0.1:7466` | never | something answers, but not with 200; or the server names another node than this home's (it serves another `ANOTHER_HOME`). Nothing answering is `info` |
| `daemon` | The launchd service (macOS only) | never | it is installed but not loaded, or loaded but not answering. Serving a different home is `info` |
| `peers` | Paired nodes: role, revocation and last sync | never | the hub has not synced for 24 hours, or a spoke for 7 days |

Statuses are `ok`, `info` (optional and not set up), `warn`, `fail` and `skip` (does not apply here). The exit status is 1 only when a check is `fail`, so a script can gate on it; warnings exit 0.

## What it does not do

- **It changes nothing.**
  - It does not create the home or the database, and it does not migrate the database or write an event.
  - Every database read goes through read-only connections, which also cannot checkpoint the WAL into the main file.
  - It does not edit a hook config or touch the daemon; for the daemon it only reads launchd's record and the plist.
  - One exception: SQLite may leave the empty `another.db-shm` and `another.db-wal` files that any reader of the database creates.
- **It contacts nothing but loopback.**
  - There is exactly one network call, `GET /health`, to a literal loopback address (`127.0.0.1:PORT` or `[::1]:PORT`), with no proxy and no redirect followed.
  - A host name or any other address given with `--bind` is skipped, not looked up or dialled.
  - It never contacts a federation peer, a tunnel or any remote host.
- **It runs no fix.** Each `fix:` is a command for you to run.
- **Not checked:** tokens and OAuth grants, tunnels, Herdr, tmux and cmux, and the resource scanner's permissions.

## How long it takes

Usually well under a second. It is not bounded overall:
- **Bounded.** A locked database is waited on for at most 2 s, and `/health` gets 2 s.
- **Not bounded.**
  - `quick_check` reads the whole database, so its time grows with the database's size.
  - On macOS the daemon row runs `id -u` and `launchctl print`, with no timeout of their own. A stuck `launchd` holds `doctor` up with it.

Press Ctrl-C to stop it; it has changed nothing.

## A damaged database

If `database` fails with a `quick_check` complaint:
- stop `another serve` and the daemon (`another daemon uninstall` on macOS);
- copy `another.db` **together with** its `-wal` and `-shm` files somewhere safe before trying anything else. Committed data can sit in the `-wal` file, so a copy of `another.db` alone may be missing it.

SQLite's `.recover` command (in the `sqlite3` shell) can often salvage the rows. Another has no backup or restore command yet.
