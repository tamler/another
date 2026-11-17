# `another doctor`

`another doctor` checks an installation in one read-only pass. It prints one line per check and, under anything that is not `ok`, the command that fixes it.

```text
$ another doctor
ok    binary    another 0.1.0 at /Users/me/.local/bin/another (the one on PATH)
ok    home      /Users/me/.another (0700)
warn  database  schema v8, this build migrates it to v10
                fix: another status (any normal command backs it up, then migrates it)
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
| `database` | `another.db`, opened read-only: the schema version and its tables against this build's, SQLite's `quick_check`, the local node, and `build.json` (which build created the home and which last migrated it) | the file is missing, not a database or damaged; the schema is newer than this build; the schema version is negative (not an another database, or edited by hand); the tables are not this build's for that version (a different build changed them); the schema is older and this build will not migrate it (a development build on a home that is not a development home) | the schema is older (a normal command backs it up, then migrates it); the database file is empty (a normal command creates the schema); the database stayed locked for 2 s |
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

SQLite's `.recover` command (in the `sqlite3` shell) can often salvage the rows. If the damage followed an upgrade, restore the backup taken before it (next section).

## Upgrades and backups

A normal command brings an older database to its own schema. Three rules keep that safe.

- **A backup first.** Before any migration, the database is copied to `backups/another-v<schema>-<UTC time>.db` in the home and the copy is checked. If the copy fails, nothing is migrated. After a migration succeeds, three backups are kept: the one just taken and the two newest before it, by the time in their names. Older ones are deleted. Only files with exactly that name are ever deleted, so anything else you keep in `backups` stays. A backup whose name is dated in the future (the clock was wrong once, or you copied it in) is left alone and does not count as one of the three; delete it yourself when you no longer want it. A migration that fails is rolled back, and the backup it took is removed again, since the database it copied is still there. `backups` must be a directory: a symbolic link there is not followed, and the migration stops.
- **Only a schema the build knows.** A build refuses a database whose schema is newer than its own, and one whose tables are not what it has for that version (a different build changed them; the message names the tables). It says so and leaves the database file as it was. Every normal command reads the tables itself. Tool hooks and `another board` make the same check but skip the catalog queries when `build.json` shows the tables were already checked and the schema has not changed since. When a normal command refuses the tables, it removes that note, so hooks and `board` refuse them from then on too.
- **A development build keeps to its own homes.** A development build is one compiled without optimisations (`cargo build`, `cargo run`, `cargo test`), or a release-profile binary that is still in the Cargo build directory it was built in (`target/release/another` after `cargo build --release`; it is recognised by Cargo's `.fingerprint` directory beside the binary, whatever the directory is called). It refuses to migrate a home unless only development builds have ever created, migrated or run a normal command on it. Give it its own home with `ANOTHER_HOME=$(mktemp -d)`. `--dev-migrate` overrides this for one run; a backup is still taken first, and the next migration needs the flag again. `another --dev-migrate doctor` reports what a run with the flag would do.

An installed build migrates with no flag, whatever its path: `cargo install`, a release archive, a package manager's versioned directory, or a copy of the binary anywhere outside a Cargo build directory. One way of running `another` is refused: straight from the build directory of a checkout (`./target/release/another`, or a symlink to it). That is where an unmerged branch's build lives, so it is treated as a development build even when it is the only `another` you have. Install it (`cargo install --locked --path crates/another` in the checkout) or copy the binary out to a directory on your `PATH`, and it is an installed build. A home that only such a build has ever used stays a development home, which any development build may migrate.

`build.json` in the home records which build created the database, the first release build to use it (`adopted`) and which build last migrated it, each with its version and its binary's path. The paths are for you to read; they decide nothing. Release use is recorded when a release build creates the home, migrates it or runs a normal command on it; `another doctor`, `another board` and tool hooks write nothing, so they do not count. From then on development builds need the flag for that home. A normal command may also note in `build.json` that the tables were checked. A build from before this record that migrates the home rewrites `build.json` without these notes.

While a build refuses a home, hooks still exit 0 and print nothing, so the session is not interrupted; mail is not announced and edits are not guarded until it is fixed. The reason is written once to `logs/hooks.log` in the home, and any normal command or `another doctor` prints it.

To restore a backup:

```sh
another daemon uninstall          # and stop every other another process
cd ~/.another
cp backups/another-v10-20261117T031000Z.db another.db
rm -f another.db-wal another.db-shm
another doctor                    # with the build that matches that schema
```

Everything written after the backup is gone. There is no restore command.
