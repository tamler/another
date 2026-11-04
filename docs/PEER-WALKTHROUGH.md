# Walkthrough: two peer agents, no manager

Another works with peers only. You don't need a manager bot: agents coordinate with each other, and a person answers the questions that need one. This walkthrough uses two agents, `claude` and `codex`, sharing one repository. They split the work into lanes, hold files, hit a conflict and resolve it, and ask a person one question. Everything happens from the shell, so you can follow it without either client.

It takes about five minutes. You need `another` on your `PATH`. `jq` is used once, to pick out a decision id. CI runs this page's commands exactly as written (`scripts/run-doc-commands.py docs/PEER-WALKTHROUGH.md "## Run it"`).

## Run it

Use a scratch home and project, so your real `~/.another` is left alone:

```bash
export ANOTHER_HOME="$(mktemp -d)/another" && mkdir -p "$ANOTHER_HOME/../shop" && cd "$ANOTHER_HOME/../shop"
```

```bash
another init
```

### 1. Two peers in one repository

Each agent is registered with its program and the project. Its address becomes `name@project`, and the project is named after the folder, `shop`:

```bash
another agent register claude --program claude --project .
```

```bash
another agent register codex --program codex --project .
```

The person who answers questions is an agent too, with the program `human`. Make them the node's decider, so questions that name nobody go to them:

```bash
another agent register jacob --program human
```

```bash
another config decider jacob
```

### 2. Split the work into lanes

No one hands out work. Each agent creates the lane it will lead, with a charter:

```bash
another lane create checkout --agent claude@shop --lead claude@shop --charter "Rework the checkout flow under src/checkout"
```

```bash
another lane create search --agent codex@shop --lead codex@shop --charter "Add fuzzy search under src/search"
```

```bash
another lane list
```

### 3. Hold what you are changing

A hold claims paths for a while (30 minutes by default; `another heartbeat` renews it). It takes the lane its agent leads, so each lane's holds are grouped together:

```bash
another hold acquire --agent claude@shop 'src/checkout/**' --reason "checkout rework"
```

```bash
another hold acquire --agent codex@shop 'src/search/**' --reason "fuzzy search"
```

```bash
another holds --by-lane
```

### 4. A conflict, and how peers resolve it

`codex` needs a helper in `src/checkout/cart.rs`, which another lane holds. The acquire is refused with exit code 3, and nothing is granted:

<!-- doc-test: exit 3 -->
```bash
another hold acquire --agent codex@shop src/checkout/cart.rs --reason "search hook"
```

`claude` leads the holding lane, so the system agent mails it about the conflict. That way the holder learns about it without anyone watching:

```bash
another inbox --agent claude@shop
```

`codex` asks for the file with a request, which is mail with an owner and a status:

```bash
another send --from codex@shop --to claude@shop --subject "Need src/checkout/cart.rs" --body "I need to add a search hook to cart.rs; release it when you can?" --request
```

`claude` finishes the cart work, reports progress on its lane, and narrows its hold to the file it still needs. It does this by releasing every hold it has, then holding `payment.rs` alone:

```bash
another lane report checkout --agent claude@shop --summary "Cart refactor done; releasing cart.rs" --progress 70
```

```bash
another hold release --agent claude@shop $(another holds --json | jq -r '.[] | select(.agent == "claude@shop") | .hold_id')
```

```bash
another hold acquire --agent claude@shop src/checkout/payment.rs --reason "payment step"
```

Now `codex` gets the file, for its own lane:

```bash
another hold acquire --agent codex@shop src/checkout/cart.rs --lane search --reason "search hook"
```

With the pre-edit hook (`another hook install --pre-edit`), Claude Code and Codex run this same check before every edit. An edit to a file another lane holds is denied with the reason shown above.

### 5. Ask the person, not a manager

Some calls belong to a person. `codex` asks a structured question. It can set a deadline and what happens when it passes; here it waits:

```bash
ID=$(another decision ask --agent codex@shop --question "Ship fuzzy search on by default, or behind a flag?" --option default="On by default" --option flag="Behind a flag" --json | jq -r .decision_id)
```

```bash
another decision show "$ID"
```

The person answers from this machine. They could also answer over HTTP, or from their phone with their own token (see [HUMAN-DECISIONS.md](HUMAN-DECISIONS.md)). The answer goes to `codex` as mail in the decision's thread:

```bash
another decision answer "$ID" flag --text "Flag first; we'll flip it after a week"
```

```bash
another inbox --agent codex@shop
```

The asking agent can never answer its own question. Over a token, its answer is refused even when it claims to speak for the person.

### 6. Finish

Each lane closes with a DONE report, and the summary shows what happened:

```bash
another lane done checkout --agent claude@shop --outcome shipped --summary "New checkout flow" --pr https://example.test/pr/1
```

```bash
another lane done search --agent codex@shop --outcome shipped --summary "Fuzzy search behind a flag" --pr https://example.test/pr/2
```

```bash
another summary
```

## What you didn't need

- **A manager agent.** The lanes were created and led by the agents themselves. The conflict reached the holder by mail, and the question went to a person.
- **A running server.** Everything above is CLI against the local store. Agents reach the same store over MCP (`another serve --stdio`), and hooks inject the inbox digest at session start ([HOOKS.md](HOOKS.md)).

To add a manager bot later, see [MANAGER-BOT.md](MANAGER-BOT.md). It uses the same tools and adds nothing that peers depend on.
