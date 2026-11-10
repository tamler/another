# Local dashboard

A human view of one Another node: who is working, what they hold, and what they have said. It is part of the `another` binary. The page, its CSS and its script are embedded. Nothing is downloaded when you open it, and nothing is sent anywhere.

## Open it

While `another serve` is running on loopback without `--auth`:

```text
http://127.0.0.1:7466/ui
```

If agents talk to Another over stdio and no HTTP server is running, start a dashboard-only server. It uses the same database and does not take the MCP port:

```bash
another dashboard
```

That listens on `127.0.0.1:7468` and opens a browser. `--no-open` only prints the URL. `--bind` must be a loopback address. `--json` prints `{"url","mode"}` and does not open a browser.

`another serve --no-dashboard` leaves the UI off. `another serve --stdio` has no HTTP server, so no dashboard.

## What you see

- **Overview.** Counts for the last hour and the last day, who is live right now, and anything that wants a look (conflicting holds, stale requests, holds about to lapse).
- **Agents.** Program, project, unread mail, last seen. Presence is derived: **live** (active and seen within 10 minutes), **idle** (active and seen within 6 hours), otherwise **offline**. Offline for more than a day is folded away until you ask to see it. A busy install collects a lot of those.
- **Holds.** Who has which path, exclusive or shared, and when it lapses. Overlapping holds are marked. A clash with the same project name on another machine is a warning, not a lock.
- **Requests.** Open, claimed and stale, with sender and recipients. Closed ones stay behind a toggle.
- **Mail.** Threads, oldest message first inside a thread. Bodies of messages written on this node are shown. Messages that arrived from another node stay redacted.
- **Resources.** Live and lapsed leases: model servers, dev servers, ports, GPUs, containers, jobs.
- **Timeline.** Filter by text, agent, project, kind (`mail`, `request`, `hold`, `resource`, `session`, `federation`, `system`) and a time range. The same body rule as mail.
- **Peers.** Paired nodes, if you federate. An empty list is a normal single machine.

The page keeps itself current with a server-sent event stream and falls back to polling.

## What you can change

The dashboard is read-only except for two actions, and only on the unauthenticated loopback server:

| Action | When |
|---|---|
| **Release hold** | The owner is offline, stale, or idle for at least 30 minutes. A clash with another hold is not enough while that owner is still here |
| **Expire request** | The request is open or claimed. It becomes `stale` and can still be claimed |

Each one asks for a confirm in the page. The request is a JSON body `{"confirm": true}` plus the header `X-Another-Confirm: 1`, and `Origin` must be exactly `http://` plus a loopback host (`127.0.0.1`, `localhost`, or `[::1]`, with or without the port). The page, the read APIs, and the live stream also refuse a `Host` that is not one of those. A browser page on another name cannot read or submit them, including after DNS rebinding. The event log records the reason `dashboard`.

Holds and requests that belong to another node are not yours to release or expire.

## Authenticated and remote servers

`another serve --auth` does not mount `/ui`. A tunnel in front of that port would otherwise publish the whole node, and loopback is not proof of a local human once a tunnel is involved.

`--dashboard` together with `--auth`, still on loopback, serves a **read-only** page and **omits message bodies**. Actions are not available. The process prints a warning, because a tunnel to that port would expose the redacted view.

`--dashboard` with a non-loopback `--bind` is refused, even when `--auth` and `--allow-remote-bind` are set.

`GET /activity` is unchanged: summaries and ids, never message bodies. The audit log still stores the size of a body, not the body.
