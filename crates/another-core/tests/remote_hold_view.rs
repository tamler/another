//! FLE-23 M-H (owner decision Q2): what a remote token sees of holds. A hold is shown when,
//! from the top of its checkout, it overlaps the whole tree of a local project the token's
//! globs allow. The hold list, the activity feed and its summary, and the confined board use
//! that rule; refusals still refuse but name nothing hidden. Local callers are unchanged.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use another_core::*;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const HIDDEN: &str = "the requested path is held by another agent in this checkout";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Make `link` a second name for the directory `target`: a symlink, or on Windows a
/// junction, which needs no privilege.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {made:?}");
    }
}

fn scope(globs: &[String]) -> TokenScope {
    TokenScope {
        token_id: "t".into(),
        agent_id: "a".into(),
        agent: "bob@sub".into(),
        project_globs: globs.to_vec(),
    }
}

/// A repository `shop` with a subdirectory project `shop/sub`, a linked worktree `shop-wt`
/// of the same repository (another checkout), and an unrelated `blog` that is not a
/// repository. alice works in `shop`, bob in `sub`, wanda in `shop-wt`, carol in `blog`.
struct Fx {
    dir: tempfile::TempDir,
    shop: PathBuf,
    sub: PathBuf,
    wt: PathBuf,
    blog: PathBuf,
    store: Store,
    _clock: Arc<AtomicI64>,
    seq: AtomicU64,
}

impl Fx {
    fn new() -> Fx {
        Fx::at(None)
    }

    /// With `spell`, the repository lives at `real/shop` and `spell` is a second name for
    /// `real`; `shop` is registered under its real path and `sub` under the other one.
    fn at(spell: Option<&str>) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let real = root.join("real");
        let shop = real.join("shop");
        write(
            &shop.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        write(&shop.join("sub/README"), "sub\n");
        let sub = match spell {
            None => shop.join("sub"),
            Some(name) => {
                let link = root.join(name);
                link_dir(&real, &link);
                link.join("shop").join("sub")
            }
        };
        let wt = real.join("shop-wt");
        let wt_git = shop.join(".git/worktrees/shop-wt");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        let blog = root.join("blog");
        std::fs::create_dir_all(&blog).unwrap();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&root.join("home"), clock).unwrap();
        for (name, dir) in [
            ("alice", &shop),
            ("bob", &sub),
            ("wanda", &wt),
            ("carol", &blog),
        ] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(path(dir)),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        let fx = Fx {
            dir,
            shop,
            sub,
            wt,
            blog,
            store,
            _clock: handle,
            seq: AtomicU64::new(0),
        };
        // Link every project to its checkout, as using it does.
        for (agent, dir) in [
            ("alice@shop", &fx.shop),
            ("bob@sub", &fx.sub),
            ("wanda@shop-wt", &fx.wt),
            ("carol@blog", &fx.blog),
        ] {
            fx.store
                .check_holds(agent, &path(dir), &["warm-up".to_string()], true)
                .unwrap();
        }
        fx
    }

    fn acquire(&self, agent: &str, dir: &Path, paths: &[&str], wide: bool) -> HoldReport {
        self.store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: path(dir),
                paths: paths.iter().map(|p| p.to_string()).collect(),
                ttl_seconds: None,
                // Shared, so the fixture's holds can overlap one another; an exclusive
                // request still conflicts with each of them.
                exclusive: false,
                reason: Some(format!("{agent} reason")),
                repo_wide: wide,
            })
            .unwrap()
    }

    fn hold(&self, agent: &str, dir: &Path, pattern: &str, wide: bool) -> String {
        let got = self.acquire(agent, dir, &[pattern], wide);
        assert_eq!(got.granted.len(), 1, "{got:?}");
        got.granted[0].hold_id.clone()
    }

    fn sub_only(&self) -> TokenScope {
        scope(&[path(&self.sub)])
    }

    fn project_id(&self, dir: &Path) -> String {
        self.store.project(&path(dir)).unwrap().project_id
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("home").join(DB_FILE)).unwrap()
    }

    /// Store an event of this node as its producer would, without driving the producer.
    fn forge(&self, kind: &str, payload: Value) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("forged-{n:04}");
        self.sql()
            .execute(
                "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
                 VALUES (?1, ?2, ?3, 'system', ?4, ?5)",
                rusqlite::params![id, self.store.node_id(), T0, kind, payload.to_string()],
            )
            .unwrap();
        id
    }

    fn feed(&self, scope: Option<&TokenScope>) -> Vec<ActivityItem> {
        self.store
            .activity_scoped(
                ActivityQuery {
                    since: None,
                    limit: 500,
                    kinds: vec![ActivityKind::Hold],
                },
                scope,
            )
            .unwrap()
            .items
    }

    fn board(&self, project: &Path, scope: Option<TokenScope>) -> Board {
        self.store
            .board(BoardQuery {
                project: Some(path(project)),
                confined: true,
                scope,
                ..Default::default()
            })
            .unwrap()
    }
}

