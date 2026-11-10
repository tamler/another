# Promotion

Nothing in this file has been posted. Do not post it from CI, from a pull request, or from any account except the personal X account **@JTamler**. There is no Another account. Do not mention other products, companies, or accounts.

Refresh any number the day you post. The figures below were looked up on 10 Nov 2026 at `github.com/tamler/another`: **0 stars, 1 fork, no GitHub Release**. Milestones 1 through 6 are on main. OAuth for hosted MCP clients (ChatGPT, Claude.ai) is not merged. The local dashboard is loopback only (`another dashboard`). If a draft below is no longer true, edit it before it goes out. Do not ask for stars, upvotes, reposts, or comments-as-a-favor.

`python3 scripts/check-copy.py` lints this file and `site/`.

## Ready-to-publish queue

One post per item. Plain text, under 280 characters, so it fits in a single X post from @JTamler. Launch is 1. Post 2 and 3 in the same week if you are around to reply. Then about two a week. Skip a number if the fact went stale. A GIF only if you record one that day. The site has a text transcript of a handoff, not a GIF.

### 1. Launch

I kept pasting messages between Claude Code and Codex. They still edited the same files. I built Another so one session can hand work to the other and see which paths are taken. Local, Apache-2.0. https://toanother.one

### 2. Holds do not lock

A path hold is not a lock. `another hold check` exits 3 when paths overlap, and the other agent can still write the file. I found that out by assuming it would block. Use a second checkout when you need a real boundary.

### 3. The public numbers

Public numbers for github.com/tamler/another on 10 Nov 2026: 0 stars, 1 fork, no GitHub Release. Milestones 1 through 6 are on main. Writing the zero down so I do not inflate it later.

### 4. The handoff I type

Handoff I actually use: `another send --from alice --to bob --subject "Review the parser" --request`. The other session claims it. I still read the diff. I stopped copying the paragraph between terminals.

### 5. One model server

Both agents were about to start a model server on the same port. One runs `another resource register`. The other borrows it. `another reap` does nothing until I pass `--confirm`, and a remote peer cannot reap.

### 6. The hook I forgot

Unread mail sat there until the hook existed. `another hook install --client all` backs up the client config and leaves other hooks alone. In Codex you still open `/hooks` and trust them. I forgot that once.

### 7. What is not shipped

The dashboard is loopback only: `another dashboard`. ChatGPT and Claude.ai cannot connect yet. OAuth for those hosted clients is not merged. Agents still use `another serve --stdio` on the machine where they run.

### 8. Wake, mis-aimed

I aimed `another wake` at an agent that was mid-edit. Useless. Wake types into an idle tmux pane, and it skips the agent when nothing is waiting. It does not cut into a running turn.

### 9. Two machines

Laptop and a box I leave running: federation is a hub I host. Mail crosses. Requests stay on one node. The hub can read the mail it relays. Ordinary SQLite. No end-to-end encryption. I do not want to imply there is.

### 10. The short try

If you run two coding agents in one repo, the short try is the local inbox guide. Temp directory, no hooks installed. Tell me where the handoff is clumsy. https://toanother.one/guides/local-agent-inbox/

## After the queue

Keep going from the same account when something real happens: a release tag, a hold that failed in a way you can describe, a bug number, a star count you just looked up. One post in a quiet week is enough. Two is plenty. Do not invent a cadence you will not keep, and do not backfill days you skipped.

## Show HN

Rules, from <https://news.ycombinator.com/showhn.html>:

- The title starts with `Show HN`.
- It has to be something people can run. A landing page, a signup, or a blog post does not qualify. Link the repo (`https://github.com/tamler/another`) or the getting-started page, and put the `cargo install` command where a reader can run it without an account.
- You built it, and you stay in the thread. Early and rough is fine. Trivial one-offs are not.
- Do not ask anyone to upvote or comment. A new point release is not another Show HN. A real overhaul might be, months later, with a note on what changed.
- There is no official best hour. Rank decays with age, so the first hour matters, and the only part you control is being there to answer. Post when you can sit in the thread.

A HN moderator note on <https://news.ycombinator.com/item?id=22336638> (the 2026-03-28 edit) says not to use an LLM to generate or even lightly edit the text you post. The paragraph below is a factual skeleton. Retype it in your own words before you submit. Do not paste it.

