# CLI reference

Generated from `another … --help` by `scripts/gen-docs.py`; do not edit by hand. CI fails when this file is out of date or a command has no description.

| Command | What it does |
|---|---|
| [`another init`](#another-init) | Create the data directory and database if needed; show the local node |
| [`another status`](#another-status) | Show the local node, database location and registered agents |
| [`another doctor`](#another-doctor) | Check the setup: store, server, tokens, client hooks (and Codex hook trust), ports and resources. Prints PASS / WARN / FAIL with a fix for each problem; exits 1 on FAIL |
| [`another serve`](#another-serve) | Run the daemon: MCP over loopback streamable HTTP (default) or stdio |
| [`another project`](#another-project) | Register projects or list them |
| [`another project ensure`](#another-project-ensure) | Register a project by path (default: current directory) |
| [`another project list`](#another-project-list) | List registered projects |
| [`another agent`](#another-agent) | Register agents, list them, or show one |
| [`another agent register`](#another-agent-register) | Register or update an agent |
| [`another agent list`](#another-agent-list) | List registered agents |
| [`another agent show`](#another-agent-show) | Show one agent: its program, status and when it was last seen |
| [`another agent kick`](#another-agent-kick) | Remove an agent now: revoke all its tokens, release its holds and leases, tell its lanes |
| [`another agent remotes`](#another-agent-remotes) | Agents that joined through invites, with presence (online, idle, gone) |
| [`another send`](#another-send) | Send a message |
| [`another reply`](#another-reply) | Reply to a message within its thread |
| [`another inbox`](#another-inbox) | Show an agent's inbox and open requests |
| [`another ack`](#another-ack) | Mark messages read and acknowledged |
| [`another threads`](#another-threads) | List threads an agent is part of |
| [`another thread`](#another-thread) | Show every message in a thread |
| [`another requests`](#another-requests) | List requests |
| [`another request`](#another-request) | Claim, release, complete or cancel a request |
| [`another hold`](#another-hold) | Acquire, check, renew or release path holds |
| [`another hold acquire`](#another-hold-acquire) | Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted) |
| [`another hold check`](#another-hold-check) | Report conflicts for paths without acquiring (exit code 3 on conflict) |
| [`another hold renew`](#another-hold-renew) | Renew your holds (all, or the given ids) |
| [`another hold release`](#another-hold-release) | Release your holds (all, or the given ids) |
| [`another holds`](#another-holds) | List live path holds |
| [`another heartbeat`](#another-heartbeat) | Session heartbeat: renew holds; with --end release them and go offline |
| [`another resources`](#another-resources) | Find registered resources (model servers, dev servers, ports, GPUs, containers, jobs) |
| [`another resource`](#another-resource) | Register a resource or renew resource leases |
| [`another resource register`](#another-resource-register) | Register a running resource and take the owner lease |
| [`another resource heartbeat`](#another-resource-heartbeat) | Renew your resource leases (all, or the given ids / kind:name) |
| [`another borrow`](#another-borrow) | Borrow a shared resource (id or kind:name) |
| [`another release`](#another-release) | Release your lease on a resource (id or kind:name) |
| [`another orphans`](#another-orphans) | List orphan candidates: lapsed resources and unregistered listeners |
| [`another reap`](#another-reap) | Reap an orphan (resource id, kind:name, or pid:N). Without --confirm, only shows the plan |
| [`another token`](#another-token) | Create, list or revoke bearer tokens for remote peers |
| [`another token create`](#another-token-create) | Create a token that acts as one agent within project globs. Printed once |
| [`another token list`](#another-token-list) | List tokens: id, agent, project globs, status and last use (never the secret) |
| [`another token revoke`](#another-token-revoke) | Revoke a token by id (takes effect on the next request) |
| [`another audit`](#another-audit) | Show the audit log of remote (token-authenticated) calls |
| [`another node`](#another-node) | Show or rename this node |
| [`another node show`](#another-node-show) | Show this node's id, name and key fingerprint |
| [`another node name`](#another-node-name) | Rename this node (do this before pairing; names must be unique across nodes). Without a name, prints the current one |
| [`another pair`](#another-pair) | Pair nodes for federation: invite (hub), join (spoke), revoke |
| [`another pair invite`](#another-pair-invite) | Hub: print a single-use invite code for a spoke |
| [`another pair join`](#another-pair-join) | Spoke: redeem an invite code and record the hub |
| [`another pair revoke`](#another-pair-revoke) | Stop accepting (or using) a paired node, by name or id |
| [`another peers`](#another-peers) | List paired nodes |
| [`another sync`](#another-sync) | Spoke: push this node's events to the hub and pull everyone else's |
| [`another wake`](#another-wake) | Nudge agents with unread mail through tmux, cmux or Herdr (optional adapters) |
| [`another wake set`](#another-wake-set) | Store where to wake an agent (hooks capture tmux/cmux targets automatically) |
| [`another wake clear`](#another-wake-clear) | Forget an agent's wake target |
| [`another wake list`](#another-wake-list) | List stored wake targets |
| [`another lane`](#another-lane) | Lanes: named units of work with a charter, lead, status and structured reports |
| [`another lane create`](#another-lane-create) | Create a lane (status starts at planned) |
| [`another lane list`](#another-lane-list) | List lanes (open ones unless --all or --status) |
| [`another lane show`](#another-lane-show) | Show a lane with links and recent reports |
| [`another lane update`](#another-lane-update) | Change status, charter or lead, or link threads / requests / holds / messages |
| [`another lane report`](#another-lane-report) | Post a status report (also mailed to the lane's counterpart) |
| [`another lane done`](#another-lane-done) | Post the DONE report: outcome and optional PR link; closes the lane |
| [`another lane close`](#another-lane-close) | Close a lane without a report (done, or abandoned with --abandon) |
| [`another lane export`](#another-lane-export) | Write a lane as markdown: to stdout, --out FILE, or --dir DIR (<name>-STATUS.md, plus DONE-<name>.md when done) |
| [`another config`](#another-config) | Node settings: `another config` lists them, `another config KEY` shows one, `another config KEY VALUE` sets it (e.g. `another config decider jacob`) |
| [`another decision`](#another-decision) | Decisions: structured questions from agents to a person, answered here or by the manager bot |
| [`another decision ask`](#another-decision-ask) | Ask a person (default: `another config decider`, else jacob) a question with 2-6 options |
| [`another decision list`](#another-decision-list) | List decisions (open ones unless --status or --all) |
| [`another decision show`](#another-decision-show) | Show a decision; --markdown prints the short phone-readable form |
| [`another decision answer`](#another-decision-answer) | Answer a decision: an option value (or its number), and/or --text if allowed |
| [`another decision cancel`](#another-decision-cancel) | Withdraw a decision you asked |
| [`another invite`](#another-invite) | Invites for remote agents: single-use codes that admit an agent with a scoped token |
| [`another invite create`](#another-invite-create) | Mint an invite. Prints the code once (and, with --print-snippet, a join script) |
| [`another invite list`](#another-invite-list) | List invites and their status |
| [`another invite revoke`](#another-invite-revoke) | Stop an invite from being redeemed |
| [`another join`](#another-join) | Join a hub as a remote agent with an invite code (saves a profile, writes MCP config) |
| [`another remote`](#another-remote) | Act on the hub you joined (uses the profile `another join` saved) |
| [`another remote status`](#another-remote-status) | Show the saved profile and send a heartbeat |
| [`another remote heartbeat`](#another-remote-heartbeat) | Heartbeat (renews holds and leases); --end ends the session |
| [`another remote lanes`](#another-remote-lanes) | Lanes in your scope on the hub |
| [`another remote report`](#another-remote-report) | Post a status report to a lane (default: the invite's lane) |
| [`another remote decisions`](#another-remote-decisions) | Open decisions in your scope |
| [`another remote answer`](#another-remote-answer) | Answer a decision on the hub (as the relaying agent) |
| [`another remote leave`](#another-remote-leave) | Leave the hub (ephemeral agents: token revoked, holds released) and forget the profile |
| [`another summary`](#another-summary) | Daily summary: what needs you, lanes, decisions, requests, holds, spend, agents |
| [`another summary schedule`](#another-summary-schedule) | Opt-in daily rollup sent by `another serve`, e.g. `another summary schedule --daily 08:30 --to mail:jacob`. Without flags, shows it |
| [`another spend`](#another-spend) | Spend ledger: budgets, spend entries, hourly leases with approval, burn-down report |
| [`another spend account`](#another-spend-account) | Manage accounts (budgets) |
| [`another spend account add`](#another-spend-account-add) | Add an account, e.g. `another spend account add claude-api --kind api_credits --unit USD --budget 200` |
| [`another spend account list`](#another-spend-account-list) | List accounts |
| [`another spend account update`](#another-spend-account-update) | Change an account |
| [`another spend account archive`](#another-spend-account-archive) | Archive an account (hidden from reports; history kept) |
| [`another spend record`](#another-spend-record) | Record an amount spent |
| [`another spend borrow`](#another-spend-borrow) | Start an hourly lease (asks for approval if the account requires it) |
| [`another spend release`](#another-spend-release) | End your lease (its cost is recorded) or withdraw a pending one |
| [`another spend leases`](#another-spend-leases) | List leases (open ones unless --all) |
| [`another spend link`](#another-spend-link) | Charge borrowing a registered resource to an account, e.g. `another spend link runpod gpu_pod:a100`. Borrowers then get a spend lease (behind the account's approval gate) that accrues while their resource lease lives |
| [`another spend unlink`](#another-spend-unlink) | Stop charging a resource to its account (open spend leases run to their end) |
| [`another webhook`](#another-webhook) | Webhooks: signed HTTP POSTs for lane, request, hold and lease events |
| [`another webhook add`](#another-webhook-add) | Subscribe a URL. Prints the signing secret once |
| [`another webhook list`](#another-webhook-list) | List subscriptions with delivery counts (secrets are never shown) |
| [`another webhook remove`](#another-webhook-remove) | Remove a subscription; pending deliveries are dropped |
| [`another webhook test`](#another-webhook-test) | Send a signed test delivery now and show the result |
| [`another webhook dead`](#another-webhook-dead) | Show dead deliveries (gave up after 7 attempts), newest first |
| [`another webhook deliveries`](#another-webhook-deliveries) | Show recent deliveries of any status |
| [`another webhook retry`](#another-webhook-retry) | Re-queue a dead delivery |
| [`another activity`](#another-activity) | Show the activity feed (who is doing what), or a summary of it |
| [`another hook`](#another-hook) | Install, remove or run client hook packs |
| [`another hook install`](#another-hook-install) | Add Another hooks to client configs (idempotent; backs up files it changes) |
| [`another hook uninstall`](#another-hook-uninstall) | Remove Another hooks from client configs, leaving other hooks untouched |
| [`another hook status`](#another-hook-status) | Show which clients have Another hooks installed |
| [`another hook run`](#another-hook-run) | Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0 |
| [`another sweep`](#another-sweep) | Expire lapsed holds and mark overdue requests stale now |
| [`another events`](#another-events) | Show the most recent event-log records |

## another

```text
Local-first inbox, requests and path holds for coding agents

Usage: another [OPTIONS] <COMMAND>

Commands:
  init       Create the data directory and database if needed; show the local node
  status     Show the local node, database location and registered agents
  doctor     Check the setup: store, server, tokens, client hooks (and Codex hook trust), ports and resources. Prints PASS / WARN / FAIL with a fix for each problem; exits 1 on FAIL
  serve      Run the daemon: MCP over loopback streamable HTTP (default) or stdio
  project    Register projects or list them
  agent      Register agents, list them, or show one
  send       Send a message
  reply      Reply to a message within its thread
  inbox      Show an agent's inbox and open requests
  ack        Mark messages read and acknowledged
  threads    List threads an agent is part of
  thread     Show every message in a thread
  requests   List requests
  request    Claim, release, complete or cancel a request
  hold       Acquire, check, renew or release path holds
  holds      List live path holds
  heartbeat  Session heartbeat: renew holds; with --end release them and go offline
  resources  Find registered resources (model servers, dev servers, ports, GPUs, containers, jobs)
  resource   Register a resource or renew resource leases
  borrow     Borrow a shared resource (id or kind:name)
  release    Release your lease on a resource (id or kind:name)
  orphans    List orphan candidates: lapsed resources and unregistered listeners
  reap       Reap an orphan (resource id, kind:name, or pid:N). Without --confirm, only shows the plan
  token      Create, list or revoke bearer tokens for remote peers
  audit      Show the audit log of remote (token-authenticated) calls
  node       Show or rename this node
  pair       Pair nodes for federation: invite (hub), join (spoke), revoke
  peers      List paired nodes
  sync       Spoke: push this node's events to the hub and pull everyone else's
  wake       Nudge agents with unread mail through tmux, cmux or Herdr (optional adapters)
  lane       Lanes: named units of work with a charter, lead, status and structured reports
  config     Node settings: `another config` lists them, `another config KEY` shows one, `another config KEY VALUE` sets it (e.g. `another config decider jacob`)
  decision   Decisions: structured questions from agents to a person, answered here or by the manager bot
  invite     Invites for remote agents: single-use codes that admit an agent with a scoped token
  join       Join a hub as a remote agent with an invite code (saves a profile, writes MCP config)
  remote     Act on the hub you joined (uses the profile `another join` saved)
  summary    Daily summary: what needs you, lanes, decisions, requests, holds, spend, agents
  spend      Spend ledger: budgets, spend entries, hourly leases with approval, burn-down report
  webhook    Webhooks: signed HTTP POSTs for lane, request, hold and lease events
  activity   Show the activity feed (who is doing what), or a summary of it
  hook       Install, remove or run client hook packs
  sweep      Expire lapsed holds and mark overdue requests stale now
  events     Show the most recent event-log records
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
  -V, --version      Print version
```

## another init

```text
Create the data directory and database if needed; show the local node

Usage: another init [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another status

```text
Show the local node, database location and registered agents

Usage: another status [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another doctor

```text
Check the setup: store, server, tokens, client hooks (and Codex hook trust), ports and resources. Prints PASS / WARN / FAIL with a fix for each problem; exits 1 on FAIL

Usage: another doctor [OPTIONS]

Options:
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --server <SERVER>  The `another serve` HTTP address to check [env: ANOTHER_SERVER=] [default: http://127.0.0.1:7466]
      --json             Print machine-readable JSON
  -h, --help             Print help
```

## another serve

```text
Run the daemon: MCP over loopback streamable HTTP (default) or stdio

Usage: another serve [OPTIONS]

Options:
      --home <HOME>                   Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --stdio                         Serve MCP on stdin/stdout instead of HTTP (for MCP client configs)
      --bind <BIND>                   HTTP bind address. Loopback unless --auth and --allow-remote-bind are both given [default: 127.0.0.1:7466]
      --json                          Print machine-readable JSON
      --auth                          Require bearer tokens (`another token create`) on /mcp and /activity; audit every call
      --allow-remote-bind             Allow a non-loopback --bind (requires --auth). Prefer a tunnel instead
      --allowed-host <ALLOWED_HOSTS>  Extra Host header to accept, e.g. a tunnel hostname (repeatable; requires --auth)
      --federation                    Act as a federation hub: serve /fed/* for paired spokes (node-signed requests)
      --sweep-secs <SWEEP_SECS>       Seconds between sweeps that expire holds and stale requests [default: 30]
  -h, --help                          Print help
```

## another project

```text
Register projects or list them

Usage: another project [OPTIONS] <COMMAND>

Commands:
  ensure  Register a project by path (default: current directory)
  list    List registered projects
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another project ensure

```text
Register a project by path (default: current directory)

Usage: another project ensure [OPTIONS] [PATH]

Arguments:
  [PATH]  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another project list

```text
List registered projects

Usage: another project list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another agent

```text
Register agents, list them, or show one

Usage: another agent [OPTIONS] <COMMAND>

Commands:
  register  Register or update an agent
  list      List registered agents
  show      Show one agent: its program, status and when it was last seen
  kick      Remove an agent now: revoke all its tokens, release its holds and leases, tell its lanes
  remotes   Agents that joined through invites, with presence (online, idle, gone)
  help      Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another agent register

```text
Register or update an agent

Usage: another agent register [OPTIONS] --program <PROGRAM> <NAME>

Arguments:
  <NAME>  

Options:
      --home <HOME>                Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --program <PROGRAM>          claude | grok | codex | cursor | grok_bot | other
      --json                       Print machine-readable JSON
      --project <PROJECT>          Project path to scope the name to (enables name@project)
      --model <MODEL>              
      --description <DESCRIPTION>  
  -h, --help                       Print help
```

## another agent list

```text
List registered agents

Usage: another agent list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another agent show

```text
Show one agent: its program, status and when it was last seen

Usage: another agent show [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another agent kick

```text
Remove an agent now: revoke all its tokens, release its holds and leases, tell its lanes

Usage: another agent kick [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --reason <REASON>  
      --json             Print machine-readable JSON
  -h, --help             Print help
```

## another agent remotes

```text
Agents that joined through invites, with presence (online, idle, gone)

Usage: another agent remotes [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another send

```text
Send a message

Usage: another send [OPTIONS] --from <FROM> --to <TO> --subject <SUBJECT>

Options:
  -f, --from <FROM>              Sender: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>              Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                     Print machine-readable JSON
  -t, --to <TO>                  Recipients (repeat or comma-separate)
      --cc <CC>                  
  -s, --subject <SUBJECT>        
  -b, --body <BODY>              Message body; `-` reads stdin [default: ""]
  -i, --importance <IMPORTANCE>  [default: normal]
      --ack                      Ask recipients to acknowledge
      --request                  Also open a claimable request
      --due <DUE>                Request due time in seconds (implies --request)
      --thread <THREAD>          Post into an existing thread
  -h, --help                     Print help
```

## another reply

```text
Reply to a message within its thread

Usage: another reply [OPTIONS] --from <FROM> --body <BODY> <MESSAGE_ID>

Arguments:
  <MESSAGE_ID>  

Options:
  -f, --from <FROM>              [env: ANOTHER_AGENT=]
      --home <HOME>              Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
  -b, --body <BODY>              Reply body; `-` reads stdin
      --json                     Print machine-readable JSON
      --all                      Reply to all original recipients
  -i, --importance <IMPORTANCE>  
      --ack                      
  -h, --help                     Print help
```

## another inbox

```text
Show an agent's inbox and open requests

Usage: another inbox [OPTIONS] --agent <AGENT>

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --unread         Only unread messages
      --urgent         Only high/urgent messages
      --limit <LIMIT>  [default: 20]
  -h, --help           Print help
```

## another ack

```text
Mark messages read and acknowledged

Usage: another ack [OPTIONS] --agent <AGENT> <MESSAGE_IDS>...

Arguments:
  <MESSAGE_IDS>...  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --read-only      Only mark read; do not acknowledge
  -h, --help           Print help
```

## another threads

```text
List threads an agent is part of

Usage: another threads [OPTIONS] --agent <AGENT>

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --limit <LIMIT>  [default: 20]
  -h, --help           Print help
```

## another thread

```text
Show every message in a thread

Usage: another thread [OPTIONS] <THREAD_ID>

Arguments:
  <THREAD_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another requests

```text
List requests

Usage: another requests [OPTIONS]

Options:
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --status <STATUS>  open | claimed | done | stale | cancelled
      --json             Print machine-readable JSON
      --owner <OWNER>    Owner name or name@project
      --limit <LIMIT>    [default: 50]
  -h, --help             Print help
```

## another request

```text
Claim, release, complete or cancel a request

Usage: another request [OPTIONS] --agent <AGENT> <ACTION> <REQUEST_ID>

Arguments:
  <ACTION>      claim | release | complete | cancel
  <REQUEST_ID>  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --note <NOTE>    
      --due <DUE>      On claim: due in this many seconds
  -h, --help           Print help
```

## another hold

```text
Acquire, check, renew or release path holds

Usage: another hold [OPTIONS] <COMMAND>

Commands:
  acquire  Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted)
  check    Report conflicts for paths without acquiring (exit code 3 on conflict)
  renew    Renew your holds (all, or the given ids)
  release  Release your holds (all, or the given ids)
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another hold acquire

```text
Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted)

Usage: another hold acquire [OPTIONS] --agent <AGENT> <PATHS>...

Arguments:
  <PATHS>...  

Options:
  -a, --agent <AGENT>      Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json               Print machine-readable JSON
  -p, --project <PROJECT>  Project path or slug (default: current directory)
      --ttl <TTL>          Lease seconds (default 1800)
      --shared             Shared instead of exclusive
      --reason <REASON>    
      --lane <LANE>        Lane the holds are for (default: $ANOTHER_LANE, else the one lane you lead here) [env: ANOTHER_LANE=]
  -h, --help               Print help
```

## another hold check

```text
Report conflicts for paths without acquiring (exit code 3 on conflict)

Usage: another hold check [OPTIONS] --agent <AGENT> <PATHS>...

Arguments:
  <PATHS>...  

Options:
  -a, --agent <AGENT>      Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json               Print machine-readable JSON
  -p, --project <PROJECT>  Project path or slug (default: current directory)
      --shared             
  -h, --help               Print help
```

## another hold renew

```text
Renew your holds (all, or the given ids)

Usage: another hold renew [OPTIONS] --agent <AGENT> [HOLD_IDS]...

Arguments:
  [HOLD_IDS]...  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another hold release

```text
Release your holds (all, or the given ids)

Usage: another hold release [OPTIONS] --agent <AGENT> [HOLD_IDS]...

Arguments:
  [HOLD_IDS]...  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another holds

```text
List live path holds

Usage: another holds [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
  -p, --project <PROJECT>  Project path or slug
      --agent <AGENT>      
      --json               Print machine-readable JSON
      --by-lane            Group by lane
  -h, --help               Print help
```

## another heartbeat

```text
Session heartbeat: renew holds; with --end release them and go offline

Usage: another heartbeat [OPTIONS] --agent <AGENT>

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --start          Session is starting: record a `session.started` event
      --end            
      --keep           With --end: keep resource leases (servers meant to outlive the session)
  -h, --help           Print help
```

## another resources

```text
Find registered resources (model servers, dev servers, ports, GPUs, containers, jobs)

Usage: another resources [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --kind <KIND>        model_server | dev_server | port | gpu | container | long_job
      --json               Print machine-readable JSON
  -q, --query <QUERY>      Substring of name, endpoint or purpose
  -p, --project <PROJECT>  Project path or slug
      --all                Include released resources
      --local              Only resources owned by this node (default includes other nodes')
  -h, --help               Print help
```

## another resource

```text
Register a resource or renew resource leases

Usage: another resource [OPTIONS] <COMMAND>

Commands:
  register   Register a running resource and take the owner lease
  heartbeat  Renew your resource leases (all, or the given ids / kind:name)
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another resource register

```text
Register a running resource and take the owner lease

Usage: another resource register [OPTIONS] --agent <AGENT> <KIND> <NAME>

Arguments:
  <KIND>  model_server | dev_server | port | gpu | container | long_job
  <NAME>  Identity, e.g. ollama@11434, vite@5173, tcp:127.0.0.1:5432, metal:0

Options:
  -a, --agent <AGENT>        Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>          Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --endpoint <ENDPOINT>  
      --json                 Print machine-readable JSON
      --port <PORT>          
      --pid <PID>            
  -p, --project <PROJECT>    Project path or slug
      --purpose <PURPOSE>    
      --exclusive            Refuse borrowers
      --ttl <TTL>            Lease seconds (default 1800, max 86400)
      --meta <KEY=VALUE>     Metadata as key=value (repeatable), e.g. --meta model=llama3 --meta backend=metal
  -h, --help                 Print help
```

## another resource heartbeat

```text
Renew your resource leases (all, or the given ids / kind:name)

Usage: another resource heartbeat [OPTIONS] --agent <AGENT> [TARGETS]...

Arguments:
  [TARGETS]...  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another borrow

```text
Borrow a shared resource (id or kind:name)

Usage: another borrow [OPTIONS] --agent <AGENT> <TARGET>

Arguments:
  <TARGET>  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --ttl <TTL>      Lease seconds (default 1800)
  -h, --help           Print help
```

## another release

```text
Release your lease on a resource (id or kind:name)

Usage: another release [OPTIONS] --agent <AGENT> <TARGET>

Arguments:
  <TARGET>  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --stopped        Owner: the resource was shut down; end all leases and mark it released
  -h, --help           Print help
```

## another orphans

```text
List orphan candidates: lapsed resources and unregistered listeners

Usage: another orphans [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --no-scan      Skip the listening-process scan (lapsed registered resources only)
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another reap

```text
Reap an orphan (resource id, kind:name, or pid:N). Without --confirm, only shows the plan

Usage: another reap [OPTIONS] <TARGET>

Arguments:
  <TARGET>  

Options:
      --grace <GRACE>  Seconds the target must have been orphaned (or alive, for pid targets) [default: 600]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --confirm        Human confirmation: actually send SIGTERM. Agents never pass this
      --json           Print machine-readable JSON
      --force          With --confirm: SIGKILL if the process survives SIGTERM
  -h, --help           Print help
```

## another token

```text
Create, list or revoke bearer tokens for remote peers

Usage: another token [OPTIONS] <COMMAND>

Commands:
  create  Create a token that acts as one agent within project globs. Printed once
  list    List tokens: id, agent, project globs, status and last use (never the secret)
  revoke  Revoke a token by id (takes effect on the next request)
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another token create

```text
Create a token that acts as one agent within project globs. Printed once

Usage: another token create [OPTIONS] --agent <AGENT>

Options:
  -a, --agent <AGENT>       The agent the token acts as (register it first, e.g. with --program grok_bot)
      --home <HOME>         Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                Print machine-readable JSON
  -p, --project <PROJECTS>  Project path or slug glob the token may touch (repeatable), e.g. '/work/*' or shop
      --label <LABEL>       A note to recognise the token by
      --expires <EXPIRES>   Expiry: seconds or a duration like 12h, 30d
  -h, --help                Print help
```

## another token list

```text
List tokens: id, agent, project globs, status and last use (never the secret)

Usage: another token list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another token revoke

```text
Revoke a token by id (takes effect on the next request)

Usage: another token revoke [OPTIONS] <TOKEN_ID>

Arguments:
  <TOKEN_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another audit

```text
Show the audit log of remote (token-authenticated) calls

Usage: another audit [OPTIONS]

Options:
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --limit <LIMIT>  [default: 50]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another node

```text
Show or rename this node

Usage: another node [OPTIONS] <COMMAND>

Commands:
  show  Show this node's id, name and key fingerprint
  name  Rename this node (do this before pairing; names must be unique across nodes). Without a name, prints the current one
  help  Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another node show

```text
Show this node's id, name and key fingerprint

Usage: another node show [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another node name

```text
Rename this node (do this before pairing; names must be unique across nodes). Without a name, prints the current one

Usage: another node name [OPTIONS] [NAME]

Arguments:
  [NAME]  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another pair

```text
Pair nodes for federation: invite (hub), join (spoke), revoke

Usage: another pair [OPTIONS] <COMMAND>

Commands:
  invite  Hub: print a single-use invite code for a spoke
  join    Spoke: redeem an invite code and record the hub
  revoke  Stop accepting (or using) a paired node, by name or id
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another pair invite

```text
Hub: print a single-use invite code for a spoke

Usage: another pair invite [OPTIONS] --url <URL>

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --url <URL>          URL spokes use to reach this hub (e.g. its tunnel URL)
      --json               Print machine-readable JSON
      --perms <PERMS>      Permissions for the spoke: read_feed, send_mail, borrow_resource (comma-separated) [default: read_feed,send_mail,borrow_resource]
      --expires <EXPIRES>  Invite lifetime: seconds or a duration like 30m, 24h [default: 1h]
  -h, --help               Print help
```

## another pair join

```text
Spoke: redeem an invite code and record the hub

Usage: another pair join [OPTIONS] <CODE>

Arguments:
  <CODE>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another pair revoke

```text
Stop accepting (or using) a paired node, by name or id

Usage: another pair revoke [OPTIONS] <NODE>

Arguments:
  <NODE>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another peers

```text
List paired nodes

Usage: another peers [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another sync

```text
Spoke: push this node's events to the hub and pull everyone else's

Usage: another sync [OPTIONS]

Options:
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --watch          Keep syncing every --every seconds
      --every <EVERY>  [default: 30]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another wake

```text
Nudge agents with unread mail through tmux, cmux or Herdr (optional adapters)

Usage: another wake [OPTIONS]
       another wake <COMMAND>

Commands:
  set    Store where to wake an agent (hooks capture tmux/cmux targets automatically)
  clear  Forget an agent's wake target
  list   List stored wake targets
  help   Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json               Print machine-readable JSON
  -a, --agent <AGENT>      Agent to wake (default: $ANOTHER_AGENT); or use --all [env: ANOTHER_AGENT=]
      --all                Wake every agent with a stored target that has unread mail or open requests
      --via <VIA>          Adapter: tmux | cmux | herdr (default: the agent's stored target)
      --target <TARGET>    tmux pane, cmux surface id (or `notify`), or Herdr agent name
      --message <MESSAGE>  Text to send (default: a one-line inbox summary)
      --force              Wake even when nothing is unread
      --dry-run            Print the adapter commands without running them
  -h, --help               Print help
```

## another wake set

```text
Store where to wake an agent (hooks capture tmux/cmux targets automatically)

Usage: another wake set [OPTIONS] --agent <AGENT> --via <VIA> --target <TARGET>

Options:
  -a, --agent <AGENT>    Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json             Print machine-readable JSON
      --via <VIA>        tmux | cmux | herdr
      --target <TARGET>  
  -h, --help             Print help
```

## another wake clear

```text
Forget an agent's wake target

Usage: another wake clear [OPTIONS] --agent <AGENT>

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another wake list

```text
List stored wake targets

Usage: another wake list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another lane

```text
Lanes: named units of work with a charter, lead, status and structured reports

Usage: another lane [OPTIONS] <COMMAND>

Commands:
  create  Create a lane (status starts at planned)
  list    List lanes (open ones unless --all or --status)
  show    Show a lane with links and recent reports
  update  Change status, charter or lead, or link threads / requests / holds / messages
  report  Post a status report (also mailed to the lane's counterpart)
  done    Post the DONE report: outcome and optional PR link; closes the lane
  close   Close a lane without a report (done, or abandoned with --abandon)
  export  Write a lane as markdown: to stdout, --out FILE, or --dir DIR (<name>-STATUS.md, plus DONE-<name>.md when done)
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another lane create

```text
Create a lane (status starts at planned)

Usage: another lane create [OPTIONS] --agent <AGENT> --lead <LEAD> <NAME>

Arguments:
  <NAME>  

Options:
  -a, --agent <AGENT>      Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json               Print machine-readable JSON
  -p, --project <PROJECT>  Project path or slug
      --lead <LEAD>        Agent that leads the work
      --charter <CHARTER>  The brief; `-` reads stdin [default: ""]
      --status <STATUS>    
  -h, --help               Print help
```

## another lane list

```text
List lanes (open ones unless --all or --status)

Usage: another lane list [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --status <STATUS>    
      --json               Print machine-readable JSON
  -p, --project <PROJECT>  
      --lead <LEAD>        
      --all                
  -h, --help               Print help
```

## another lane show

```text
Show a lane with links and recent reports

Usage: another lane show [OPTIONS] <LANE>

Arguments:
  <LANE>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --markdown     Print as markdown (like the old STATUS files)
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another lane update

```text
Change status, charter or lead, or link threads / requests / holds / messages

Usage: another lane update [OPTIONS] --agent <AGENT> <LANE>

Arguments:
  <LANE>  

Options:
  -a, --agent <AGENT>                 Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>                   Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                          Print machine-readable JSON
      --status <STATUS>               
      --charter <CHARTER>             
      --lead <LEAD>                   
      --link-thread <LINK_THREADS>    
      --link-request <LINK_REQUESTS>  
      --link-hold <LINK_HOLDS>        
      --link-message <LINK_MESSAGES>  
  -h, --help                          Print help
```

## another lane report

```text
Post a status report (also mailed to the lane's counterpart)

Usage: another lane report [OPTIONS] --agent <AGENT> --summary <SUMMARY> <LANE>

Arguments:
  <LANE>  

Options:
  -a, --agent <AGENT>         Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>           Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                  Print machine-readable JSON
  -s, --summary <SUMMARY>     `-` reads stdin
      --progress <PROGRESS>   
      --status <STATUS>       
      --link <LINKS>          
      --artifact <ARTIFACTS>  
  -h, --help                  Print help
```

## another lane done

```text
Post the DONE report: outcome and optional PR link; closes the lane

Usage: another lane done [OPTIONS] --agent <AGENT> --outcome <OUTCOME> <LANE>

Arguments:
  <LANE>  

Options:
  -a, --agent <AGENT>         Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>           Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                  Print machine-readable JSON
      --outcome <OUTCOME>     
      --pr <PR>               
  -s, --summary <SUMMARY>     
      --artifact <ARTIFACTS>  
  -h, --help                  Print help
```

## another lane close

```text
Close a lane without a report (done, or abandoned with --abandon)

Usage: another lane close [OPTIONS] --agent <AGENT> <LANE>

Arguments:
  <LANE>  

Options:
  -a, --agent <AGENT>      Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --abandon            
      --json               Print machine-readable JSON
      --outcome <OUTCOME>  
  -h, --help               Print help
```

## another lane export

```text
Write a lane as markdown: to stdout, --out FILE, or --dir DIR (<name>-STATUS.md, plus DONE-<name>.md when done)

Usage: another lane export [OPTIONS] <LANE>

Arguments:
  <LANE>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --out <OUT>    
      --dir <DIR>    
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another config

```text
Node settings: `another config` lists them, `another config KEY` shows one, `another config KEY VALUE` sets it (e.g. `another config decider jacob`)

Usage: another config [OPTIONS] [KEY] [VALUE]

Arguments:
  [KEY]    
  [VALUE]  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another decision

```text
Decisions: structured questions from agents to a person, answered here or by the manager bot

Usage: another decision [OPTIONS] <COMMAND>

Commands:
  ask     Ask a person (default: `another config decider`, else jacob) a question with 2-6 options
  list    List decisions (open ones unless --status or --all)
  show    Show a decision; --markdown prints the short phone-readable form
  answer  Answer a decision: an option value (or its number), and/or --text if allowed
  cancel  Withdraw a decision you asked
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another decision ask

```text
Ask a person (default: `another config decider`, else jacob) a question with 2-6 options

Usage: another decision ask [OPTIONS] --agent <AGENT> --question <QUESTION> --option <OPTIONS>

Options:
  -a, --agent <AGENT>            Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>              Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                     Print machine-readable JSON
  -q, --question <QUESTION>      
  -o, --option <OPTIONS>         `value=Label` (repeat 2-6 times), e.g. `--option ship="Ship behind a flag"`
      --free-text                Accept a free-text answer too
      --link <LINKS>             Context link (repeatable)
      --deadline <DEADLINE>      Duration (2h) or RFC 3339 time
      --on-timeout <ON_TIMEOUT>  choose_default, cancel or keep_open (default)
      --default <DEFAULT>        Option value applied by choose_default
      --to <TO>                  Who to ask
  -p, --project <PROJECT>        
      --lane <LANE>              
  -h, --help                     Print help
```

## another decision list

```text
List decisions (open ones unless --status or --all)

Usage: another decision list [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --status <STATUS>    
      --all                
      --json               Print machine-readable JSON
  -p, --project <PROJECT>  
      --lane <LANE>        
      --for <HUMAN>        Only decisions for this person
  -h, --help               Print help
```

## another decision show

```text
Show a decision; --markdown prints the short phone-readable form

Usage: another decision show [OPTIONS] <DECISION>

Arguments:
  <DECISION>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --markdown     
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another decision answer

```text
Answer a decision: an option value (or its number), and/or --text if allowed

Usage: another decision answer [OPTIONS] <DECISION> [VALUE]

Arguments:
  <DECISION>  
  [VALUE]     

Options:
      --home <HOME>              Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --text <TEXT>              
      --as <ANSWERED_BY>         Who is answering (default: the person asked)
      --json                     Print machine-readable JSON
      --relayed-by <RELAYED_BY>  The manager or person on this node relaying the answer (needed for another node's decision, unless --as names a person registered here)
  -h, --help                     Print help
```

## another decision cancel

```text
Withdraw a decision you asked

Usage: another decision cancel [OPTIONS] --agent <AGENT> <DECISION>

Arguments:
  <DECISION>  

Options:
  -a, --agent <AGENT>    Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json             Print machine-readable JSON
      --reason <REASON>  
  -h, --help             Print help
```

## another invite

```text
Invites for remote agents: single-use codes that admit an agent with a scoped token

Usage: another invite [OPTIONS] <COMMAND>

Commands:
  create  Mint an invite. Prints the code once (and, with --print-snippet, a join script)
  list    List invites and their status
  revoke  Stop an invite from being redeemed
  help    Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another invite create

```text
Mint an invite. Prints the code once (and, with --print-snippet, a join script)

Usage: another invite create [OPTIONS] --agent <AGENT> --project <PROJECTS>

Options:
      --agent <AGENT>            Name the joining agent gets
      --home <HOME>              Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                     Print machine-readable JSON
      --program <PROGRAM>        claude | codex | grok | grok_bot | cursor | other [default: other]
  -p, --project <PROJECTS>       Project path or slug glob the agent may touch (repeatable, required)
      --expires <EXPIRES>        How long the code is valid (default 24h, at most 30d) [default: 24h]
      --uses <USES>              How many times it can be redeemed (each use gets its own token) [default: 1]
      --lane <LANE>              Lane the agent joins (id, name or name@project)
      --idle-hours <IDLE_HOURS>  Ephemeral agents leave after this many hours unseen (default: the node's `remote_idle_hours` setting, 4 unless changed with `another config`)
      --persistent               Keep the agent after its session ends or it goes idle
      --token-ttl <TOKEN_TTL>    Expiry of issued tokens (default: none)
      --print-snippet            Also print a shell snippet for a cloud session (needs --hub-url)
      --hub-url <HUB_URL>        The hub URL the remote agent will use
  -h, --help                     Print help
```

## another invite list

```text
List invites and their status

Usage: another invite list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another invite revoke

```text
Stop an invite from being redeemed

Usage: another invite revoke [OPTIONS] <INVITE_ID>

Arguments:
  <INVITE_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another join

```text
Join a hub as a remote agent with an invite code (saves a profile, writes MCP config)

Usage: another join [OPTIONS] <HUB> <CODE>

Arguments:
  <HUB>
          The hub's URL, e.g. https://my-laptop.my-tailnet.ts.net

  <CODE>
          The invite code (anj_…)

Options:
      --home <HOME>
          Data directory (default: $ANOTHER_HOME, else ~/.another)
          
          [env: ANOTHER_HOME]

      --write-mcp <WRITE_MCP>
          Write MCP client config: auto, claude, codex, both or none

          Possible values:
          - auto:   Claude Code if `claude` is installed, Codex if `~/.codex` exists
          - claude
          - codex
          - both
          - none
          
          [default: none]

      --hello
          Send a session-start heartbeat and a "joined" report to the invite's lane

      --json
          Print machine-readable JSON

  -h, --help
          Print help (see a summary with '-h')
```

## another remote

```text
Act on the hub you joined (uses the profile `another join` saved)

Usage: another remote [OPTIONS] <COMMAND>

Commands:
  status     Show the saved profile and send a heartbeat
  heartbeat  Heartbeat (renews holds and leases); --end ends the session
  lanes      Lanes in your scope on the hub
  report     Post a status report to a lane (default: the invite's lane)
  decisions  Open decisions in your scope
  answer     Answer a decision on the hub (as the relaying agent)
  leave      Leave the hub (ephemeral agents: token revoked, holds released) and forget the profile
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another remote status

```text
Show the saved profile and send a heartbeat

Usage: another remote status [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another remote heartbeat

```text
Heartbeat (renews holds and leases); --end ends the session

Usage: another remote heartbeat [OPTIONS]

Options:
      --end          
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another remote lanes

```text
Lanes in your scope on the hub

Usage: another remote lanes [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another remote report

```text
Post a status report to a lane (default: the invite's lane)

Usage: another remote report [OPTIONS] --summary <SUMMARY>

Options:
      --home <HOME>          Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --lane <LANE>          
      --json                 Print machine-readable JSON
      --summary <SUMMARY>    
      --status <STATUS>      
      --progress <PROGRESS>  
  -h, --help                 Print help
```

## another remote decisions

```text
Open decisions in your scope

Usage: another remote decisions [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another remote answer

```text
Answer a decision on the hub (as the relaying agent)

Usage: another remote answer [OPTIONS] <DECISION> [VALUE]

Arguments:
  <DECISION>  
  [VALUE]     

Options:
      --home <HOME>       Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --text <TEXT>       
      --as <ANSWERED_BY>  The person who decided
      --json              Print machine-readable JSON
  -h, --help              Print help
```

## another remote leave

```text
Leave the hub (ephemeral agents: token revoked, holds released) and forget the profile

Usage: another remote leave [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another summary

```text
Daily summary: what needs you, lanes, decisions, requests, holds, spend, agents

Usage: another summary [OPTIONS]
       another summary <COMMAND>

Commands:
  schedule  Opt-in daily rollup sent by `another serve`, e.g. `another summary schedule --daily 08:30 --to mail:jacob`. Without flags, shows it
  help      Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --since <SINCE>  Duration (24h), RFC 3339 time, or event id [default: 24h]
  -h, --help           Print help
```

## another summary schedule

```text
Opt-in daily rollup sent by `another serve`, e.g. `another summary schedule --daily 08:30 --to mail:jacob`. Without flags, shows it

Usage: another summary schedule [OPTIONS]

Options:
      --daily <DAILY>  Local time, HH:MM (24-hour)
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
      --to <TO>        `webhook` (local webhooks with the daily_summary filter) or `mail:NAME`
      --off            Turn the rollup off
  -h, --help           Print help
```

## another spend

```text
Spend ledger: budgets, spend entries, hourly leases with approval, burn-down report

Usage: another spend [OPTIONS]
       another spend <COMMAND>

Commands:
  account  Manage accounts (budgets)
  record   Record an amount spent
  borrow   Start an hourly lease (asks for approval if the account requires it)
  release  End your lease (its cost is recorded) or withdraw a pending one
  leases   List leases (open ones unless --all)
  link     Charge borrowing a registered resource to an account, e.g. `another spend link runpod gpu_pod:a100`. Borrowers then get a spend lease (behind the account's approval gate) that accrues while their resource lease lives
  unlink   Stop charging a resource to its account (open spend leases run to their end)
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>           Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json                  Print machine-readable JSON
      --period <PERIOD>       current (default), previous, or YYYY-MM [default: current]
      --account <ACCOUNT>     
      --lane <LANE>           
      --agent <AGENT_FILTER>  Only this agent's spend
  -h, --help                  Print help
```

## another spend account

```text
Manage accounts (budgets)

Usage: another spend account [OPTIONS] <COMMAND>

Commands:
  add      Add an account, e.g. `another spend account add claude-api --kind api_credits --unit USD --budget 200`
  list     List accounts
  update   Change an account
  archive  Archive an account (hidden from reports; history kept)
  help     Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another spend account add

```text
Add an account, e.g. `another spend account add claude-api --kind api_credits --unit USD --budget 200`

Usage: another spend account add [OPTIONS] --kind <KIND> --unit <UNIT> <NAME>

Arguments:
  <NAME>  

Options:
      --home <HOME>                    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --kind <KIND>                    gpu_pod, api_credits, plan_reset, server, domain
      --json                           Print machine-readable JSON
      --unit <UNIT>                    USD, credits, resets, hours, ...
      --budget <BUDGET>                Budget per period, in the unit
      --period <PERIOD>                monthly (default) or none [default: monthly]
      --reset-day <RESET_DAY>          Day of the month the period resets (1-28) [default: 1]
      --cost-per-hour <COST_PER_HOUR>  
      --requires-approval              Borrowing asks a person first (a decision)
      --approver <APPROVER>            Who approves (default: the node's decider, `another config decider NAME`)
  -p, --project <PROJECT>              Owning project; omit for a shared account
      --description <DESCRIPTION>      
  -h, --help                           Print help
```

## another spend account list

```text
List accounts

Usage: another spend account list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another spend account update

```text
Change an account

Usage: another spend account update [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --budget <BUDGET>
          
      --home <HOME>
          Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --clear-budget
          
      --json
          Print machine-readable JSON
      --reset-day <RESET_DAY>
          
      --cost-per-hour <COST_PER_HOUR>
          
      --requires-approval <REQUIRES_APPROVAL>
          [possible values: true, false]
      --approver <APPROVER>
          
      --description <DESCRIPTION>
          
  -h, --help
          Print help
```

## another spend account archive

```text
Archive an account (hidden from reports; history kept)

Usage: another spend account archive [OPTIONS] <NAME>

Arguments:
  <NAME>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another spend record

```text
Record an amount spent

Usage: another spend record [OPTIONS] --agent <AGENT> --account <ACCOUNT> --amount <AMOUNT>

Options:
  -a, --agent <AGENT>      Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --account <ACCOUNT>  
      --json               Print machine-readable JSON
      --amount <AMOUNT>    In the account's unit
      --lane <LANE>        
  -p, --project <PROJECT>  
      --note <NOTE>        
  -h, --help               Print help
```

## another spend borrow

```text
Start an hourly lease (asks for approval if the account requires it)

Usage: another spend borrow [OPTIONS] --agent <AGENT> --account <ACCOUNT>

Options:
  -a, --agent <AGENT>                  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>                    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --account <ACCOUNT>              
      --json                           Print machine-readable JSON
      --lane <LANE>                    
      --cost-per-hour <COST_PER_HOUR>  
      --ttl <TTL>                      Lease seconds, renewed by heartbeat (default 3600)
      --hours <HOURS>                  Expected hours (shown in the approval question)
      --note <NOTE>                    
  -h, --help                           Print help
```

## another spend release

```text
End your lease (its cost is recorded) or withdraw a pending one

Usage: another spend release [OPTIONS] --agent <AGENT> <LEASE_ID>

Arguments:
  <LEASE_ID>  

Options:
  -a, --agent <AGENT>  Acting agent: name or name@project (default: $ANOTHER_AGENT) [env: ANOTHER_AGENT=]
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json           Print machine-readable JSON
  -h, --help           Print help
```

## another spend leases

```text
List leases (open ones unless --all)

Usage: another spend leases [OPTIONS]

Options:
      --all          
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another spend link

```text
Charge borrowing a registered resource to an account, e.g. `another spend link runpod gpu_pod:a100`. Borrowers then get a spend lease (behind the account's approval gate) that accrues while their resource lease lives

Usage: another spend link [OPTIONS] <ACCOUNT> <RESOURCE>

Arguments:
  <ACCOUNT>   
  <RESOURCE>  Resource id or kind:name

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another spend unlink

```text
Stop charging a resource to its account (open spend leases run to their end)

Usage: another spend unlink [OPTIONS] <RESOURCE>

Arguments:
  <RESOURCE>  Resource id or kind:name

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another webhook

```text
Webhooks: signed HTTP POSTs for lane, request, hold and lease events

Usage: another webhook [OPTIONS] <COMMAND>

Commands:
  add         Subscribe a URL. Prints the signing secret once
  list        List subscriptions with delivery counts (secrets are never shown)
  remove      Remove a subscription; pending deliveries are dropped
  test        Send a signed test delivery now and show the result
  dead        Show dead deliveries (gave up after 7 attempts), newest first
  deliveries  Show recent deliveries of any status
  retry       Re-queue a dead delivery
  help        Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another webhook add

```text
Subscribe a URL. Prints the signing secret once

Usage: another webhook add [OPTIONS] --url <URL> --events <EVENTS>

Options:
      --home <HOME>                Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --url <URL>                  `https://` URL (plain http only to 127.0.0.1, localhost or [::1])
      --events <EVENTS>            Comma-separated filters: lane_status, lane_report, needs_decision, request_created, request_answered, hold_conflict, lease_expiring, budget, daily_summary (with --all-projects only), all
      --json                       Print machine-readable JSON
  -p, --project <PROJECTS>         Project path or slug glob (repeatable). Required unless --all-projects
      --all-projects               Every project on this node
      --description <DESCRIPTION>  
  -h, --help                       Print help
```

## another webhook list

```text
List subscriptions with delivery counts (secrets are never shown)

Usage: another webhook list [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another webhook remove

```text
Remove a subscription; pending deliveries are dropped

Usage: another webhook remove [OPTIONS] <WEBHOOK_ID>

Arguments:
  <WEBHOOK_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another webhook test

```text
Send a signed test delivery now and show the result

Usage: another webhook test [OPTIONS] <WEBHOOK_ID>

Arguments:
  <WEBHOOK_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another webhook dead

```text
Show dead deliveries (gave up after 7 attempts), newest first

Usage: another webhook dead [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --webhook <WEBHOOK>  
      --json               Print machine-readable JSON
      --limit <LIMIT>      [default: 50]
  -h, --help               Print help
```

## another webhook deliveries

```text
Show recent deliveries of any status

Usage: another webhook deliveries [OPTIONS]

Options:
      --home <HOME>        Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --webhook <WEBHOOK>  
      --json               Print machine-readable JSON
      --limit <LIMIT>      [default: 50]
  -h, --help               Print help
```

## another webhook retry

```text
Re-queue a dead delivery

Usage: another webhook retry [OPTIONS] <DELIVERY_ID>

Arguments:
  <DELIVERY_ID>  

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another activity

```text
Show the activity feed (who is doing what), or a summary of it

Usage: another activity [OPTIONS]

Options:
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --since <SINCE>  Start point: a duration (30m, 2h, 1d), an RFC 3339 time, or an event id cursor
      --json           Print machine-readable JSON
      --limit <LIMIT>  [default: 50]
      --kinds <KINDS>  Only these categories: mail, request, hold, resource, session, system, federation
      --summary        Print a summary (counts, active agents, highlights) instead of items
  -h, --help           Print help
```

## another hook

```text
Install, remove or run client hook packs

Usage: another hook [OPTIONS] <COMMAND>

Commands:
  install    Add Another hooks to client configs (idempotent; backs up files it changes)
  uninstall  Remove Another hooks from client configs, leaving other hooks untouched
  status     Show which clients have Another hooks installed
  run        Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0
  help       Print this message or the help of the given subcommand(s)

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another hook install

```text
Add Another hooks to client configs (idempotent; backs up files it changes)

Usage: another hook install [OPTIONS] --client <CLIENT>

Options:
      --client <CLIENT>  Clients to configure (repeat or comma-separate) [possible values: claude, codex, grok, cursor, all]
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --dry-run          Show what would change without writing anything
      --json             Print machine-readable JSON
      --lane-reports     Also install a Stop hook that posts an automatic report into the agent's lane (debounced to one every 15 minutes). Installing without it removes that hook
      --pre-edit         Also install a pre-edit hook (Claude Code, Codex) that holds files before they are edited and denies the edit if another lane or agent holds them. Installing without it removes that hook
  -h, --help             Print help
```

## another hook uninstall

```text
Remove Another hooks from client configs, leaving other hooks untouched

Usage: another hook uninstall [OPTIONS] --client <CLIENT>

Options:
      --client <CLIENT>  [possible values: claude, codex, grok, cursor, all]
      --home <HOME>      Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --dry-run          
      --json             Print machine-readable JSON
  -h, --help             Print help
```

## another hook status

```text
Show which clients have Another hooks installed

Usage: another hook status [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another hook run

```text
Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0

Usage: another hook run [OPTIONS] --client <CLIENT> <EVENT>

Arguments:
  <EVENT>
          Possible values:
          - session-start
          - prompt
          - session-end
          - stop:          End of an agent turn: posts an automatic lane report (opt-in)
          - pre-edit:      Before a file edit: takes an exclusive hold on the files, denying the edit on a conflict (opt-in; Claude Code and Codex)

Options:
      --client <CLIENT>
          [possible values: claude, codex, grok, cursor]

      --home <HOME>
          Data directory (default: $ANOTHER_HOME, else ~/.another)
          
          [env: ANOTHER_HOME]

      --json
          Print machine-readable JSON

  -h, --help
          Print help (see a summary with '-h')
```

## another sweep

```text
Expire lapsed holds and mark overdue requests stale now

Usage: another sweep [OPTIONS]

Options:
      --home <HOME>  Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --json         Print machine-readable JSON
  -h, --help         Print help
```

## another events

```text
Show the most recent event-log records

Usage: another events [OPTIONS]

Options:
      --home <HOME>    Data directory (default: $ANOTHER_HOME, else ~/.another) [env: ANOTHER_HOME]
      --limit <LIMIT>  [default: 20]
      --json           Print machine-readable JSON
  -h, --help           Print help
```
