# Open pull request triage

Status: draft for owner review, 17 Nov 2026. Nothing has been closed, merged or changed. This lists the 101 pull requests open on 17 Nov 2026 (main at `5df47a98`) against [V0.1.0-SCOPE.md](V0.1.0-SCOPE.md). It was made from each pull request's title, description, file list and base branch; the code was not re-reviewed, and no CI state was checked.

## How to read it

- **Keep**: it serves a scope id. It is rebased, independently reviewed, and merged with CI green.
- **Fold**: a useful piece moves into the named scope item, usually as part of one batch pull request. The original is closed once the batch exists.
- **Close**: cut, frozen or superseded. The branch stays, so nothing is lost, and a finding behind it becomes a proposed queue item if it still matters.
- **Owner**: a security or data-loss finding. The scope sends these to the owner personally. The architect's recommendation is given; the owner decides.

Three rules produced most of the answers:

1. A fix or test on a frozen surface (OAuth, bearer-token peers, the resource registry, reaping, the dashboard, Windows) is closed unless it is a security or data-loss fix the owner admits.
2. A test-only pull request is folded when a v0.1.0 item is about to change the code it pins (requests for M1, holds for M4, the store for M3, the hub path for M9, wake targets for M5). Otherwise it is closed: the scope cuts open-ended coverage with no admitted item behind it.
3. Nothing merges on a reserved schema or decision number. Numbers are assigned at merge.

## Totals

| Verdict | Count |
|---|---|
| Keep | 15 |
| Fold | 24 |
| Close | 52 |
| Owner decision | 10 |

## Owner decisions (security and data loss)

| PR | Title | Recommendation | Reason |
|---|---|---|---|
| #248 | A peer's node list may not rename other nodes or take names | Keep, M9 | A paired node can redirect mail meant for another node. This is on the hub path v0.1.0 runs. |
| #253 | An out-of-range peer port or pid no longer breaks every listing | Keep, M9 | One bad synced row breaks `resources`, the dashboard and doctor's scan on the hub. A bug in one's own node can cause it. |
| #197 | Refuse weak Ed25519 keys and verify signatures strictly | Keep, M9 | Signature checking is what the hub's authority rests on; small and contained. |
| #178 | Refuse control characters in hold reasons, agent and resource text | Keep, M6 | Stored text reaches the digest every agent reads at each turn; a forged line there is an instruction to an agent. |
| #245 | The session digest shows stored text as one safe line | Keep, M6 | Same class as #178, at the digest itself. Merge after #178 and drop the overlap. |
| #176 | Human output never passes terminal control characters | Keep, M4 and M6 | Output side of #178. Conflicts with main and uses a reserved decision number; rebase and renumber. |
| #222 | Keep credentials out of Debug output | Close | Secrets leak only if a debug line is logged, on frozen surfaces (OAuth, tokens) the owner does not run. Record as a proposed item. |
| #198 | IPv6 forms that embed IPv4 get the IPv4 rules in the metadata fetch guard | Close | Hosted OAuth is frozen and not run. Record as a proposed item that must be fixed before OAuth is unfrozen. |
| #233 | OAuth audit rows record the socket address | Close | Same: frozen OAuth, and it conflicts with main. Record as proposed. |
| #246 | Hold a peer's mail body to the local 256 KiB limit | Fold, M9 | Low risk between machines one person owns. Fold with #247 into one "a peer's events obey the local rules" change if the owner admits it. |

## Keep

| PR | Title | Scope | Reason |
|---|---|---|---|
| #165 | Resolve a local caller's project paths like the CLI does | M4 | Through MCP, one directory became two projects and a hold check missed a live hold. Ready for review and mergeable. |
| #179 | Nested projects outside git see each other's holds | M4 | Two agents could both get an exclusive hold on one file. |
| #180 | Refuse a held path over 4096 bytes | M4 | A long path panics the hold check. |
| #240 | Resolve a relative gitdir from the `.git` file's real location | M4 | A worktree reached through a symlink did not share holds with its main checkout. Stacked on #237; merge them as one. |
| #237 | Tests for relative gitdir files from worktrees and submodules | M4 | The fixture #240 needs. Goes in with #240. |
| #149 | A waiter's session end ends its waits | M4 | Stops a "hold free" ping going to an agent that has gone. Its background review failed; needs a fresh independent review. |
| #214 | Push records stay on their node | M9 | Sync correctness on the hub path. |
| #217 | An older build refuses a newer database and leaves it untouched | M3 | Exactly the M3 done condition. |
| #239 | Doctor fails on a negative schema version | M3 | Doctor gave advice that could not work. |
| #163 | Hook status reports drift | M6 | Notices a hook wrapper that points at an old binary or home, part of what broke on 17 Nov. |
| #168 | 50-agent races for holds, edit-guard holds and request claims | M1, M4 | The harness the M1 done condition ("fifty agents racing for `next`") needs. |
| #150 | Release readiness: NOTICE, archive checks, static musl, tag version | M10 | Direct M10 work. |
| #147 | Opt-in Cursor edit guard | N3 | Nice to have. Parked until the must-haves are on track. |
| #159 | Advisory pre-commit hold guard | N4 | Nice to have, but 2,400 lines and conflicting, against a scope that says "small". Parked; it must shrink before review. |
| #171 | Check client hooks live: results for Claude Code, Codex and Grok | M6 | Evidence for the M6 done condition. Docs only. |