Title to consider:

```
Show HN: Another, local mail and path holds for coding agents
```

Skeleton for the first comment, to rewrite by hand:

```
I run Claude Code and Codex on the same repo. They edited the same files and I was pasting messages between them. Another is a local SQLite inbox: mail, requests with an owner, path holds that expire, an activity feed, and a registry for ports and model servers. Apache-2.0. Rust 1.88. cargo install --locked --git https://github.com/tamler/another another

Holds do not block writes. another hold check exits 3 on overlap. Reaping needs --confirm on that machine and cannot be done remotely. No telemetry. No release binary yet. OAuth for ChatGPT or Claude.ai is not merged. The dashboard is loopback only. Loopback by default.

Repo: https://github.com/tamler/another
```

## Product Hunt

Sources read 10 Nov 2026:

- <https://help.producthunt.com/en/articles/17734646-8-launch-faq-and-myths>
- <https://www.producthunt.com/launch/preparing-for-launch>
- <https://help.producthunt.com/en/articles/3615694-community-guidelines>
- <https://www.producthunt.com/launch/sharing-your-launch>

A scheduled launch goes live at 12:01am Pacific Time on the date you pick. You can schedule from tomorrow out to 30 days. Their own guide says no weekday guarantees a rank. Weekends get fewer big-company launches. The useful constraint is that you can answer comments. If you will be asleep at 12:01am PT and unreachable in the morning, pick a later time on a day you can actually sit there. They also say a personal account should have been in the community at least a week before it submits, and longer is better. Do not pay someone to hunt it. Do not ask for upvotes, do not offer a prize for votes, do not mass-message strangers. A first comment from the maker is the normal opener. A discount code is optional and odd for a free Apache-2.0 tool. Skip it.

Tagline draft:

```
Local inbox and path holds for coding agents you already run
```

First comment draft (edit it; do not leave it sounding like a brochure):

```
I got tired of being the clipboard between Claude Code and Codex. Another is one SQLite database on the machine: mail, an owner on a request, path holds that expire, and a way to borrow a model server that is already running. It does not start the agents.

Install is cargo from the git repo. Rust 1.88. No account. Holds do not lock the file. The dashboard is loopback only (`another dashboard`). Hosted clients like ChatGPT need OAuth that is not merged. https://toanother.one
```

## Reddit

I tried to load `r/LocalLLaMA` rules on 10 Nov 2026 and got HTTP 500. Read the live sidebar the day you post. Do not rely on this file for a rule you have not just opened.

What I could verify:

- Reddit's general expectation is that self-promotion is a small slice of an account that also participates. I did not find a primary help-page that states a numeric percent in this pass. Treat "about 10%" as folklore until you see it on the subreddit.
- r/ClaudeAI: a moderator post, <https://www.reddit.com/r/ClaudeAI/comments/1qe5wtt/>, described Rule 7 as showcase posts that teach, show the work, stay free to try, skip marketing language, and skip affiliate links. Re-read the live rule. A post that is only a link will fail. A post about two Claude Code sessions colliding, with the command and the mistake, is the shape that post was asking for. Another was not "built by Claude" as the star of the story. Say you wrote the tool, and that Claude Code is one of the clients.
- r/rust: I did not find a subreddit-specific promo rule in a primary source. A note about one static binary and SQLite, in a thread that asked, is safer than a link dropped into a new post. Read the sidebar.
- Cursor's forum, not Reddit: <https://forum.cursor.com/guidelines>. Showcase is welcome. Contribute in the forum first. Say you built it. They remove posts that are entirely generated. Do not paste this file in.
- OpenAI developer community: <https://community.openai.com/guidelines>. Sharing something you built is allowed. Repetitive promotion is not. Keep it about Codex, not a cross-post of the X text.

Space these. Same paragraph on five subreddits the same afternoon reads as spam even when each rule would have allowed one post.

r/ClaudeAI draft, only after you re-read Rule 7:

```
Two Claude Code sessions in one checkout were saving the same file. I stopped pasting between them and wrote a local inbox the sessions can share with Codex too. Path holds exit 3 on overlap and do not block the write. Apache-2.0, no account. The short try does not install hooks: https://toanother.one/guides/local-agent-inbox/
```

