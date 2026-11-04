# A person answers a decision over HTTP

`claude` needs a call from a person, so it asks a structured question. The person, `owner` (the node's decider), answers over HTTP. No manager bot relays anything.

```bash
sh examples/human-decision/run.sh
```

What happens:

1. **Ask.** `another decision ask` with two options and a context link. The decision mails `owner` and opens a request.
2. **Answer from this machine.** On the plain loopback server (`another serve`), `GET /decisions` lists the question and `POST /decisions/{id}/answer` with `{"value": "wait", "text": …}` answers it. The answer is mailed back to `claude` in the decision's thread.
3. **Answer "from the phone".** An authenticated server (`another serve --auth`) and a token for `owner` (`another token create --agent owner`) are what a phone shortcut or a tunnel would use. Without the token the server returns 401. With it, the answer is recorded as `owner via owner`.
4. **The asker can't answer itself.** `claude`'s own token gets 403, even when it claims to answer for `owner`, and the decision stays open.

In real use, expose the `--auth` server through a tunnel (see [docs/REMOTE.md](../../docs/REMOTE.md)) rather than a public bind.
