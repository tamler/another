# Inbox: read, acknowledge, reply

Three things happen to a message after it lands, and each is a separate command:

| Fact | Recorded by | What the inbox shows |
|---|---|---|
| The agent fetched its inbox | nothing | The line still says `new` |
| The agent read the message | `another ack <id> --mark-read-only` | `ack?` while an ack is still owed, otherwise no marker |
| The agent acknowledged it | `another ack <id>` | No marker; `awaiting ack` drops |
| The agent answered | `another reply <id> --from <me> --body "..."` | A reply in the same thread, in the original sender's inbox |

The walkthrough below is a recorded run. It was captured on 12 Nov 2026 from `another 0.1.0` built from `5ec0a58`; the Rust source on `main` at `d474de9` differs from `5ec0a58` only in test files. It ran against a temporary project named `shop` in a temporary `ANOTHER_HOME`, with no hooks installed and no `another serve` running. Ids and timestamps are as recorded; yours will differ. Where a command needs an id from an earlier step, copy it from that step's output. The temporary path is shown as `/tmp/demo`.

The site has the same example at [toanother.one/guides/coordinate-local-agents/](https://toanother.one/guides/coordinate-local-agents/#recorded). Requests add a status and can be claimed, assigning an owner; see [REQUESTS.md](REQUESTS.md).

The flag spelling below has been updated to `--mark-read-only`; outputs remain from the recorded run. The old `--read-only` spelling remains a compatibility alias with no removal scheduled. Both spellings change the acting recipient's read state and skip acknowledgement; neither is a read-only operation.

## Setup

Two agents in one project: `writer` sends, `reader` receives.

```text
$ another init
$ another agent register writer --program claude --project .
registered writer@shop (claude)
$ another agent register reader --program codex --project .
registered reader@shop (codex)
```

## 1. Send with an ack request

`--ack` asks the recipient to acknowledge. Without `--request` or `--due`, no request is opened, so `send` prints one id.

```text
$ another send --from writer --to reader --subject "Parser review" --body "Please check the error paths." --ack
sent 01M77MXG3ADR38R971JAMA7KPV to reader@shop
```

## 2. Fetching does not mark anything read

`--unread` lists only unread messages. The header counts unread, urgent, awaiting ack, and the requests the agent should look at. Fetching twice leaves the counts and the `new` marker unchanged.

```text
$ another inbox --agent reader --unread
reader@shop: 1 unread (0 urgent), 1 awaiting ack, 0 request(s)
   new  2026-11-12T10:30:07.209Z  writer@shop      Parser review  [01M77MXG3ADR38R971JAMA7KPV]
$ another inbox --agent reader --unread
reader@shop: 1 unread (0 urgent), 1 awaiting ack, 0 request(s)
   new  2026-11-12T10:30:07.209Z  writer@shop      Parser review  [01M77MXG3ADR38R971JAMA7KPV]
```

## 3. Mark it read without acknowledging

`--mark-read-only` records that the acting agent read it without acknowledging it. Other recipients keep their own unread state. The message leaves the agent's `--unread` list. The full inbox still lists it, now marked `ack?`, and `awaiting ack` is still 1.

```text
$ another ack --agent reader 01M77MXG3ADR38R971JAMA7KPV --mark-read-only
1 updated, 0 unchanged
$ another inbox --agent reader --unread
reader@shop: 0 unread (0 urgent), 1 awaiting ack, 0 request(s)
$ another inbox --agent reader
reader@shop: 0 unread (0 urgent), 1 awaiting ack, 0 request(s)
   ack? 2026-11-12T10:30:07.209Z  writer@shop      Parser review  [01M77MXG3ADR38R971JAMA7KPV]
```

## 4. Acknowledge it

`ack` records receipt for the acting agent. The marker clears and `awaiting ack` drops to 0. The message stays in the inbox; nothing is deleted. An ack says nothing about any work the message asked for.

```text
$ another ack --agent reader 01M77MXG3ADR38R971JAMA7KPV
1 updated, 0 unchanged
$ another inbox --agent reader
reader@shop: 0 unread (0 urgent), 0 awaiting ack, 0 request(s)
        2026-11-12T10:30:07.209Z  writer@shop      Parser review  [01M77MXG3ADR38R971JAMA7KPV]
```

## 5. Reply in the thread

`reply` takes the message id and the replying agent. It goes to the original sender and keeps the thread. `--all` adds the original `to` and `cc` recipients. The replying agent is dropped from both lists, so a reply to your own message goes to the original recipients instead. The subject gets a `Re:` prefix unless the original subject already starts with `re:` in any letter case. The sender sees the reply as new mail.

```text
$ another reply 01M77MXG3ADR38R971JAMA7KPV --from reader --body "Looked. The timeout path has no test yet."
sent 01M77MXG5VWNQYK5SCDXEW8D6D to writer@shop
$ another inbox --agent writer
writer@shop: 1 unread (0 urgent), 0 awaiting ack, 0 request(s)
   new  2026-11-12T10:30:07.291Z  reader@shop      Re: Parser review  [01M77MXG5VWNQYK5SCDXEW8D6D]
```

## 6. Follow the thread

A thread's id is the id of the message that started it. `threads` lists the agent's threads with counts; `thread` prints every message with its body.

```text
$ another threads --agent reader
2026-11-12T10:30:07.291Z    2 msg   0 unread  Parser review  (reader@shop, writer@shop)  [01M77MXG3ADR38R971JAMA7KPV]
$ another thread 01M77MXG3ADR38R971JAMA7KPV
2026-11-12T10:30:07.209Z writer@shop -> reader@shop  [01M77MXG3ADR38R971JAMA7KPV]
  Parser review

  Please check the error paths.

2026-11-12T10:30:07.291Z reader@shop -> writer@shop  [01M77MXG5VWNQYK5SCDXEW8D6D]
  Re: Parser review

  Looked. The timeout path has no test yet.
```

## 7. The summary keeps subjects, not bodies

```text
$ another activity --summary
8 event(s) since 2026-11-12T09:30:07.335Z; active agents: reader@shop, writer@shop; requests: 0 open, 0 claimed, 0 stale; 0 live hold(s).
- node local created
- project shop registered at /tmp/demo/work/shop
- writer@shop registered (claude)
- reader@shop registered (codex)
- writer@shop sent message to reader@shop: Parser review
- reader@shop sent message to writer@shop: Re: Parser review
```

The count of 8 includes the read and the ack from steps 3 and 4; the highlights leave them out. The full `another activity` feed lists them as `delivery.read` and `delivery.acked` events, still without the body. `another status` prints the node id, the database path, and each agent's last-seen time.

## What this does not do

- Nothing here notifies the other agent. The hooks announce new mail at session start, at prompts, and after a tool call ([HOOKS.md](HOOKS.md)); `another wake` nudges an idle terminal ([WAKE.md](WAKE.md)).
- Read and ack state is per recipient. One agent's ack says nothing about another recipient of the same message.
- An ack is receipt. If the mail asks for work, send it with `--request` so an agent can claim it and become its owner ([REQUESTS.md](REQUESTS.md)).
- Bodies stay in the mail store. They are left out of the digest and the activity feed, but anyone with access to `ANOTHER_HOME` can read them.

MCP clients do the same through `send_message` (with `reply_to` for a reply), `fetch_inbox`, `acknowledge`, and `threads`.

For `acknowledge`, `ack: false` marks messages read without acknowledging them. It changes delivery state for the acting recipient only, just like `another ack --mark-read-only`.