fn ids(holds: &[Hold]) -> Vec<String> {
    let mut ids: Vec<String> = holds.iter().map(|h| h.hold_id.clone()).collect();
    ids.sort();
    ids
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn board_hold_ids(b: &Board) -> Vec<String> {
    sorted(
        b.projects
            .iter()
            .flat_map(|p| p.holds.iter().map(|h| h.hold_id.clone()))
            .collect(),
    )
}

fn text(items: &[ActivityItem]) -> String {
    serde_json::to_string(items).unwrap()
}

/// The prompt's first case, and a sibling checkout: a parent project's whole-tree hold
/// reaches a token allowed only on a subdirectory project; a parent hold elsewhere and a
/// hold in a sibling checkout (same repository, outside the globs) do not.
#[test]
fn a_parent_whole_tree_hold_reaches_a_subdirectory_token_and_a_sibling_does_not() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let docs = fx.hold("alice@shop", &fx.shop, "docs/**", false);
    let sibling = fx.hold("wanda@shop-wt", &fx.wt, "sub/a.rs", false);
    let own = fx.hold("bob@sub", &fx.sub, "README", false);
    let token = fx.sub_only();

    // The hold list.
    let filter = || HoldFilter {
        project: Some(path(&fx.sub)),
        agent: None,
    };
    let listed = fx.store.holds_scoped(filter(), Some(&token)).unwrap();
    assert_eq!(ids(&listed), sorted(vec![star.clone(), own.clone()]));

    // The feed and the summary.
    let feed = fx.feed(Some(&token));
    let all = text(&feed);
    assert!(all.contains(&star) && all.contains(&own), "{all}");
    for hidden in [&docs, &sibling] {
        assert!(!all.contains(hidden.as_str()), "{all}");
    }
    assert!(!all.contains("docs/**") && !all.contains("wanda"), "{all}");
    let sum = fx
        .store
        .activity_summary_scoped(None, Some(&token))
        .unwrap();
    assert_eq!(sum.live_holds, 2);
    assert_eq!(sum.counts.get("hold"), Some(&2));
    assert!(sum.text.contains("2 live hold(s)"), "{}", sum.text);
    assert!(!sum.text.contains("docs/**"), "{}", sum.text);
    let new = fx
        .store
        .whats_new_scoped("bob@sub", None, 20, Some(&token))
        .unwrap();
    let new_text = serde_json::to_string(&new.activity).unwrap();
    assert!(new_text.contains(&star) && !new_text.contains(&docs), "{new_text}");

    // The confined board shows the parent's hold in the parent's own section.
    let b = fx.board(&fx.sub, Some(token.clone()));
    assert_eq!(board_hold_ids(&b), sorted(vec![star.clone(), own.clone()]));
    let mut slugs: Vec<&str> = b.projects.iter().map(|p| p.project.as_str()).collect();
    slugs.sort_unstable();
    assert_eq!(slugs, ["shop", "sub"], "no section for shop-wt");
    assert!(
        b.agents.iter().all(|a| a.address == "bob@sub"),
        "participants stay the named project's: {:?}",
        b.agents
    );

    // The sibling is shown once the token's globs allow it: the rule, not the repository.
    let both = scope(&[path(&fx.sub), path(&fx.wt)]);
    let listed = fx.store.holds_scoped(filter(), Some(&both)).unwrap();
    assert_eq!(ids(&listed), sorted(vec![star, own, sibling]));
}

