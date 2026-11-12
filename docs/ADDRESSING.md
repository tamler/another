# Addressing agents: when a bare name is ambiguous

An agent's full address is `name@project`. Names are unique per project, not per machine, so the same name can exist in several projects on one node. A bare `name` resolves only while exactly one agent on this node has it. The moment a second project registers the same name, every command that was passing the bare name stops with an `ambiguous` error, and the fix is to use the full address.

This page is a recorded run, not a sketch. It was captured on 12 Nov 2026 from `another 0.1.0` built from commit `5ec0a58` of this repository, in a temporary `ANOTHER_HOME` and `HOME`, against three throwaway git repositories, with no hooks installed and no `another serve` running. Ids and timestamps are as recorded. The temporary root is shown as `/tmp/demo`; the real path was longer. Nothing here touched a real node.

## Setup: one name in two projects

Two repositories, `shop` and `billing`. Codex is registered in both; Claude only in `shop`.

```text
$ cd /tmp/demo/work/shop
$ another init
node      01M77J1FRB7X87PJWN1KEGQGZF (local, home)
database  /tmp/demo/another/another.db
projects  0
agents    0
$ another agent register claude --program claude --project .
registered claude@shop (claude)
$ another agent register codex --program codex --project .
registered codex@shop (codex)
$ cd /tmp/demo/work/billing
$ another agent register codex --program codex --project .
registered codex@billing (codex)
$ another agent list
claude@shop              claude    active   last seen 2026-11-12T09:39:52.213Z
codex@billing            codex     active   last seen 2026-11-12T09:39:52.230Z
codex@shop               codex     active   last seen 2026-11-12T09:39:52.222Z
```

The project part of an address is the project's slug. Another derives it from the directory name when the project is first registered: lower-cased, with anything that is not a letter, digit, `-` or `_` replaced by `-`, then with leading and trailing `-` stripped. A name with nothing left after that gets the slug `project`. If another project on the node already has the slug, a numeric suffix is added, as the second `shop` below shows. So `Invoice API` becomes `invoice-api`, and a directory called `-shop-` starts from `shop`, not `-shop-`; the [slug cases](#how-a-slug-is-chosen) section below shows each of these on the real binary. Do not derive the slug by hand when it matters: `another project list` prints every slug on the node next to its absolute path, and `another agent list` shows every full address, so those two are the first things to run when a name stops working.

## The error

Both the acting agent (`--agent`, `--from`) and a recipient (`--to`) go through the same resolution, so both fail the same way.

```text
$ another inbox --agent codex
another: agent name "codex" is ambiguous: it is registered in several projects; use name@project, one of: codex@shop, codex@billing
$ another send --from claude --to codex --subject "Review the invoice parser" --body "src/invoice.rs"
another: agent name "codex" is ambiguous: it is registered in several projects; use name@project, one of: codex@shop, codex@billing
```

Both exit with status 1. Nothing was sent. The error lists the candidates, so the fix is a copy from that line. With `--json` the same failure is one object on stdout, and the MCP tools return the same `ambiguous` code. This line was captured later in the run, after a third project had registered the name, which is why it lists three:

```text
$ another inbox --agent codex --json
{"error":{"code":"ambiguous","message":"agent name \"codex\" is ambiguous: it is registered in several projects; use name@project, one of: codex@shop, codex@billing, codex@shop-2"}}
```

## The fix: `name@project`

```text
$ another send --from claude@shop --to codex@billing --subject "Review the invoice parser" --body "src/invoice.rs"
sent 01M77J1FT561NTTVYQPS8FBM9S to codex@billing
$ another inbox --agent codex@billing
codex@billing: 1 unread (0 urgent), 0 awaiting ack, 0 request(s)
   new  2026-11-12T09:39:52.261Z  claude@shop      Review the invoice parser  [01M77J1FT561NTTVYQPS8FBM9S]
$ another inbox --agent codex@shop
codex@shop: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
```

The mail went to the Codex in `billing` and only there. A name that is still unique keeps working bare: `claude` has one registration, so `another inbox --agent claude` answers as `claude@shop`.

```text
$ another inbox --agent claude
claude@shop: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
```

Prefer the full address anyway, in scripts and in prompts. A bare name that works today is one `agent register` away from breaking.

## Two repositories with the same directory name

Slugs are unique per node. A second project whose directory is also called `shop` gets the slug `shop-2`, and its agents are `name@shop-2`.

```text
$ cd /tmp/demo/scratch/shop
$ another agent register codex --program codex --project .
registered codex@shop-2 (codex)
$ another agent list
claude@shop              claude    active   last seen 2026-11-12T09:39:52.261Z
codex@billing            codex     active   last seen 2026-11-12T09:39:52.230Z
codex@shop               codex     active   last seen 2026-11-12T09:39:52.222Z
codex@shop-2             codex     active   last seen 2026-11-12T09:39:52.295Z
$ another inbox --agent codex@shop-2
codex@shop-2: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
```

