# Another

![Another: several agents, one project, one machine.](docs/assets/banner.png)

**Several agents, one project, one machine.**

[![ci](https://github.com/tamler/another/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/tamler/another/actions/workflows/ci.yml) [![license Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE) [![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-informational)](Cargo.toml) [![platforms macOS and Linux](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-informational)](https://toanother.one/docs/install/) [![no GitHub Release yet](https://img.shields.io/badge/GitHub%20Release-none%20yet-lightgrey)](https://github.com/tamler/another/releases)

Another is a local-first coordination service for coding agents. It records
individual actors, admitted file edits, work claims, mail and shared resources.
It does not launch agents, stop filesystem writes, or reap processes automatically.
Free and open source, always. Apache-2.0; Rust MSRV 1.88.

## Status

The original seven milestones are on main. The approved SPEC-B7 actor/lease
cutover (one lease model, owner decision Q1) is implemented for the v0.1.0
single-node queue release. No release has been published.

Admitted pre-tag work on main: the Q31 security fixes (M9, tamler/another#281),
the FLE-23 consolidations M-A time helpers (#287), M-B crypto helpers with
constant-time digest compare (#283), M-C git-root/path/`shell_word` helpers
(#284) and M-E forged-event test helpers (#282). Hosted Windows CI runs the
workspace tests, including CLI thread exit-status checks, and the fresh-install,
hook-pack and daemon dry-run checks. The forced small-stack test uses Unix
`ulimit`; Windows exercises the CLI with its native main-thread stack. Install
upgrades from the pre-Windows schema-v8 build are checked on Linux and macOS.
See [the v0.1.0 scope](docs/V0.1.0-SCOPE.md).

Temporary rights use **one 120-second default TTL**, with a requested checkout
duration clamped to **30 seconds through 24 hours**. Every checkout stores and
returns its duration. Expiry is effective immediately, without a sweep.
Successful completion renews only the exact admitted file generations; activity
on another file, a prompt, presence or process liveness cannot renew them.
Observed authenticated session end releases its dependent scope tree immediately.
There is no dormant countdown or keep-resource exception.

Names, `ANOTHER_AGENT` and `--agent` are selectors, never actor authority.
The default CLI refuses name-only mutation with `NO_ACTOR`. Private integrations
use the versioned `another.agent/1` channel through `another adapter event`.
File, resource, queue and request compatibility rows are projections of common
leases, with authenticated scope cleanup. Results, messages and history survive.

The reference hook adapter persists private grants and actor records, admits
structured exact-target edits, correlates completion and releases observed
sessions. Routine hooks emit only conflict notices and the short idle reminder.
Opaque shell tools are refused until a tested exact-target/private call route
exists. A native callback cannot infer a resumed epoch from a public session id:
retained private proof supports explicit resume; callbacks without current epoch
carriage refuse instead of adopting it. These are stated coverage limits, not
claims of full native runtime or human acceptance.

Cursor remains installed and **unguarded**. Its live
`2026.11.22-e9042b2` mapping is recorded; shell/CLI mutations remain
`NO_ACTOR` until tested private carriage exists. Herdr/Pi private per-agent
callback integration and stronger native shell coverage still need their actual
runtime evidence. See [the contract and coverage](docs/AGENT-CONTRACT.md).

Legacy records and experimental clients have an explicit operator repair mode:
`--unguarded-repair`. It explicitly leaves guarded participation and
cannot act as a minted actor by name. Installed hooks never select it and no
error falls back to it. Old repair fixtures are not B7 conformance evidence.

## Install and inspect

Build from this checkout:

```sh
cargo install --locked --path crates/another
another init
another adapter status
```

Install reference hooks only when you intend to configure that client:

```sh
another hook install --client claude
another hook install --client codex
another hook status
```

For Codex, review and trust new or changed hooks in `/hooks`. Configuration
presence does not prove trust or execution. Another cannot grant trust and does
not recommend `--dangerously-bypass-hook-trust`.
See [hook coverage](docs/HOOKS.md).

A development build must use its own throwaway `ANOTHER_HOME`, never the live
home. Schema migration takes a verified backup, is transactional, and refuses
unknown/newer schemas. Quiesce participating writers and upgrade the binaries,
daemon and integrations together. Legacy claims are disposed at cutover, not
adopted by name. [Upgrade safety](docs/DOCTOR.md).

After upgrading, a missing working directory used in a weak wake match is
resolved through its existing physical parent, including on Windows where that
can restore a verbatim prefix and on-disk letter case. A cwd value stored by an
older process may miss that match until the session's hook runs again and
restamps it. See [docs/WAKE.md](docs/WAKE.md#sessions). Upgrade a federation hub
and all its spokes together; see
[docs/FEDERATION.md](docs/FEDERATION.md#upgrading).

## Integrating an agent

A local integration can enroll with `another adapter enroll`. That command
writes its grant under the private adapter directory and prints the location,
not the secret. Keep the grant, returned actor proof and call contexts in the
integration backend, outside model context, argv, ordinary environment and logs.

`another adapter event` reads one typed envelope on stdin and returns JSON.
Register a root/child under the participant grant before admission. Checkout,
check, renew, release and completion carry actor proof, epoch and exact lease or
call references. Resume keeps identity and opens a new epoch; old callbacks cannot
change the new epoch. The facade also exposes authenticated hold/resource,
queue-next/result, mail and request operations. It does not select an actor from
a supplied display name. [Protocol](docs/AGENT-CONTRACT.md).

The default MCP catalogue contains only `another_identity` and
`another_lease`. An unbound connection reports/refuses `NO_ACTOR`; it cannot
substitute its shared root or a model-selected id. The historical catalogue is
available only when an operator explicitly starts a legacy repair service.

## Guides

- [Actor contract, leases and coverage](docs/AGENT-CONTRACT.md)
- [Read-only actor, lease and operation status](docs/STATUS.md)
- [Path holds](docs/HOLDS.md), [hook packs](docs/HOOKS.md)
- [Work queue](docs/QUEUE.md), [requests and mail](docs/REQUESTS.md)
- [Resource registry and physical cleanup](docs/RESOURCES.md)
- [Wake transports](docs/WAKE.md), [owner needs](docs/NEEDS.md)
- [Glossary](docs/GLOSSARY.md), [decisions](docs/DECISIONS.md)
- [Daemon](docs/DAEMON.md), [doctor and backups](docs/DOCTOR.md)
- [Federation](docs/FEDERATION.md), [experimental remote peers](docs/REMOTE.md)

Hub/spoke federation remains the existing signed topology. Old imported ownership
is historical/informational and never a grant of a new local actor's rights.
The hub can read relayed mail; use HTTPS or a private network for link secrecy.
Remote peers cannot reap. No deployment, DNS, credential or publication change is
part of this cutover.

`another serve --federation` without `--auth` serves only `/fed/*` and
`/health`, even when a proxy rewrites `Host` to loopback, and never mounts the
dashboard. Add `--auth` for token-gated MCP and activity routes; an explicitly
enabled dashboard on that federation listener also needs a token. Unauthenticated
rejection audits are capped at 32 rows per server store per 60 seconds. Wake
subjects and edit-denial hold values use the hook quoting rules. Foreign
resource checks use IP literals only and a bounded probe budget.

## Development

```sh
export ANOTHER_HOME="$(mktemp -d)"
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked --no-fail-fast
cargo clean
rm -rf "$ANOTHER_HOME"
```

Some process/signal tests deliberately refuse local macOS through
`hosted_ci::require_hosted_runner`; run them on GitHub-hosted Linux/Windows.
Do not bypass or weaken that guard. A local pass excludes only those exact
refusals, never another failure. CI also checks MSRV 1.88 and copy lint.
Automated, native runtime/device, independent review and human acceptance are
separate evidence.

For Rust callers, `another_core` exports `now_ms`, `fmt_ts`, `parse_ts` and
`parse_duration`. The duration helper returns seconds and lets the caller choose
whether to accept bare numbers. CLI durations still accept bare seconds; activity
`--since` durations still require a suffix (`s`, `m`, `h`, `d` or `w`). Federation
requests use the store's clock, including a manual clock supplied by a caller.

The website source lives in [site/](site/README.md). `sh site/build.sh` checks
the pages, and `python3 scripts/check-copy.py` rejects em dashes and a
banned-phrase list.

## Clean-room

Implementation uses this repository's docs and public integration docs only.
Do not copy restricted code, schemas, SQL, phrasing or distinctive APIs from
`mcp_agent_mail*` or projects with non-permissive riders. Call GPL cmux only as
a separate CLI process; never link or vendor it.