/// Local callers are unchanged: the list includes the sibling checkout (D92), the feed and
/// summary count every hold, the unscoped confined board is today's, and conflicts name the
/// holder with no `hidden` field.
#[test]
fn local_callers_see_what_they_saw_before() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let docs = fx.hold("alice@shop", &fx.shop, "docs/**", false);
    let sibling = fx.hold("wanda@shop-wt", &fx.wt, "sub/a.rs", false);
    let listed = fx
        .store
        .holds(HoldFilter {
            project: Some(path(&fx.sub)),
            agent: None,
        })
        .unwrap();
    assert_eq!(
        ids(&listed),
        sorted(vec![star.clone(), docs.clone(), sibling.clone()])
    );
    let all = text(&fx.feed(None));
    for id in [&star, &docs, &sibling] {
        assert!(all.contains(id.as_str()), "{all}");
    }
    let sum = fx.store.activity_summary(None).unwrap();
    assert_eq!(sum.live_holds, 3);
    assert_eq!(sum.counts.get("hold"), Some(&3));
    // A confined board without a token keeps its old view: the named project only.
    let b = fx.board(&fx.sub, None);
    assert!(board_hold_ids(&b).is_empty(), "{b:?}");
    let conflicts = fx
        .store
        .check_holds("bob@sub", &path(&fx.sub), &["a.rs".to_string()], true)
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].held.agent, "alice@shop");
    assert!(conflicts[0].hidden.is_none());
    let json = serde_json::to_value(&conflicts[0]).unwrap();
    assert!(json.get("hidden").is_none(), "{json}");
}

/// One checkout under two spellings (`/tmp` and `/private/tmp` on macOS): compared from the
/// top of the checkout, whatever path each project was registered under.
#[cfg(unix)]
#[test]
fn symlinked_spellings_of_one_checkout_still_compare() {
    let fx = Fx::at(Some("spelled"));
    assert!(path(&fx.sub).contains("spelled"), "{:?}", fx.sub);
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let docs = fx.hold("alice@shop", &fx.shop, "docs/**", false);
    let sibling = fx.hold("wanda@shop-wt", &fx.wt, "sub/a.rs", false);
    let token = scope(&[path(&fx.sub)]);
    let listed = fx
        .store
        .holds_scoped(
            HoldFilter {
                project: Some(path(&fx.sub)),
                agent: None,
            },
            Some(&token),
        )
        .unwrap();
    assert_eq!(ids(&listed), vec![star.clone()]);
    let all = text(&fx.feed(Some(&token)));
    assert!(all.contains(&star) && !all.contains(&docs) && !all.contains(&sibling));
}

/// On a volume that ignores letter case, `SUB/a.rs` is a file of `sub`; elsewhere it is not.
#[test]
fn letter_case_follows_the_volume() {
    let fx = Fx::new();
    let upper = fx.hold("alice@shop", &fx.shop, "SUB/a.rs", false);
    let folds = std::fs::metadata(fx.shop.join("SUB")).is_ok();
    let token = fx.sub_only();
    let listed = fx
        .store
        .holds_scoped(
            HoldFilter {
                project: Some(path(&fx.sub)),
                agent: None,
            },
            Some(&token),
        )
        .unwrap();
    assert_eq!(listed.iter().any(|h| h.hold_id == upper), folds);
    assert_eq!(text(&fx.feed(Some(&token))).contains(&upper), folds);
}

/// A project with no repository is judged by its own tree; nothing else reaches it.
#[test]
fn a_project_outside_any_repository_sees_its_own_holds() {
    let fx = Fx::new();
    let post = fx.hold("carol@blog", &fx.blog, "post.md", false);
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let token = scope(&[path(&fx.blog)]);
    let listed = fx
        .store
        .holds_scoped(
            HoldFilter {
                project: Some(path(&fx.blog)),
                agent: None,
            },
            Some(&token),
        )
        .unwrap();
    assert_eq!(ids(&listed), vec![post.clone()]);
    let all = text(&fx.feed(Some(&token)));
    assert!(all.contains(&post) && !all.contains(&star), "{all}");
}