Which `shop` is which? `another status` and `another agent list` print addresses, not directories. `another project list` prints each slug with the absolute path it was registered from, so run it before picking an address:

```text
$ another project list
billing  /tmp/demo/work/billing
shop  /tmp/demo/work/shop
shop-2  /tmp/demo/scratch/shop
```

Here `shop` is `work/shop` and `shop-2` is `scratch/shop`, so the Codex in the scratch checkout is `codex@shop-2`. The project part also accepts the project's absolute path, which Another answers with the canonical address:

```text
$ another inbox --agent codex@/tmp/demo/scratch/shop
codex@shop-2: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
```

The numbered slug is assigned when the project is first registered and does not change, so the second `shop` stays `shop-2` for the life of the node.

## How a slug is chosen

Three more directories registered in the same node, with `another project ensure`, which registers a project without an agent and prints its slug and path. `-shop-` loses its hyphens and starts from `shop`; that is taken twice already, so it gets `shop-3`. `Invoice API` becomes `invoice-api`. A name of nothing but hyphens has nothing left and gets `project`.

```text
$ another project ensure /tmp/demo/edge/-shop-
shop-3  /tmp/demo/edge/-shop-
$ another project ensure "/tmp/demo/edge/Invoice API"
invoice-api  /tmp/demo/edge/Invoice API
$ another project ensure /tmp/demo/edge/---
project  /tmp/demo/edge/---
$ another project list
billing  /tmp/demo/work/billing
invoice-api  /tmp/demo/edge/Invoice API
project  /tmp/demo/edge/---
shop  /tmp/demo/work/shop
shop-2  /tmp/demo/scratch/shop
shop-3  /tmp/demo/edge/-shop-
```

The suffix counts from 2 and skips taken slugs, so the order projects are registered in decides who is `shop` and who is `shop-3`. An agent registered in `/tmp/demo/edge/-shop-` would be `name@shop-3`.

## Set the identity once

`--agent` and `--from` default to `$ANOTHER_AGENT`. Set it to the full address for a shell or a session and leave the flags off:

```text
$ ANOTHER_AGENT=codex@billing another inbox
codex@billing: 1 unread (0 urgent), 0 awaiting ack, 0 request(s)
   new  2026-11-12T09:39:52.261Z  claude@shop      Review the invoice parser  [01M77J1FT561NTTVYQPS8FBM9S]
```

The installed hooks never hit this: a hook acts as `<client>@<project>` for the checkout the client is running in, which is already a full address ([HOOKS.md](HOOKS.md)). Two Claude Code sessions in two repositories are `claude@shop` and `claude@billing` without any setup. The ambiguity appears when a person or a script types the bare name, and when `ANOTHER_AGENT` is set to a bare name that later stops being unique; in that case the hook fails open and the session gets no digest.

## Other resolution errors

| Message | Meaning | Fix |
|---|---|---|
| `agent name "codex" is ambiguous: ... one of: codex@shop, codex@billing` | More than one project has the name | Use one of the listed addresses |
| `unknown agent "codex@payments"; register it with register_agent first (or sync with its node)` | No project on this node has that slug or path, and no paired node is known by that name | Check `another agent list`; register the agent, or fix the slug |
| `invalid agent address "codex@shop@local@x": use name, name@project or name@project@node` | More than two `@` | Use one of the three forms |
| `codex@shop@laptop belongs to another node; agents act only on their own node` | The address is valid but the agent lives on a paired node | A remote agent can receive mail; it cannot be `--agent` or `--from` here |

The last row is the form of the error in the code; it was not part of this run, which had a single node.

## Across nodes

`name@project` looks at this node's projects. If nothing matches, Another tries the part after the `@` as a node name for a project-less agent on a paired node, which is why the unknown-project message mentions syncing. To reach an agent on another node, write all three parts, `name@project@node`, or `name@@node` for an agent registered there without a project ([FEDERATION.md](FEDERATION.md)). `name@project` does not pick between two nodes that both have a `codex@shop`; the node part does. Requests stay on one node either way; cross-node addresses carry plain mail only.

## What was run

Three `git init` repositories under one temporary root, one `another init`, four `agent register` calls, then the `inbox`, `send`, `agent list` and `status` commands shown above, in one continuous run. The error lines are copied from stderr. Timestamps are UTC, as the CLI prints them.

The `another project list` output and the slug cases come from a second run on the same day, same setup, with `another 0.1.0` built from commit `d474de9`. The slug code did not change between the two commits. That run registered the same three repositories and agents, then three more directories with `project ensure`. The paths are shown as `/tmp/demo` as above.
