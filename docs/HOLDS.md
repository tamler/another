# Path holds

A hold is an agent's own only on the node that took it. Legacy rows whose hold, agent and project nodes disagree are inert and reported by `another doctor`; they are kept, never repaired automatically.

A hold says "I am changing these files". It is a lease on a path or glob inside a project, with a TTL (default 30 minutes, 1 minute to 4 hours). Holds do not lock files on disk. An exclusive hold conflicts with any overlapping hold of another agent in the same checkout; a shared hold conflicts only with exclusive ones. Agents in other worktrees or clones of the repository are never blocked by it (see [Holds and checkouts](#holds-and-checkouts)).

```bash
another hold acquire --agent claude@shop src/api/routes.rs
```

```bash
another holds --project .
```

Conflicts are all-or-nothing: if any requested path overlaps another agent's live hold, nothing is granted and the command exits with status 3. Session hooks renew an agent's holds and release them at session end.

## The edit guard

With the hook pack installed, agents do not have to remember to take holds. Before each file edit, the `pre-edit` hook takes a narrow exclusive hold on that file for the editing agent, or stops the edit and names the holder when another agent holds it. [HOOKS.md](HOOKS.md) has the details, the clients it covers, and what happens when the database is unavailable.

Automatic holds name files only. File targets are literal, even when a new or existing filename contains `[`, `{`, `*` or `?`; stored patterns escape these characters. Directory hints in tool input are skipped, and patch headers supply the exact files a patch changes. To hold a directory or use glob syntax, explicitly request it in `another hold acquire` or MCP `hold_paths`. Be careful: a directory hold blocks every file below it, including unrelated work. Prefer individual files whenever possible.

Manual holds also keep literal intent for bracketed or braced names anywhere in the pattern. A pattern is compared with a literal path one component at a time. A component matches when it is the same text, when it matches as a glob, or when it matches with `[`, `]`, `{` and `}` read as plain characters while `*` and `?` stay wildcards; `**` spans any number of components. An alternation is spelled out as the comparison reaches it, one alternative at a time however many the pattern chains, so `src/{a/b,c}` names the directories `src/a/b` and `src/c`, and `src/{a/b,c}/[id]/page.tsx` keeps `[id]` literal below either; the braces read as plain characters still count, so `app/{group}` also names a directory called `{group}`. A comparison that would take too many steps counts as an overlap rather than as free. An escape such as `[*]` or `[?]` is one plain character, so `packages/*/app/[*]` names the directory `*` under each app and covers `packages/web/app/*/page.tsx`. A pattern whose last component is a plain, bracketed or braced name (no `*` or `?` outside a character class) and that ends above a file names a directory, and that hold covers the file; `src/*` names the entries directly in `src` and nothing deeper, so use `src/**` for everything below. Thus `app/[id]`, `app/[id]/**`, `app/[id]/*.tsx`, `packages/*/app/[id]/page.tsx`, `app/{shop,blog}/[id]/page.tsx`, `src/{a/b,c}` and `app/[id]/**/[slug]/*.tsx` protect the matching literal files whichever hold comes first. The original glob interpretation still counts too, so `app/[ab]/page.tsx` also meets `app/a/page.tsx`.

A stopped edit automatically waits for the file. Do not keep retrying: repeat attempts get a short backoff, and Another sends one inbox ping after every conflicting hold clears, unless the waiter's session ends first, in which case the wait is cancelled and no ping is sent. The file is not reserved by that ping. See [Waiting for a held file](HOOKS.md#waiting-for-a-held-file) for timing and delivery.

To ask an owner to release a hold, use `another hold request-release --agent AGENT HOLD_ID` with an ID from `another holds` or a deny notice. The command reuses your outstanding request for that exact hold; it closes automatically when the hold is released or expires, including session end. A new hold is a new request. Unlinked free-text requests stay manual. This command is local CLI only; it does not add a remote release capability.

## Releasing a file when you are done

A hold taken by the edit guard lasts while its session keeps working, so a file edited once stays held. When you are done with a file, say so, and another agent in the same checkout can pick it up at once:

```bash
another hold release --agent claude@shop src/api/routes.rs
```

```text
released 1 hold(s)
```

The argument is a file or a directory, and it is resolved from the file, the way the edit guard resolves an edit: it works from any directory, through a symlink, with a relative or an absolute path, and for a file in another checkout that you edited by its absolute path. It releases your own holds on that file and nothing else. A directory releases your holds on the files below it and your glob holds whose literal directory is that directory or lies below it, also when the hold spells that directory with escaped brackets (`src/br[[]1[]]/**` for the directory `src/br[1]`); a glob with no literal directory, such as `**/*.rs`, stays. No project is registered by it; `--project` only names the directory relative paths start from.

- An argument that names an existing file or directory is a path, whatever characters its name has.
- An argument shaped like a hold id (26 characters, in upper case as `another holds` prints them) is a hold id, as before. A file whose whole name looks like one is given as `./NAME`.
- An argument made of letters and digits only that names no existing file or directory may be a mistyped hold id. It is still tried as a path, so a hold of yours on a file of that name that is not created yet is released. If no hold of yours names that path, covers it (it lies inside a wider hold of yours) or lies below it (a glob such as `build/**` for the name `build`), the command fails with status 1, releases nothing by path and names both readings. `./NAME` always means a path and never fails this way. Hold ids in the same command are released first; when a later argument then fails, the error ends with what is already released, for example `; 1 hold(s) already released: 01M7MB3VTGJPCE70WC7RB0B4SK`.
- Any other argument with a wildcard is a glob: it releases your hold with exactly that pattern in the project at the current directory or the nearest one above it.
- With no argument every hold under your name is released, as before.

```text
another: no live hold 01NOSUCHHOLD: it is not a hold id (26 characters) and no hold of yours names the path /Users/j/app/01NOSUCHHOLD; for a file of that name, give ./01NOSUCHHOLD
```

When a path has nothing to release, the command says why and exits with status 0:

```text
another: nothing of yours is held at /Users/j/app/src/a.rs (checkout /Users/j/app)
```

```text
another: src/a.rs is inside your hold src/** [01M7MB3VTGJPCE70WC7RB0B4SK]; release that hold to free it
```

MCP callers use `hold_paths` with `action: "release"`, `paths` and `project` (the directory relative paths start from). A remote caller's path must lie inside a project its token covers. For a symlink inside such a project, the answers to a remote caller differ with where the link leads (into a checkout, into another checkout or into none), so it can learn that much about the target; no path outside its projects is shown, and a remote caller cannot create the link.

## Holds and checkouts

A hold blocks only inside the checkout it was taken in. A checkout is one working directory:

- for a file in git, the nearest directory above it that has a `.git` entry (a directory, or the file a linked worktree or submodule has);
- for a file outside git, the registered project directory that contains it;
- a file in neither is not held.

A hold applies to a file exactly when the file's checkout is the hold's checkout and the hold's pattern, read from the top of that checkout, matches the file's path from there. Other checkouts are told nothing: the edit guard, `hold acquire` and `hold check` say nothing about holds elsewhere. `another holds` still lists every hold with its project, for anyone who looks, and marks the ones that were taken in another checkout than the one the command is run in:

```text
exclusive codex@w1         w1:src/a.rs  until 2026-11-17T13:00:30.288Z  (editing (taken by the edit hook))  [01M7MQSGWGDZMF8TB8BGYKW8QZ]
exclusive claude@another   another:src/a.rs  until 2026-11-17T13:00:30.299Z  (editing (taken by the edit hook))  [01M7MQSGWXSVM817P6JEDM8N8W]  other checkout
other checkout: taken in a different working directory from this one; it does not block edits here
```

A marked hold is not a reason to leave a file alone in your own checkout. With `--json` each hold carries the boolean `other_checkout`. The mark is judged from the current directory, also with `--project`. Run outside any checkout, the command marks nothing, prints no such line, and gives `other_checkout` as false throughout.

| Case | Result |
|---|---|
| The main checkout and a linked worktree, beside it or inside it (`.claude/worktrees/x`) | Two checkouts. No hold of one applies to a file of the other, also not `**` with `--repo-wide`. |
| Two clones, on any branches, also the same branch | Two checkouts. |
| A submodule or a nested repository | Its own checkout. An outer hold does not apply inside it. |
| A project registered at a subdirectory of a checkout | The same checkout as the top-level project. Paths are compared from the checkout's top, so a hold on `api/**` in a project at `src` overlaps `src/api/x.rs` in the top-level project. |
| A symlink in checkout A to a file in checkout B | The file is B's. A symlinked spelling of a whole checkout is the same checkout. |
| A checkout that switches branches | Keeps its holds: the unit is the directory, not the branch. |
| A checkout that is moved or renamed | A new project and a new agent name. Holds under the old path are not seen from the new one until they end. |
| One directory reached through two mount points, or through a macOS firmlink spelling (`/System/Volumes/Data/Users/...`) | Two checkouts when the two spellings do not resolve to one path. This is a known limit. |
| `git init` in a directory Another first saw outside git | Still treated as outside git. It matters only for a project registered at a subdirectory. |

The checkout is found from files, without running git, and kept in a node-local cache that is filled the first time a project is used. Holds on other machines are still matched by project name and reported as warnings, never conflicts (D26).

`another holds --project X` and the board also list holds in other checkouts of the same repository. They are other work on that repository and will not block you. Another tells one repository from another by its `origin` remote with the scheme, user name, password or token, port and `.git` suffix removed and the host lowercased, or, with no `origin`, by the shared git directory every worktree points at. Credentials are never stored.

### A hold that points into another checkout

A pattern whose directory lies in another checkout would protect nothing there, so it is refused, with the command that takes it in the right place:

```bash
cd ~/Projects && another hold acquire --agent claude@Projects 'test/another/src/**'
```

```text
another: test/another/src/** is inside another checkout (/Users/jacob/Projects/test/another); a hold taken here would not apply to its files. Take it there: another hold acquire --agent claude@Projects --project /Users/jacob/Projects/test/another 'src/**'
```

The same goes for a pattern from the main checkout that names a worktree inside it, such as `.claude/worktrees/x/src/**`, and for `hold check`. The directory is the part of the pattern before its first wildcard; one that does not exist yet is judged at its nearest existing parent. A pattern that starts with a wildcard has no such directory and is granted: it covers the files of its own checkout only.

A pattern whose directory leads, through a symlinked directory, out of the project to a place that is in no checkout (not in git and not under a registered directory) is refused the same way, because the edit guard holds nothing there:

```text
another: plainlink/sub/** points outside any checkout (/Users/jacob/plain/sub); a hold taken here would apply to nothing
```

A remote caller gets the same message without the directory.

A file that is itself a symlink is judged by where it points. The edit guard holds such a file where its name lives, and also at the file it points to when a hold can exist there.

- The link points to a place in no checkout and no registered directory, or to a file that does not exist yet: only the name is held, so `config.toml` in this checkout is held here, and `hold check` reports a conflict on it as for any file.
- The link points into another checkout: refused as "inside another checkout", with the command that takes the hold on the target there. The same goes for a file below a directory link into another checkout, whatever that file links to.

Two known limits remain. A file link into another checkout is refused although the edit guard does hold the link's name in this checkout. A link into a registered directory outside git is granted, and `hold check` reports no conflict, although the edit guard also holds the target there and can stop the edit for a hold in that directory.

## Symlinks

A path given to `another hold acquire` or `hold check`, and every file the edit guard sees, is resolved the way the filesystem resolves it before it is held or checked:

- `alias/a.rs` (with `alias -> src`) is `src/a.rs`;
- `..` applies after the symlink before it, so `link/../a.rs` is the parent of where `link` points, then `a.rs`;
- `..` out of a directory that does not exist yet goes back to following links, so `nope/../dangle.rs` is checked where `dangle.rs` points;
- a dangling link to a file that does not exist yet is checked at the file it would create;
- for a glob, its literal directories are resolved and the pattern is kept, so `alias/**` is held as `src/**`. A wildcard is not expanded, so a glob whose wildcard covers a link's own name (`al*/a.rs`) is held as written and does not protect `src/a.rs`. Hold the real directory instead;
- a path through a symlink loop cannot be opened, so it is checked as written, with only its existing directories resolved.

A path whose real location is outside the project is passed on as given. Hard links cannot be seen from a path: an edit through a second hard link to a held file is not stopped.

A project given to the CLI as a path (`--project /tmp/shop`, `another project ensure`, or the current directory by default) is resolved the same way, so a symlinked spelling names the project already registered at the real directory instead of adding a second one. A project registered earlier under a symlinked spelling keeps its row, and that spelling still finds it. Paths given to the MCP `ensure_project` tool are still only normalized as text (`.` and `..` removed), because a remote token's project scope is judged on the path as written.

## Letter case

An existing file is always checked under its on-disk spelling, so `SRC/A.RS` is `src/a.rs` wherever the volume treats them as one file.

A file that does not exist yet has no on-disk spelling. On a volume that ignores letter case (APFS and NTFS by default), holds in that project are compared without regard to case, so a held new `src/new.rs` also stops `src/NEW.RS`. Another tells by looking up the project directory under a different spelling, which writes nothing. On a case-sensitive volume (ext4, or APFS formatted case-sensitive) `src/new.rs` and `src/NEW.RS` are different files and do not collide. Letters are compared with Unicode lowercasing, which can differ from the volume's own rules for a few non-ASCII letters.

Ignoring case only adds collisions: anything that collides under exact spelling still does. A pattern with a character class, such as `src/[!a-z]*.rs`, is always compared exactly, because ignoring case would change which names the class lets in.

## Whole-project holds

A hold on the whole project blocks every other agent in the checkout, so it is refused unless asked for explicitly:

```bash
another hold acquire --agent claude@shop '**'
```

```text
another: "**" would hold the whole project (**) and block every other agent in the checkout; hold the files or directories you will change, such as src/api, or pass --repo-wide (MCP: repo_wide) if you really mean all of it
```

"Whole project" means `**`, `.` or the project root, and any glob that starts with a wildcard and contains `**`, such as `**/*.rs`. Holds like that taken before this rule keep working. `another hold check` still accepts them, since checking takes nothing.

A whole repository cannot be held from a directory above it. From `~/Projects`, `another hold acquire test/another` is refused like any pattern that points into another checkout, and the command it names carries `--repo-wide`:

```text
another: test/another is inside another checkout (/Users/jacob/Projects/test/another); a hold taken here would not apply to its files. Take it there: another hold acquire --agent claude@Projects --project /Users/jacob/Projects/test/another --repo-wide '**'
```

That hold then applies to that checkout only, not to its other worktrees or clones.

## Pattern size, and a pattern that cannot be read

A hold pattern is at most 4,096 bytes, the longest path Linux takes in one call (macOS stops at 1,024), so every real path fits. `another hold acquire`, `another hold check` and the MCP tool `hold_paths` refuse a longer one and write nothing:

```text
another: hold pattern is 4097 bytes; the limit is 4096
```

`another hold release <path>` refuses a glob over the limit with the same words. A plain path of any length is looked up as text, so a hold the edit guard took on a long path can still be released by path; when nothing matches it answers `nothing of yours is held at <path>` and exits 0.

The edit guard's own holds are not held to the limit. They are file names with `[`, `]`, `{`, `}`, `*` and `?` escaped (three bytes for each), compared as text and never compiled as a glob, so a long path cannot make an edit fail.

The same limit applies to a hold another node sends ([FEDERATION.md](FEDERATION.md#model)). No pattern makes a command panic. What a pattern can cost is bounded as follows; the times are from a release build on the development machine.

- **One comparison** of a glob with a path takes at most 2,000 steps: under 25 ms (the longest measured, over the costly shapes of two reviews, took 21 ms). The widest honest pattern measured takes 493 steps and the patterns in this repository's tests at most 37. Between two holds of this node, a comparison that runs out counts as an overlap, so the result is a conflict that a retry or a narrower pattern clears, never a missed one. For an agent's own hold it counts as "does not cover", so the edit guard takes a narrow hold on the edited file.
- **Holds of other nodes** never block, so comparing with them is best effort within one budget for the whole command: about 40 ms of estimated work for `another hold acquire`, the MCP tool and the dashboard snapshot, whatever a peer sent. It runs after the command's own write has been committed, so the comparison is never done while the command holds the database's write lock. The budget counts work, not time: measured against 10,000 holds of each costly shape tried, `hold acquire` took about 50 ms at most in all on a quiet machine, and more when the machine was busy. When the budget is spent the remaining holds are not compared, and the command says so in one line. At most ten overlapping holds are listed one by one, a pattern over 40 characters as its start and its length; the rest are a count.
- **What one command compares.** Holds on plain paths, which is what a peer's edits make, are compared as they are read, and a requested glob is matched against such a path directly. Measured with paths of about 50 bytes: 19,999 of a peer's holds are compared when a path is asked for and 19,991 to 19,995 when a glob is. A peer's glob holds are set aside and compared after the plain ones, and each costs a compile and, against a requested path, a walk: with 10,000 globs of about 30 bytes from a peer, 541 are compared when a path is asked for and 1,769 when a glob is. Reading a hold costs too (40,000 short patterns, or about 8,000 of 4 KB, use the whole budget), so a peer with that many holds stored ahead of another node's can keep the other's from being compared; the line above names the nodes. The dashboard shares one budget among all the holds of this node it shows. The direct match is used where it gives the walk's answer; the walk is kept for a requested glob whose last component has no wildcard or that has a `/` inside braces, for a path with a bracket or a brace in it, and for a path with fewer components than the glob has up to its first `**`.

```text
warning: 1000 hold(s) on other nodes were not compared with yours (1000 from vps): comparing with other nodes' holds stops at a fixed amount of work, so these may overlap (holds are node-local; they do not block you)
```

- **Storing a peer's holds.** Nothing is compiled when an event from another node is stored; only the length of its pattern is judged. A `sync` that pulls 10,000 holds with 4 KB globs took 1.25 s here, the same as before this limit existed (1.27 s), in pages of 500 events that each hold the write lock for about 60 ms. Whether a stored pattern compiles is found out when it is first compared, inside the budget above.
- **Not bounded:** how many holds a peer may send. `hold check` and the edit guard never read another node's holds, but expiring holds and listing them read every stored hold: with 10,000 holds of 4 KiB each from a peer, every command that takes or checks a hold took about 9 ms longer, and the dashboard snapshot about 100 ms in all. A pull stores each page of events under the write lock, and a page of 500 can take longer than the 3 seconds an edit hook waits before it denies the edit. A comparison between two holds of this node is bounded one at a time, not in total: a project with many costly globs held by local agents costs up to 25 ms for each.

A row in the database can still hold a pattern this build cannot read: a glob longer than the limit, written by an earlier build or by hand, or a glob that does not compile, which another node can send. Such a hold counts as overlapping every path in the project it is in, and nothing outside that project. For a hold on this node, `another hold acquire`, `another hold check` and the edit guard refuse what it overlaps, name the holder and the node, and say how to clear it. The pattern is shown as its first 40 characters and its length, never whole:

```text
nothing acquired
conflict: src/b.rs: hold 01M7W8JPVH7GWPJQ74323AW5CK of codex@shop on this node (mac) has a pattern that could not be read ("src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…" (2097156 bytes)), so it counts as overlapping every path in project shop. To clear it: another hold release --agent codex@shop 01M7W8JPVH7GWPJQ74323AW5CK
```

A hold from another node never blocks, so one with such a pattern gives a warning on every `hold acquire` in the project of the same name, until it ends:

```text
warning: src/b.rs: hold 01M7W7XR88R0RV9HCEERP6EA00 of mal@shop@vps on node vps has a pattern that could not be read ("src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…" (2097156 bytes)), so it counts as overlapping every path in project shop. It ends at 2026-11-20T11:30:00.000Z unless that node renews it; to clear it sooner, release hold 01M7W7XR88R0RV9HCEERP6EA00 on node vps and sync (holds are node-local; this one does not block you)
```

An agent's own hold with such a pattern covers none of its edits: the edit guard takes a narrow hold on the edited file as usual.