/// Counts and pages: hidden events neither appear nor take a place in a page, and a release
/// of a shown and a hidden hold counts the shown one.
#[test]
fn feed_pages_and_summary_counts_cover_shown_holds_only() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    for n in 0..20 {
        fx.hold("alice@shop", &fx.shop, &format!("docs/{n}.md"), false);
    }
    let token = fx.sub_only();
    let page = fx
        .store
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(T0 - 1)),
                limit: 1,
                kinds: vec![ActivityKind::Hold],
            },
            Some(&token),
        )
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(text(&page.items).contains(&star));
    let rest = fx
        .store
        .activity_scoped(
            ActivityQuery {
                since: page.next_since.map(Since::After),
                limit: 50,
                kinds: vec![ActivityKind::Hold],
            },
            Some(&token),
        )
        .unwrap();
    assert!(rest.items.is_empty(), "{:?}", rest.items);

    let docs: Vec<String> = fx
        .store
        .holds(HoldFilter {
            project: Some(path(&fx.shop)),
            agent: None,
        })
        .unwrap()
        .into_iter()
        .filter(|h| h.pattern.starts_with("docs/"))
        .map(|h| h.hold_id)
        .collect();
    fx.store
        .release_holds("alice@shop", &[star.clone(), docs[0].clone()])
        .unwrap();
    let released: Vec<ActivityItem> = fx
        .feed(Some(&token))
        .into_iter()
        .filter(|i| i.kind == "hold.released")
        .collect();
    assert_eq!(released.len(), 1);
    assert!(released[0].summary.contains("released 1 hold(s)"), "{released:?}");
    assert_eq!(released[0].refs["hold_ids"], json!([star]));
    let local: Vec<ActivityItem> = fx
        .feed(None)
        .into_iter()
        .filter(|i| i.kind == "hold.released")
        .collect();
    assert!(local[0].summary.contains("released 2 hold(s)"), "{local:?}");

    let sum = fx
        .store
        .activity_summary_scoped(None, Some(&token))
        .unwrap();
    assert_eq!(sum.counts.get("hold"), Some(&2), "acquired and released");
    assert_eq!(sum.live_holds, 0);
    for h in &sum.highlights {
        assert!(!h.contains("docs/"), "{h}");
    }
    let local = fx.store.activity_summary(None).unwrap();
    assert_eq!(local.counts.get("hold"), Some(&22));
    assert_eq!(local.live_holds, 19);
}

/// Every hold event kind, written as its producer writes it: a path the token may see is
/// kept with hidden blockers and error text removed; a hidden path, an unknown `hold.*`
/// kind and a payload of the wrong shape are left out. Local callers see all of them.
#[test]
fn every_hold_event_kind_is_filtered_by_path_and_blocker() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let docs = fx.hold("alice@shop", &fx.shop, "docs/**", false);
    let shop = fx.project_id(&fx.shop);
    let token = fx.sub_only();
    let secret = "error naming /elsewhere/secret";
    let kinds = [
        "hold.waiting",
        "hold.wait.rechecked",
        "hold.available",
        "hold.recheck_failed",
        "hold.wait_failed",
        "hold.notify_failed",
        "hold.wait.finished",
    ];
    let mut shown = Vec::new();
    let mut hidden = Vec::new();
    for kind in kinds {
        let payload = |p: &str| {
            let mut v = json!({ "agent_id": "x", "project_id": shop, "path": p, "error": secret });
            if kind != "hold.notify_failed" && kind != "hold.wait.finished" {
                v["blockers"] = json!([star, docs]);
            }
            v
        };
        shown.push((kind, fx.forge(kind, payload("sub/[odd].rs"))));
        hidden.push(fx.forge(kind, payload("docs/a.md")));
    }
    let malformed = [
        fx.forge("hold.waiting", json!({ "project_id": shop })),
        fx.forge("hold.waiting", json!({ "project_id": shop, "path": 7 })),
        fx.forge(
            "hold.waiting",
            json!({ "project_id": shop, "path": "sub/a.rs", "blockers": "x" }),
        ),
        fx.forge("hold.acquired", json!({ "project_id": shop })),
        fx.forge("hold.renewed", json!({ "hold_ids": [1] })),
        fx.forge("hold.blocked", json!({ "project_id": shop, "conflicts": [7] })),
        fx.forge("hold.blocked", json!({ "project_id": shop })),
        fx.forge("hold.zzz", json!({ "project_id": shop, "path": "sub/a.rs" })),
        fx.forge("hold.zzz", json!("not an object")),
    ];

    let remote = fx.feed(Some(&token));
    let by_id = |id: &str| remote.iter().find(|i| i.event_id == id);
    for (kind, id) in &shown {
        let item = by_id(id).unwrap_or_else(|| panic!("{kind} with a shown path is kept"));
        let json = serde_json::to_string(item).unwrap();
        assert!(!json.contains(secret), "{kind}: {json}");
        assert!(!json.contains(&docs), "{kind}: {json}");
        if item.refs.get("blockers").is_some() {
            assert_eq!(item.refs["blockers"], json!([star]), "{kind}");
        }
    }
    for id in hidden.iter().chain(&malformed) {
        assert!(by_id(id).is_none(), "{id} is left out: {:?}", by_id(id));
    }

    // Locally nothing changes: every event, every blocker, the error text.
    let local = fx.feed(None);
    for id in shown.iter().map(|(_, id)| id).chain(&hidden).chain(&malformed) {
        assert!(local.iter().any(|i| &i.event_id == id), "{id}");
    }
    let waiting = local.iter().find(|i| i.event_id == shown[0].1).unwrap();
    let json = serde_json::to_string(waiting).unwrap();
    assert!(json.contains(&docs), "{json}");
    let failed = local.iter().find(|i| i.event_id == shown[3].1).unwrap();
    assert!(serde_json::to_string(failed).unwrap().contains(secret));
}

