# Path holds

A hold says "I am changing these files". It is a lease on a path or glob inside a project, with a TTL (default 30 minutes, 1 minute to 4 hours). Holds do not lock files on disk. An exclusive hold conflicts with any overlapping hold of another agent; a shared hold conflicts only with exclusive ones.

```bash
another hold acquire --agent claude@shop src/api
```

```bash
another holds --project .
```

Conflicts are all-or-nothing: if any requested path overlaps another agent's live hold, nothing is granted and the command exits with status 3. Session hooks renew an agent's holds and release them at session end.

## The edit guard

With the hook pack installed, agents do not have to remember to take holds. Before each file edit, the `pre-edit` hook takes a narrow exclusive hold on that file for the editing agent, or stops the edit and names the holder when another agent holds it. [HOOKS.md](HOOKS.md) has the details, the clients it covers, and what happens when the database is unavailable.

## Holds across worktrees

Each checkout is its own project, but holds are checked across every local project in the same git repository. Two worktrees of one repository, or two clones of one origin, collide on the same files:

```text
nothing acquired
conflict: src/api/routes.rs is held by codex@shop-review (shop-review:src/api/**, exclusive until 2026-11-10T16:04:00.000Z)
```

Another works out the repository from files, without running git:

- the key is the `origin` remote with the scheme, user name, password or token, port and `.git` suffix removed, and the host lowercased, so `git@GitHub.com:team/shop.git` and `https://github.com/team/shop` match. The path keeps its case: a Git server may treat `Team/Shop` and `team/shop` as two repositories. Credentials are never stored;
- with no `origin`, the key is the repository's shared git directory, which every worktree of it points at;
- a project registered at a subdirectory of a repository compares paths from the repository's top level, so a hold on `api/**` in a project at `src` overlaps `src/api/x.rs` in the top-level project.

The link is a node-local cache, refreshed whenever a project is used. Projects registered before this existed are linked the first time their holds are checked. `another holds --project X` lists holds in sibling checkouts too, since those are the ones that will conflict. Holds on other machines are still matched by project name and reported as warnings, never conflicts (D26).

## Symlinks

A path given to `another hold acquire` or `hold check`, and every file the edit guard sees, is resolved the way the filesystem resolves it before it is held or checked:

- `alias/a.rs` (with `alias -> src`) is `src/a.rs`;
- `..` applies after the symlink before it, so `link/../a.rs` is the parent of where `link` points, then `a.rs`;
- a dangling link to a file that does not exist yet is checked at the file it would create;
- for a glob, its literal directories are resolved and the pattern is kept, so `alias/**` is held as `src/**`.

A path whose real location is outside the project is passed on as given. Hard links cannot be seen from a path: an edit through a second hard link to a held file is not stopped.

## Whole-project holds

A hold on the whole project blocks every other agent in the repository, so it is refused unless asked for explicitly:

```bash
another hold acquire --agent claude@shop '**'
```

```text
another: "**" would hold the whole project (**) and block every other agent in the repository; hold the files or directories you will change, such as src/api, or pass --repo-wide (MCP: repo_wide) if you really mean all of it
```

"Whole project" means `**`, `.` or the project root, and any glob that starts with a wildcard and contains `**`, such as `**/*.rs`. Holds like that taken before this rule keep working. `another hold check` still accepts them, since checking takes nothing.
