# External commands: `another <name>`

`another <name> [args...]` runs an executable called `another-<name>` when `<name>` is not one of Another's own commands. It works the way `git <name>` finds `git-<name>`. This is the first, smallest extension point from roadmap #10: there is no plugin loader, no registry, no event feed and no policy hook.

## Write one

Any executable works. This one prints a short report from the read-only work board:

```sh
#!/bin/sh
# another-busy: which agents hold something right now
another --json board "$@" | jq -r '.projects[] | .project as $p | .holds[] | "\($p)  \(.agent)  \(.pattern)"'
```

Save it as `another-busy` in a directory on your `PATH`, make it executable (`chmod +x another-busy`), then run `another busy`. On Windows the file must be `another-busy.exe`.

## The rules

- **Built-in commands always win.** `another status` is Another's `status` even if `another-status` is on `PATH`. The same holds for every alias and for `help`. A new built-in in a later release takes the name over from an external command with the same name.
- **The name comes first.** `another busy --json` runs `another-busy --json`. In `another --json busy`, `busy` is not the first argument, so it is not looked up; Another reports an unknown command. Options before the name are not passed on. To point an external command at another data directory, set `ANOTHER_HOME` in its environment.
- **Plain names only.** `<name>` must be 1 to 63 characters, using lowercase ASCII letters, digits and hyphens, and must not start with a hyphen. So it can never be a path (`../x`), an option, a hidden file or another case spelling of a built-in.
- **Where it looks.** Only absolute entries in `PATH` are searched, in order, and the first match is run. An empty entry or `.` is skipped, so a file in the current directory is never run because of how `PATH` is written. On Unix the file must be a regular file with an execute bit. On Windows only `another-<name>.exe` is run, never a `.bat` or `.cmd` (those would go through `cmd.exe`).
- **How it runs.**
  - The arguments after the name are passed unchanged, with no shell: `$(…)`, `;` and `*` stay literal.
  - Stdin, stdout and stderr are the terminal's.
  - The exit status is the external command's. On Unix, Another replaces itself with the command (`exec`), so a signal is reported the same way.
  - If the file exists but cannot be started, Another prints `could not run <path>` and exits 126.
- **Not found.** If no `another-<name>` exists, the result is the same as before this feature: `unrecognized subcommand`, with a spelling suggestion, exit 2.

## What Another gives it, and what it does not

- Another adds nothing to the environment and passes no database path, token or message body. The command sees the environment you started `another` with, exactly as if you had run `another-<name>` yourself.
- Another does not open, create or migrate the database before running it.
- There is no time or output limit. The command is interactive, like any program you run.
- The external command talks to Another through the documented CLI (`another --json …`) or MCP. The SQLite schema, internal types, federation formats and the text (non-JSON) output are not a stable interface. Treat `--json` output as unstable as well, until a versioned extension contract exists.

## Trust

An external command is a program you chose to install. It runs with your user's permissions: it can read your files and credentials and use the network. It is not sandboxed, and Another does not check where it came from. Install only commands you have read or trust, in a directory only you can write to. Dispatch adds nothing beyond running the program yourself. One thing does differ: a directory on `PATH` that someone else can write to now also lets them add `another <name>` commands. Keep `PATH` to directories only you can write to.

## Not in this step

These are in the [plugins proposal](ROADMAP.md) and each needs its own spec:

- listing external commands (`another help` does not show them);
- MCP tool add-ons;
- event or webhook subscribers;
- hold and request policy hooks.