## Fold

| PR | Title | Into | What moves |
|---|---|---|---|
| #152 | Group work queues with atomic `request next` (schema v14) | M1 | The atomic `next`, priority order and tests. It needs the admission states, the approver record and the per-agent limit, must not depend on notes (#143) or #140, and gets its schema number at merge. A new spec comes first. |
| #140 | #136 integrated on hold fixes | M3 | Only the backup-before-migrate code and the frozen schema catalog tests. The rest is #136's session-bound admission and checked-handle edits, a new hold feature outside M4. |
| #144 | Herdr-compatible agent status | M5 | The mapping of agent state to working, idle, blocked and unknown feeds `status(agent)`. The board and dashboard changes do not move. |
| #205 | A keyless legacy database gets a key and signs only its own events | M3 | Upgrade-path test. |
| #200 | The background sweeper expires lapsed work and stops when cancelled | M8 | Daemon behaviour test. |
| #203 | Wake target validation, auto versus manual, and what each clear removes | M5 | Pins target handling before the adapter interface changes it. |
| #235 | The case-folding probe climbs past directory names without letters | M4 | Hold test batch. |
| #213 | Hold check exit codes, shared rules, and that it grants nothing | M4 | Hold test batch. |
| #181 | Renew and release by id, locally and in a token's scope | M4 | Hold test batch; drop the token-scope half. |
| #236 | Request list filters by owner and status | M1 | Request test batch: pins today's behaviour before the queue changes it. |
| #216 | Who may complete a request, and who is told | M1 | Request test batch. |
| #201 | Request claim, release and cancel follow ownership, with exit codes | M1 | Request test batch. |
| #212 | A hub refuses relayed and never-shared events from a spoke | M9 | Hub path test batch. |
| #208 | Pairing and sync refusals that had no test | M9 | Hub path test batch. |
| #204 | `another sweep` and `another pair revoke` | M9 | The pair-revoke half only. |
| #247 | A peer's agent name follows the local name rule | M9 | With #246, one "a peer's events obey the local rules" change, if the owner admits it. |
| #154 | Correct stale claims in README, HOOKS, RESOURCES and FEDERATION | M10 | Conflicts with the README changes of 17 Nov. Redo inside the M10 docs pass. |
| #169 | An unauthenticated loopback server answers every local account | M10 | A true statement the M10 docs pass should carry. |
| #175 | Changelog for 13 to 16 Nov merges | M10 | Release notes input. |
| #70 | Signed origin authority; hub decides lease timing (option A) | v0.2.0 | Origin authority is already on main (D72). The hub-decided lease timing is input to the v0.2.0 cross-node spec. Close the pull request; cite it in that spec. |
| #173 | Events per day and holds and requests gauges in stats | M7 | Stats stays as it is. If the M7 list needs a count, take it from here. Conflicts with main. |
| #170 | Help on every argument, JSON usage errors, completions | M10 | Only the help text gaps, in the M10 docs pass. Completions and the rest are not in scope. Conflicts with main. |
| #230 | Pin feed lines, the summary cursor and `whats_new` limits | M6 | The digest reads the feed; keep the feed-line tests with the M6 hook work. |
| #199 | Agent names and address resolution | M5 | `list()` matches transport agents to registered names; keep the resolution tests with it. |

## Close