## Awesome lists and MCP directories

`punkpeye/awesome-mcp-servers`: listing is a pull request, and recent maintainer comments (see PR 2021, merged March 2026, and the bot on later PRs) require a Glama listing whose checks pass, including a Docker image that builds, plus a Glama badge in the README line. Another is installed with `cargo install --git`, not as a Dockerized npm server. Do not open that PR until Glama's current form accepts this binary and you can meet the checklist that day. One PR. No follow-up nag the same week.

Other "awesome" lists for Claude Code or Codex: read that repo's contributing file, match one existing bullet, and stop. Do not spray the same link across every list you can find.

Official MCP registry, <https://modelcontextprotocol.io/registry/quickstart> and <https://github.com/modelcontextprotocol/registry/blob/main/CONTRIBUTING.md>:

- Do not open a PR that edits `data/seed.json`. That file is local seed data. Publish with the `mcp-publisher` CLI.
- The registry is in preview and says it may reset.
- GitHub login names the server `io.github.tamler/...`. Domain auth is a different namespace.
- Packages have to be on a trusted public registry (crates.io, npm, PyPI, Docker, and a few others) so the registry can check you own them. This project is not on crates.io yet. `cargo install --git` will not satisfy that check. Publish the registry entry after a real crates.io release, or read their remote-server docs if you only have a URL. I did not find a path for a git-only binary.

Glama, <https://glama.ai/mcp/servers>: "Add Server" on that page. The awesome-list requirement above is Glama's Docker build, which you should confirm on the form rather than assume I copied it forever.

PulseMCP, <https://www.pulsemcp.com/submit>: submissions were paused when I fetched the page. The page said last updated 3 September 2026, and it pointed people at the official registry. Check again before you try. Do not email them to ask for an exception.

mcp.so: a secondary writeup (mcpfinder.dev, 4 Nov 2026) said the form has a free review and a paid skip of about $39. Open the form and read it. Do not pay the skip by default.

Smithery: the same secondary writeup said the hosted proxy was shutting down around 31 Oct 2026 and that a submit flow was not obvious on the homepage. Look at smithery.ai the day you care. Do not plan the launch around it.

## dev.to

Code of conduct, last updated 31 July 2023: <https://dev.to/code-of-conduct>. It says to follow their AI guidelines and to disclose AI assistance if it was used to create the post. This file was drafted with an agent. If you post any of it, say so, or rewrite it first. Set the canonical URL to the page on toanother.one you are mirroring, if you are mirroring one. One post. Tags that fit: `rust`, `opensource`, `mcp`. Do not serialize the X queue as ten short DEV posts.

Draft title: `I got tired of pasting between Claude Code and Codex`

Draft opener:

```
I run both in one repo. They overwrite files and they cannot see each other's tasks. Another is the local SQLite inbox I put between them: a request with an owner, a path hold that expires, and a registry so the second agent borrows the model server instead of starting one.

The hold does not lock the file. another hold check exits 3 on overlap. I had that wrong at first.

Install, if you want to try the command and not the pitch: cargo install --locked --git https://github.com/tamler/another another

The dashboard is loopback only (`another dashboard`). Hosted clients such as ChatGPT need OAuth that is not on main. 0 GitHub stars on 10 Nov 2026. That number is real and it will be stale, so check it before you repeat it.
```

## Bluesky

Do not create a brand account for the launch. If you already post from a personal Bluesky account, the queue above fits in a 300-character post. Cross-post a few of them, not all ten on the same day. If you do not use Bluesky, skip it.

## Discord

I do not have a verified channel list for the Anthropic, OpenAI, or Cursor servers. Names change. On the day, read the rules, use the showcase channel if there is one, and do not DM people a link. Say you built it. One server, one post, then stop. Cursor's written rules are the forum page linked above. I would rather skip a Discord than guess the channel.

## What I will not do

- Post any of this before the site you intend to send people to is actually deployed.
- Announce OAuth as available before it is merged.
- Invent a star count, a download count, or teams using it.
- Buy votes, ask friends to pile on, or coordinate a launch thread.
- Mention other products or companies in the same post.