/// A stopped edit is judged per conflict: kept when any conflicting path is shown, with only
/// those conflicts, each naming its holder only when that hold is shown. Checked in the
/// feed, the summary's highlights and the confined board.
#[test]
fn a_stopped_edit_is_judged_per_conflict() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    let docs = fx.hold("alice@shop", &fx.shop, "docs/**", false);
    let shop = fx.project_id(&fx.shop);
    let conflict = |p: &str, hold: &str, pattern: &str| {
        json!({ "path": p, "holder": "alice@shop", "hold_id": hold, "pattern": pattern, "project": "shop" })
    };
    let blocked = |conflicts: Value| {
        json!({
            "agent_id": "x", "agent": "wanda@shop", "project_id": shop, "project": "shop",
            "repo": null, "paths": ["docs/a.md", "sub/b.rs"], "conflicts": conflicts,
        })
    };
    // A: the first conflict is outside the token's tree, a later one inside.
    let a = fx.forge(
        "hold.blocked",
        blocked(json!([conflict("docs/a.md", &docs, "docs/**"), conflict("sub/b.rs", &star, "**")])),
    );
    // B: the first is inside but its hold is hidden; a later one is outside.
    let b = fx.forge(
        "hold.blocked",
        blocked(json!([conflict("sub/b.rs", &docs, "docs/**"), conflict("docs/a.md", &star, "**")])),
    );
    // C: every conflict outside.
    let c = fx.forge(
        "hold.blocked",
        blocked(json!([conflict("docs/a.md", &docs, "docs/**")])),
    );
    let token = fx.sub_only();
    let feed = fx.feed(Some(&token));
    let item = |id: &str| feed.iter().find(|i| i.event_id == id).cloned();
    let a = item(&a).expect("A is kept");
    // `forge` writes the event with actor `system`.
    assert_eq!(a.summary, "system was stopped editing shop:sub/b.rs (held by alice@shop)");
    assert_eq!(a.refs["hold_id"], json!(star));
    let b = item(&b).expect("B is kept");
    assert_eq!(
        b.summary,
        "system was stopped editing shop:sub/b.rs (held by another agent in this checkout)"
    );
    assert_eq!(b.refs["hold_id"], Value::Null);
    assert!(item(&c).is_none());
    for i in [&a, &b] {
        let json = serde_json::to_string(i).unwrap();
        assert!(!json.contains("docs/") && !json.contains(&docs), "{json}");
    }
    let sum = fx
        .store
        .activity_summary_scoped(None, Some(&token))
        .unwrap();
    let highlights = sum.highlights.join("\n");
    assert!(!highlights.contains("docs/"), "{highlights}");
    assert!(highlights.contains("held by another agent in this checkout"), "{highlights}");

    let board = fx.board(&fx.sub, Some(token));
    let rows: Vec<&BoardBlocked> = board.projects.iter().flat_map(|p| &p.blocked).collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(row.path, "sub/b.rs");
    }
    let named: Vec<Option<&str>> = rows.iter().map(|r| r.holder.as_deref()).collect();
    assert!(named.contains(&Some("alice@shop")) && named.contains(&None), "{named:?}");
    let hidden = rows.iter().find(|r| r.holder.is_none()).unwrap();
    assert!(hidden.pattern.is_none() && hidden.holder_project.is_none());
}

