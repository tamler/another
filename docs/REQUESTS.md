# Requests: received is not done

A message can be acknowledged. A request has an owner and a status. Those are three different facts:

| Fact | Recorded by | What it proves |
|---|---|---|
| The agent saw the mail | `another ack <message-id>` | Receipt. Nothing about the work |
| One agent owns the work | `another request claim <id>` | Who to ask. Not that it is finished |
| The owner says it finished | `another request complete <id>` | A claim of completion. Review the result yourself |

The walkthrough below is a recorded run, not a sketch. It was captured on 12 Nov 2026 from `another 0.1.0` built from this tree, against a temporary project named `shop` in a temporary `ANOTHER_HOME`, with no hooks installed and no `another serve` running. Ids and timestamps are as recorded; yours will differ. Where a command needs an id from an earlier step, copy it from that step's output.

The site has the same example at [toanother.one/guides/hand-off-tasks/](https://toanother.one/guides/hand-off-tasks/#received).

## Setup

Three agents in one project: `lead` opens work, `worker` takes it, `reviewer` is around later.

```text
$ another init
$ another agent register lead --program claude --project .
registered lead@shop (claude)
$ another agent register worker --program codex --project .
registered worker@shop (codex)
$ another agent register reviewer --program claude --project .
registered reviewer@shop (claude)
```

## 1. The sender opens work

`--ack` asks the recipient to acknowledge the mail. `--due 900` gives the request a deadline 900 seconds out, and implies `--request`.

```text
$ another send --from lead --to worker --subject "Fix the flaky test" --body "See tests/net.rs" --ack --due 900
sent 01M77HNNED9HZY6CKDHEZTPNSS to worker@shop
opened request 01M77HNNEDVZ92ZVSKQEYXYRB5
```

Two ids: the message, and the request opened alongside it.

## 2. The worker acks the mail

Ack means received. The request is still `open` with no owner.

```text
$ another inbox --agent worker
worker@shop: 1 unread (0 urgent), 1 awaiting ack, 1 request(s)
   new  2026-11-12T09:33:24.813Z  lead@shop        Fix the flaky test  [01M77HNNED9HZY6CKDHEZTPNSS]

requests:
open      -                Fix the flaky test  (from lead@shop due 2026-11-12T09:48:24.813Z)  [01M77HNNEDVZ92ZVSKQEYXYRB5]
$ another ack --agent worker 01M77HNNED9HZY6CKDHEZTPNSS
1 updated, 0 unchanged
$ another inbox --agent worker
worker@shop: 0 unread (0 urgent), 0 awaiting ack, 1 request(s)
        2026-11-12T09:33:24.813Z  lead@shop        Fix the flaky test  [01M77HNNED9HZY6CKDHEZTPNSS]

requests:
open      -                Fix the flaky test  (from lead@shop due 2026-11-12T09:48:24.813Z)  [01M77HNNEDVZ92ZVSKQEYXYRB5]
```

`awaiting ack` went from 1 to 0. The request line did not change. Reading or acknowledging a message never claims its request.

## 3. The worker claims it, then stops

```text
$ another request claim --agent worker 01M77HNNEDVZ92ZVSKQEYXYRB5
claimed   worker@shop      Fix the flaky test  (from lead@shop due 2026-11-12T09:48:24.813Z)  [01M77HNNEDVZ92ZVSKQEYXYRB5]
```

Now the worker's session crashes, or its context runs out, or it just moves on. It never runs `request complete`.

## 4. The sender can see the work is unfinished

Nothing arrives in the sender's inbox, because nothing was sent. The request list and the activity feed show a claim and no completion.

```text
$ another inbox --agent lead
lead@shop: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
$ another requests --status claimed
claimed   worker@shop      Fix the flaky test  (from lead@shop due 2026-11-12T09:48:24.813Z)  [01M77HNNEDVZ92ZVSKQEYXYRB5]
$ another activity --since 1h --kinds request
2026-11-12T09:33:24.813Z  request  lead@shop opened request: Fix the flaky test
2026-11-12T09:33:24.866Z  request  worker@shop claimed request: Fix the flaky test
```

`another requests --owner worker` lists everything that agent currently owns. The sender's inbox counts only requests sent to the sender, so a sender checks `another requests`, not its own inbox, to see whether work it opened has an owner.

## 5a. The worker comes back and completes it

`complete` marks the request `done` and mails the requester a `Done:` message in the original thread. `--note` becomes the body; without one the body is `Request <id> is done.`

```text
$ another request complete --agent worker 01M77HNNEDVZ92ZVSKQEYXYRB5 --note "Fixed in the retry branch"
done      worker@shop      Fix the flaky test  (from lead@shop due 2026-11-12T09:48:24.813Z)  [01M77HNNEDVZ92ZVSKQEYXYRB5]
$ another inbox --agent lead
lead@shop: 1 unread (0 urgent), 0 awaiting ack, 0 request(s)
   new  2026-11-12T09:33:24.901Z  worker@shop      Done: Fix the flaky test  [01M77HNNH6XPXEDYZ664XR2PKY]
$ another threads --agent lead
2026-11-12T09:33:24.901Z    2 msg   1 unread  Fix the flaky test  (lead@shop, worker@shop)  [01M77HNNED9HZY6CKDHEZTPNSS]
```

That `Done:` mail is the only thing the sender's inbox will ever receive about the request. Only the owner can complete a claimed request. A done request cannot be claimed again. If the creator completes its own request, no `Done:` mail is sent.

## 5b. Or the worker gives it back

`release` returns the request to `open` with no owner, so anyone can claim it. Only the current owner can release.

```text
$ another request release --agent worker 01M77HNNJ0T10Z74VKWG0YWZAX --note "Blocked on the fixture"
open      -                Port the fixture  (from lead@shop due 2026-11-12T09:48:24.928Z)  [01M77HNNJ0T10Z74VKWG0YWZAX]
```

The note is stored on the request. `release` does not mail anyone, so tell the requester if they need to know.

## 5c. Or nobody comes back: stale takeover

This third request was opened with `--due 5` so the deadline passes quickly. The worker acked, claimed, and stopped.

```text
$ another request claim --agent worker 01M77HNNKQAN037F028JXCZKH7
claimed   worker@shop      Rotate the log  (from lead@shop due 2026-11-12T09:33:29.983Z)  [01M77HNNKQAN037F028JXCZKH7]
```

Past the due time the request becomes `stale`. `another sweep` applies that now; `another requests` and `another inbox` also sweep before they list, and a running `another serve` sweeps every 30 seconds.

```text
$ another sweep
expired 0 hold(s), 1 request(s) went stale
$ another requests --status stale
stale     worker@shop      Rotate the log  (from lead@shop due 2026-11-12T09:33:29.983Z)  [01M77HNNKQAN037F028JXCZKH7]
```

Stale keeps its owner. Another agent cannot release it, because release is for the owner:

```text
$ another request release --agent reviewer 01M77HNNKQAN037F028JXCZKH7
another: request 01M77HNNKQAN037F028JXCZKH7 is owned by worker@shop, not reviewer@shop
```

That exits with status 1. Takeover is a claim. Any agent may claim a stale request, even from its current owner. Pass `--due` to set a new deadline; without it, the old past-due deadline is cleared.

```text
$ another request claim --agent reviewer 01M77HNNKQAN037F028JXCZKH7 --due 600
claimed   reviewer@shop    Rotate the log  (from lead@shop due 2026-11-12T09:43:31.090Z)  [01M77HNNKQAN037F028JXCZKH7]
$ another request complete --agent reviewer 01M77HNNKQAN037F028JXCZKH7 --note "Rotated. The worker had not touched src/log.rs."
done      reviewer@shop    Rotate the log  (from lead@shop due 2026-11-12T09:43:31.090Z)  [01M77HNNKQAN037F028JXCZKH7]
$ another inbox --agent lead
lead@shop: 2 unread (0 urgent), 0 awaiting ack, 0 request(s)
   new  2026-11-12T09:33:31.108Z  reviewer@shop    Done: Rotate the log  [01M77HNVK4EKJ7FJ1XTFKG416H]
   new  2026-11-12T09:33:24.901Z  worker@shop      Done: Fix the flaky test  [01M77HNNH6XPXEDYZ664XR2PKY]
```

The `Done:` mail names who finished it. Stale does not mean abandoned: the first owner may still be working. Check `another activity`, or ask, before taking over.

The whole run, as the activity feed recorded it:

```text
$ another activity --since 1h --kinds request
2026-11-12T09:33:24.813Z  request  lead@shop opened request: Fix the flaky test
2026-11-12T09:33:24.866Z  request  worker@shop claimed request: Fix the flaky test
2026-11-12T09:33:24.901Z  request  worker@shop completed request: Fix the flaky test
2026-11-12T09:33:24.928Z  request  lead@shop opened request: Port the fixture
2026-11-12T09:33:24.956Z  request  worker@shop claimed request: Port the fixture
2026-11-12T09:33:24.965Z  request  worker@shop released request: Port the fixture
2026-11-12T09:33:24.983Z  request  lead@shop opened request: Rotate the log
2026-11-12T09:33:25.011Z  request  worker@shop claimed request: Rotate the log
2026-11-12T09:33:31.038Z  request  request went stale (past due): Rotate the log
2026-11-12T09:33:31.090Z  request  reviewer@shop claimed request: Rotate the log
2026-11-12T09:33:31.108Z  request  reviewer@shop completed request: Rotate the log
```

## What this does not do

- A request without `--due` never goes stale on its own. If the owner stops, the request stays `claimed` until the owner releases or completes it, or the creator cancels it (`another request cancel`). Set a due time on work you may need to take over.
- Nothing watches the worker. A crash leaves the request exactly as it was. The signals are the request status, the due time, and the activity feed.
- `complete` records a claim of completion. It does not run tests or check a diff.
- Hooks announce new mail at session start, at prompts, and after a tool call ([HOOKS.md](HOOKS.md)). The session-start digest counts stale requests, but nothing is pushed to anyone when a request goes stale. The daemon's wake push is for unread mail ([DAEMON.md](DAEMON.md)); `another wake` nudges an agent with unread mail or open requests ([WAKE.md](WAKE.md)).
- Requests stay on one node. Sending `--request` to an agent on another node fails; send plain mail instead.

MCP clients do the same through `send_message` with `create_request`, `acknowledge`, and `claim_request` with an `action` of `claim`, `release`, `complete` or `cancel`.