| PR | Title | Reason |
|---|---|---|
| #136 | Session-bound admission and opt-in checked-handle edits | Superseded by #140, and a new hold feature outside M4. |
| #143 | Project notes (v13) | Cut from v0.1.0. |
| #192 | Key rotation primitives | Cut: revoke and pair again is enough. |
| #164 | `another log export` | Cut. |
| #161 | Resource list, borrow, return and `borrow --wait` | Resource registry is frozen: no new features. Conflicts with main. |
| #254 | Cap a resource's endpoint and purpose | Frozen resource registry; low risk. Finding stays as a proposed item. |
| #250 | A peer's `resource.registered` follows the local name rule | Frozen resource registry; low risk. |
| #244 | Cap resource metadata at 64 KiB | Frozen resource registry; low risk. |
| #225 | `reachable` cuts the endpoint authority correctly | Frozen: borrowing another node's resource. |
| #182 | `derive_port` handles a URL query and bare IPv6 | Frozen: orphan listing. |
| #189 | `another sweep` text also counts expired resource leases | Cosmetic, on a frozen surface. |
| #207 | An empty audit log says so | Cosmetic; no scope item. |
| #232 | A token registers its agent by any spelling of its own project | Bearer-token peers are frozen; not a security fix. |
| #255 | Dashboard: a long thread shows its newest forty | Dashboard is frozen; test only. |
| #243 | Dashboard: an idle owner keeps their holds for thirty minutes | Dashboard is frozen; test only. |
| #223 | Dashboard: the activity endpoint checks its filters | Dashboard is frozen; test only. |
| #220 | Dashboard: foreign holds are not released | Dashboard is frozen; test only. |
| #251 | OAuth: client name and redirect URI limits | OAuth is frozen; test only. |
| #224 | OAuth: what registration, authorize and token refuse | OAuth is frozen; test only. |
| #195 | OAuth: the IPv6 half of the metadata fetch guard | OAuth is frozen; test only. |
| #194 | CLI: `another oauth` subcommands | OAuth is frozen; test only. |
| #193 | OAuth: deny, grant revocation and the revoke endpoint | OAuth is frozen; test only. |
| #190 | OAuth: an operator revoke ends a grant's tokens | OAuth is frozen; test only. |
| #184 | OAuth: denying a waiting client | OAuth is frozen; test only. |
| #229 | Remote: a token registers its agent only in its own project | Bearer-token peers are frozen; test only. |
| #191 | Remote: `another remote up` with a fake cloudflared | Frozen; test only. |
| #187 | Remote: a connection acts only as its token's agent | Bearer-token peers are frozen; test only. |
| #186 | Tokens: remote reply access by thread membership | Bearer-token peers are frozen; test only. |
| #202 | Requests: a token changes only requests it created, received or owns | Bearer-token peers are frozen; test only. |
| #241 | Resources: non-object metadata and bad reap grace are refused | Frozen; test only. |
| #227 | MCP: resource tools map exclusive, ttl and stopped onto the store | Frozen; test only. |
| #215 | CLI: resource heartbeat renews only the caller's leases | Frozen; test only. |
| #211 | Resources: lease renewal across nodes | Frozen; test only. |
| #157 | MCP: resource tools' lifecycle, error codes and remote scope | Frozen; test only. |
| #252 | Stats: wakes are counted in full but listed fifty | Stats stays as is; test only with no item behind it. |
| #242 | Mail: every importance survives storage | Mail is unchanged in v0.1.0; coverage with no item behind it. |
| #221 | Mail: what a send or an ack refuses | Same. |
| #219 | Mail: message limits, thread refusals and replies to one's own mail | Same. |
| #234 | Identity: same-named project directories get distinct slugs | Coverage with no item behind it. |
| #228 | CLI: `--expires` units and bad durations | Coverage with no item behind it. |
| #209 | CLI: activity summary, kinds and since | Coverage with no item behind it. |
| #196 | CLI: human output for agents, mail, threads, events, tokens, audit, peers, wake | Coverage with no item behind it. |
| #183 | CLI: agent show, hold renew, node name, request cancel, audit | Coverage with no item behind it. |
| #188 | Wake: prune for tmux and cmux targets | The dedicated tmux and cmux adapters are being replaced (M5). |
| #111 | Close linked release requests and dedupe pushes | Already on main through #137 (checked on 16 Nov in the earlier close list). |
| #106 | CLI project paths resolve like the edit hook | Already on main as D60 (same check). |
| #83 | Clarify manual keep-leases limits | Main has the same clarification in other words. |
| #82 | Accept the approved sitemap route set | Its target file and base were never merged. |
| #81 | README: list all ten task guides | Superseded by main's Guides section. |
| #76 | Adapt role-barrier fixtures to origin authority | Its files are not on main; it belongs to a line of work that was replaced. |
| #74 | Reconcile bug 12 status and changelog entries | Its target files are not on main. |
| #62 | Coverage pass 3 | Built on an unmerged line; its test files do not match main. Coverage with no item behind it. |

## What happens after review

1. The owner marks any row to change, and decides the ten owner rows.
2. The architect posts the verdict and reason as a comment on each pull request, then closes the Close rows. Branches are not deleted.
3. Keep rows become ready queue items. Each still needs an independent review and green CI.
4. Fold rows are closed once the batch pull request that carries their content is open.
5. A finding behind a closed fix is written down as a proposed queue item, so it is not lost and not built until admitted.