/// The confined board's history cap counts only what the token may see: many hidden edits
/// newer than a shown one neither crowd it out nor report truncation.
#[test]
fn hidden_edits_do_not_fill_the_boards_scan_cap() {
    let fx = Fx::new();
    let shop = fx.project_id(&fx.shop);
    let edit = |pattern: &str| {
        json!({ "hold_id": "e", "agent_id": "x", "project_id": shop, "pattern": pattern,
                "exclusive": true, "auto": true })
    };
    fx.forge("hold.acquired", edit("sub/seen.rs"));
    let db = fx.sql();
    db.execute(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < ?1)
         INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         SELECT 'hidden-' || x, ?2, ?3, 'alice@shop', 'hold.acquired', ?4 FROM n",
        rusqlite::params![
            BOARD_EVENT_SCAN_CAP + 10,
            fx.store.node_id(),
            T0,
            edit("docs/hidden.md").to_string()
        ],
    )
    .unwrap();
    let b = fx.board(&fx.sub, Some(fx.sub_only()));
    assert!(!b.history_truncated);
    let edits: Vec<&str> = b
        .projects
        .iter()
        .flat_map(|p| p.edits.iter().map(|e| e.path.as_str()))
        .collect();
    assert_eq!(edits, ["sub/seen.rs"]);
    // Locally the same history fills the cap.
    let local = fx
        .store
        .board(BoardQuery {
            project: Some(path(&fx.shop)),
            ..Default::default()
        })
        .unwrap();
    assert!(local.history_truncated);
}

/// Refusals still refuse. A blocking hold the token may not see is reported with no names:
/// only the caller's own path and one sentence, for a readable pattern and an unreadable
/// one alike.
#[test]
fn a_hidden_blocker_still_refuses_and_names_nothing() {
    let fx = Fx::new();
    let star = fx.hold("alice@shop", &fx.shop, "**", true);
    // A token that may see only the sibling checkout: alice's hold in `shop` is hidden.
    let token = scope(&[path(&fx.wt)]);
    let check = |token: &TokenScope| {
        fx.store
            .check_holds_scoped(
                "bob@sub",
                &path(&fx.sub),
                &["a.rs".to_string()],
                true,
                Some(token),
            )
            .unwrap()
    };
    let assert_redacted = |conflicts: &[HoldConflict]| {
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        let c = &conflicts[0];
        assert_eq!(c.path, "a.rs");
        assert_eq!(c.hidden.as_deref(), Some(HIDDEN));
        assert!(c.unreadable.is_none());
        let h = &c.held;
        assert!(h.hold_id.is_empty() && h.agent.is_empty() && h.project.is_empty());
        assert!(h.pattern.is_empty() && h.reason.is_none() && h.expires_at.is_empty());
        let json = serde_json::to_string(c).unwrap();
        for leak in ["alice", "shop", "**", "alice@shop reason", star.as_str()] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
    };
    assert_redacted(&check(&token));
    let got = fx
        .store
        .acquire_holds_scoped(
            AcquireHolds {
                agent: "bob@sub".into(),
                project: path(&fx.sub),
                paths: vec!["a.rs".into()],
                ttl_seconds: None,
                exclusive: true,
                reason: None,
                repo_wide: false,
            },
            Some(&token),
        )
        .unwrap();
    assert!(got.granted.is_empty(), "still refused: {got:?}");
    assert_redacted(&got.conflicts);

    // An unreadable stored pattern counts as all of its project, and its note (node, agent,
    // release command) is dropped with the rest.
    fx.sql()
        .execute(
            "UPDATE holds SET pattern = 'src/{a' WHERE hold_id = ?1",
            [&star],
        )
        .unwrap();
    assert_redacted(&check(&token));
    // A token that may see it gets the full conflict and its note.
    let shown = check(&fx.sub_only());
    assert_eq!(shown.len(), 1);
    assert!(shown[0].hidden.is_none() && shown[0].unreadable.is_some());
    assert_eq!(shown[0].held.agent, "alice@shop");
}
