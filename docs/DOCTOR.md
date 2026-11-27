# `another doctor`

`another doctor` checks an installation in one read-only pass. It prints one line per check and, under anything that is not `ok`, the command that fixes it.

The `peers` check also warns about unreleased holds whose node differs from their agent's or project's node, or whose agent, project or referenced node row is missing. It names the count and up to five hold ids and their nodes, marking missing references. Legacy foreign-node rows are inert since this build: they neither cover nor block edits, and local commands do not adopt, renew or release them. Rows are not removed or repaired. A normal legacy row ends when it expires or its node releases it; an orphan may have no surviving node to release it. There is no repair command.

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
info  transports herdr only (no transports.json, so no command transport)
warn  resources 2 lapsed: long_job eval-run (for 3 h), model_server ollama@11434 (for 2 d)
                fix: another orphans
info  listeners 3 unregistered (lsof): port 5173 node, port 8000 python3, port 11434 ollama
                fix: another orphans
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
| `daemon` | The launchd service on macOS or the systemd user unit on Linux, including its definition, loaded/enabled state and main PID | never | it is installed but not loaded/enabled, or loaded but the server check did not accept its response. An installed definition serving a different home is `info`; a loaded launchd job whose home differs or cannot be verified, or a systemd unit whose home cannot be verified, is `warn` |
| `peers` | Paired nodes: role, revocation and last sync. "Last sync" is the last sync that ran to its end: on a spoke, of this node with its hub; on a hub, the last push or pull of that spoke it answered. A sync that fails part way does not count | never | the hub has not synced for 24 hours (fix: `another sync, and check the hub URL with another peers`), or a spoke for 7 days (fix: `run another sync on that node; if it is gone for good, another pair revoke <node>`); the hub is reached over plain `http://` at an address that is not loopback (anyone on the network path can read mail and the feed in transit); the hub was joined before invites were signed, so its address was never checked (compare it with the address the hub's owner gives, or join again with a fresh invite made with the same `--perms`); or a pairing record for this node itself exists (an older build wrote it; it is ignored and needs no action). See [FEDERATION.md](FEDERATION.md) |
| `transports` | `transports.json` in the home ([WAKE.md](WAKE.md#command-transports)), the stored wake targets and the session rows | never | the file was refused (the reason is given); a wake target or a session row names a transport that is not configured; a wake target is the old cmux target `notify`; a program a command transport would start is not found on `PATH`. No file is `info` |
| `resources` | This node's registered resources with no live lease ([RESOURCES.md](RESOURCES.md#orphans)), read without a sweep | never | any is lapsed: the count, and up to five by kind, name and how long |
| `listeners` | Listening TCP sockets that no registered resource accounts for, found by the scan `another orphans` runs | never | never. Any found is `info`: the count, and up to five by port and program name. A scan that did not run is `info` too |

The last two rows are [the orphan scan](#the-orphan-scan).

Doctor's server check accepts a successful response when its node id is missing or malformed, or when the local database identity is unavailable. In those cases the daemon row can say `healthy` without verifying identity. `another daemon status` requires a matching node id and reports `healthy no` for those same cases.

Statuses are `ok`, `info` (optional and not set up, or worth a look but not a problem), `warn`, `fail` and `skip` (does not apply here). The exit status is 1 only when a check is `fail`, so a script can gate on it; warnings exit 0.

## What it does not do

- **It changes nothing.**
  - It does not create the home or the database, and it does not migrate the database or write an event.
  - Every database read goes through read-only connections, which also cannot checkpoint the WAL into the main file.
  - It does not edit a hook config or touch the daemon; it only reads launchd's record and the plist on macOS, or `systemctl --user show` and the unit file on Linux. The daemon row is skipped on other platforms.
  - It runs no sweep: no hold, request or lease is expired, and no resource is released, adopted or reaped.
  - It stops and signals no process.
  - One exception: SQLite may leave the empty `another.db-shm` and `another.db-wal` files that any reader of the database creates.
- **It contacts nothing but loopback.**
  - There is exactly one network call, `GET /health`, to a literal loopback address (`127.0.0.1:PORT` or `[::1]:PORT`), with no proxy and no redirect followed.
  - A host name or any other address given with `--bind` is skipped, not looked up or dialled.
  - It never contacts a federation peer, a tunnel or any remote host. The `resources` row does not try to connect to another node's resource, which `another resources` does.
- **It runs no fix.** Each `fix:` is a command for you to run.
- **It runs no transport.** The `transports` row reads the config and looks for each program on `PATH`; it starts none of them, so it does not show that a command works.
- **Not checked:** tokens and OAuth grants, tunnels, Herdr, whether a configured tmux or cmux command works, and the resource scanner's permissions.

## How long it takes

Usually well under a second. It is not bounded overall:
- **Bounded.** A locked database is waited on for at most 2 s, and `/health` gets 2 s.
- **Not bounded.**
  - `quick_check` reads the whole database, so its time grows with the database's size.
  - The daemon row runs `id -u` and `launchctl print` on macOS or `systemctl --user show` on Linux, with no timeout of their own. A stuck service manager holds `doctor` up with it.
  - The orphan scan runs `lsof` or `ss` once, and `ps` once for each unregistered listener and each lapsed resource with a pid, with no timeout of their own. A stuck `lsof` holds `doctor` up with it, and the time grows with the number of unregistered listeners ([known limits](#the-orphan-scan)).

Press Ctrl-C to stop it; it has changed nothing.

## The orphan scan

The `resources` and `listeners` rows are the scan behind `another orphans` ([RESOURCES.md](RESOURCES.md#orphans)), run without the sweep that command starts with. They show what is left over; `another orphans` gives the full list, with ids, pids and command lines. Doctor names no command that stops or releases anything.

- **`resources`** counts this node's registered resources with no live lease and names up to five, with how long each has been lapsed. A resource another node owns is not listed; it is dealt with on that node.
  - Doctor reads what the last sweep recorded. This node's lease that ran out since then still counts as live until a sweep expires it, so its resource is not listed yet. `another serve` sweeps every 30 seconds, and `another sweep`, `another resources`, `another orphans`, `another inbox`, `another requests` and `another activity` each sweep when run. The hook packs read the inbox at each turn, so in a home with active agents the gap is short.
  - A resource whose only leases were another node's and have run out is listed as `not recorded by a sweep yet` in place of a time.
- **`listeners`** counts listening TCP sockets that match no registered resource by pid or port, and names up to five by port and program name. System services and Another itself are left out.
  - It is `info`, never a warning, and never changes the exit status. A dev server you started by hand and never registered is counted here; that is expected.
  - The row runs external programs, found on `PATH`: `lsof -nP -iTCP -sTCP:LISTEN -FpcLn`, or `ss -ltnpH` when `lsof` is missing or fails. Then `ps -o user=,etime=,command= -p <pid>` runs once for each unregistered listener that is not a system service, and once for each lapsed resource registered with a pid. It does not run for a listener that a registered resource accounts for.
  - When neither `lsof` nor `ss` can run, the row says the listener scan did not run and gives the reason. The `resources` row and the exit status are unaffected. Without `ps`, a listener is named as `lsof` or `ss` named it.
  - Output from `lsof` that is not its field output is read as no listener.
- **Known limits.**
  - None of these programs has a timeout. Doctor waits for `lsof` for as long as `lsof` takes.
  - `ps` runs for every unregistered listener, though only five are named, so doctor takes longer the more there are.
  - A program name is cut at its first space: a program under `/Applications/Some App.app/` is shown as `Some`. `another orphans` shows the whole command line.
- Names of resources and programs are printed as one line with control characters replaced by spaces.

## A damaged database

If `database` fails with a `quick_check` complaint:
- stop `another serve` and the daemon (`another daemon uninstall` on macOS);
- copy `another.db` **together with** its `-wal` and `-shm` files somewhere safe before trying anything else. Committed data can sit in the `-wal` file, so a copy of `another.db` alone may be missing it.

SQLite's `.recover` command (in the `sqlite3` shell) can often salvage the rows. If the damage followed an upgrade, restore the backup taken before it (next section).

## Upgrades and backups

A normal command brings an older database to its own schema. Three rules keep that safe.

- **A backup first.** Before any migration, the database is copied to `backups/another-v<schema>-<UTC time>.db` in the home and the copy is checked. If the copy fails, nothing is migrated. After a migration succeeds, three backups are kept: the one just taken and the two newest before it, by the time in their names. Older ones are deleted. Only files with exactly that name are ever deleted, so anything else you keep in `backups` stays. A backup whose name is dated in the future (the clock was wrong once, or you copied it in) is left alone and does not count as one of the three; delete it yourself when you no longer want it. A migration that fails is rolled back, and the backup it took is removed again, since the database it copied is still there. `backups` must be a directory: a symbolic link there is not followed, and the migration stops.
- **Only a schema the build knows.** A build refuses a database whose schema is newer than its own, and one whose tables are not what it has for that version (a different build changed them; the message names the tables). It says so and leaves the database file as it was. Every normal command reads the tables itself. Tool hooks and `another board` make the same check but skip the catalog queries when `build.json` shows the tables were already checked and the schema has not changed since. When a normal command refuses the tables, it removes that note, so hooks and `board` refuse them from then on too.
- **A development build keeps to its own homes.** A development build is one compiled without optimisations (`cargo build`, `cargo run`, `cargo test`), or a release-profile binary that is still in the Cargo build directory it was built in (`target/release/another` after `cargo build --release`; it is recognised by Cargo's lock file `.cargo-lock` beside the binary, whatever the directory is called). It refuses to migrate a home unless only development builds have ever created, migrated or run a normal command on it. Give it its own home with `ANOTHER_HOME=$(mktemp -d)`. `--dev-migrate` overrides this for one run; a backup is still taken first, and the next migration needs the flag again. `another --dev-migrate doctor` reports what a run with the flag would do.

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
