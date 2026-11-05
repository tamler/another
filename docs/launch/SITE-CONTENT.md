# Site content round

Source only, stacked on `launch/polish`. Nothing here was deployed or posted. Checked 2026-11-05.

## New pages

| Route | Job | Search intent |
|---|---|---|
| `/docs/install/macos/` | Command line tools, Rust, client `PATH` | install on macOS |
| `/docs/install/linux/` | Compiler, rustup, `PATH`, scan tools | install on Linux |
| `/docs/install/windows/` | No native build; WSL 2 route, untested | Windows support |
| `/docs/faq/` | Cost, privacy, delivery timing, removal | direct questions |
| `/docs/concepts/` | Glossary with an anchor per term | what a term means |
| `/compare/alternatives/` | Other options; when Another is wrong | alternatives |
| `/guides/git-worktrees/` | What crosses worktrees | agents in worktrees |
| `/guides/hand-off-tasks/` | Request ownership and takeover | agent task handoff |
| `/guides/share-model-server/` | Resource leases and cleanup | duplicate model servers |
| `/guides/wake-idle-agents/` | tmux nudge and its limits | idle agent mail |
| `/guides/agents-on-two-machines/` | Hub, pairing and what is shared | agents across machines |

`/docs/install/` stays the canonical page for Cargo, source builds, archives, checksums and binary replacement. The OS pages hold only what differs per system and link back.

## Checker additions

`site/check.py` now also requires that every route is reachable by following links from `/`, that FAQ structured data appears word for word on the page, and that nested pages carry the expected breadcrumb trail. `FAQPage` markup describes the visible text; no rich result is expected or claimed.

## What was run

| Claim | Evidence |
|---|---|
| Linux prerequisites | Clean `ubuntu:24.04` on arm64: `apt` candidate `rustc` 1.75; install fails with `linker cc not found` until `build-essential`, then passes |
| MSRV and removal | `rust:1.88-slim` on arm64: `cargo install --locked --git`, then `cargo uninstall` |
| Requests, resources, holds, worktrees | The published command blocks, run on macOS arm64 in temporary homes with expected results |
| Wake | A real tmux pane running a stand-in program |
| Federation | Two Linux containers on a Docker network: pairing, sync, mail both ways, request refusal, hold warning, borrow, revoke |
| Layout | DOM overflow measured at 390 and 1440 wide; pages viewed in Chrome at about 570 wide, dark theme |

## Not run

Windows and WSL; x86_64 Linux; a clean Mac; a tunnel or TLS in front of a hub; two physical machines; a real coding client reacting to a wake; the `xattr` step, which needs a real download. Each page that depends on one of these says so. Third-party details come from the linked public pages and were not tested.

## Found along the way

- **Remote borrow by name:** `another borrow kind:name` reports `no registered resource` for a resource on another node, while its ID works. `docs/RESOURCES.md` says either form addresses a resource. The two-machines guide documents the ID form. It should be corrected there if the behavior is fixed.
- **Reap and shell access:** `docs/RESOURCES.md` says agents cannot reap. That holds for MCP. The CLI's `--confirm` can't tell who is typing, so an agent with shell access could run it. The FAQ and resource guide say this plainly.

## Left as is

`docs/WAKE.md` and `another wake --help` still describe the wake skip rule as "no open requests". The code also counts stale requests and ones the agent has claimed, as the site pages now say.
