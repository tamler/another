//! The read-only commit check (advisory pre-commit guard) sees the same holds as the edit
//! guard, including in sibling checkouts whose repository link was never cached or was
//! cached under older key rules, and writes nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use another_core::{
    AcquireHolds, CommitGroup, CommitReader, Program, RegisterAgent, Store, DB_FILE,
};
use rusqlite::Connection;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A repository `shop` and four linked worktrees, one agent in each.
struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    main: PathBuf,
    store: Store,
}

const SIBLINGS: [(&str, &str); 4] = [
    ("bob", "shop-current"),
    ("dave", "shop-uncached"),
    ("erin", "shop-obsolete"),
    ("finn", "shop-empty"),
];

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so registered paths match what the guard computes (macOS /private).
        // Not on Windows, where `canonicalize` returns a verbatim `\\?\C:\…` path that no
        // registered project uses; its temp paths are already plain there.
        let root = if cfg!(windows) {
            dir.path().to_path_buf()
        } else {
            std::fs::canonicalize(dir.path()).unwrap()
        };
        let main = root.join("shop");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        for (_, wt) in SIBLINGS {
            let wt_git = main.join(".git/worktrees").join(wt);
            write(
                &root.join(wt).join(".git"),
                &format!("gitdir: {}\n", wt_git.display()),
            );
            write(&wt_git.join("commondir"), "../..\n");
        }
        let home = root.join("home");
        let store = Store::open(&home).unwrap();
        let register = |name: &str, project: &Path| {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(path(project)),
                    model: None,
                    description: None,
                })
                .unwrap();
        };
        register("alice", &main);
        for (name, wt) in SIBLINGS {
            register(name, &root.join(wt));
        }
        let fx = Fx {
            _dir: dir,
            home,
            main,
            store,
        };
        fx.hold("alice", &fx.main, "src/own.rs", Some("mine"));
        for (i, (name, wt)) in SIBLINGS.iter().enumerate() {
            let dir = fx.main.parent().unwrap().join(wt);
            fx.hold(name, &dir, &format!("src/f{i}.rs"), None);
        }
        fx
    }

    fn hold(&self, agent: &str, project: &Path, file: &str, reason: Option<&str>) {
        let r = self
            .store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: path(project),
                paths: vec![file.into()],
                ttl_seconds: Some(3600),
                exclusive: true,
                reason: reason.map(str::to_string),
                repo_wide: false,
            })
            .unwrap();
        assert!(r.conflicts.is_empty());
    }

    fn db(&self) -> Connection {
        Connection::open(self.home.join(DB_FILE)).unwrap()
    }

    /// A worktree's project, by its slug (the directory name), which does not depend on how
    /// the platform spells the path.
    fn project_id(&self, wt: &str) -> String {
        self.db()
            .query_row(
                "SELECT project_id FROM projects WHERE slug = ?1",
                [wt],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Every row of every table, as text, in a stable order.
    fn dump(&self) -> Vec<String> {
        let db = self.db();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut out = Vec::new();
        for t in tables {
            let mut stmt = db.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |r| {
                    let mut cells = Vec::new();
                    for i in 0..n {
                        let v: rusqlite::types::Value = r.get(i)?;
                        cells.push(format!("{v:?}"));
                    }
                    Ok(cells.join("|"))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.sort();
            out.push(format!("{t}: {}", rows.join("\n")));
        }
        out
    }

    fn reader(&self) -> CommitReader {
        CommitReader::open(&self.home, Duration::from_millis(300)).unwrap()
    }

    fn files(&self) -> Vec<String> {
        let mut f: Vec<String> = (0..SIBLINGS.len())
            .map(|i| format!("src/f{i}.rs"))
            .collect();
        f.push("src/own.rs".into());
        f
    }

    fn group(&self) -> Vec<CommitGroup> {
        vec![CommitGroup {
            dir: path(&self.main),
            paths: self.files(),
        }]
    }
}

fn holders(conflicts: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut v: Vec<_> = conflicts.into_iter().collect();
    v.sort();
    v
}

#[test]
fn sibling_links_match_the_edit_guard_and_nothing_is_written() {
    let fx = Fx::new();
    // Let the edit guard's own refresh cache every link in the current format, then put
    // three siblings in the states a cache-only lookup gets wrong (review R1, P2).
    fx.store
        .check_holds("alice", &path(&fx.main), &["src/none.rs".into()], true)
        .unwrap();
    let db = fx.db();
    let uncached = fx.project_id("shop-uncached");
    db.execute(
        "DELETE FROM project_repos WHERE project_id = ?1",
        [&uncached],
    )
    .unwrap();
    let obsolete = fx.project_id("shop-obsolete");
    let n = db
        .execute(
            "UPDATE project_repos SET repo_key = substr(repo_key, instr(repo_key, ':') + 1)
             WHERE project_id = ?1 AND repo_key LIKE 'v%:%'",
            [&obsolete],
        )
        .unwrap();
    assert_eq!(n, 1, "the obsolete-key fixture was not written");
    let empty = fx.project_id("shop-empty");
    db.execute(
        "UPDATE project_repos SET repo_key = '', repo_prefix = '' WHERE project_id = ?1",
        [&empty],
    )
    .unwrap();
    drop(db);

    let before = fx.dump();
    let reader = fx.reader();
    assert!(reader.is_readonly());
    let check = reader
        .check(
            Some("alice"),
            &fx.group(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    drop(reader);
    assert_eq!(
        before,
        fx.dump(),
        "the read-only check wrote to the database"
    );

    assert!(check.links_complete);
    assert_eq!(check.agent.as_deref(), Some("alice@shop"));
    assert_eq!(check.checked, fx.files().len());
    let ours = holders(
        check
            .conflicts
            .iter()
            .map(|c| (c.path.clone(), c.held.agent.clone())),
    );
    // Current, never cached and obsolete siblings are found; the one cached as "not in a
    // repository" is missed, by the edit guard too (it trusts that row). Alice's own hold
    // is left out.
    assert_eq!(
        ours,
        [
            ("src/f0.rs".to_string(), "bob@shop-current".to_string()),
            ("src/f1.rs".to_string(), "dave@shop-uncached".to_string()),
            ("src/f2.rs".to_string(), "erin@shop-obsolete".to_string()),
        ]
    );

    // The edit guard's answer for the same state (it refreshes, so it runs second).
    let theirs = holders(
        fx.store
            .check_holds("alice", &path(&fx.main), &fx.files(), true)
            .unwrap()
            .into_iter()
            .map(|c| (c.path, c.held.agent)),
    );
    assert_eq!(ours, theirs);
}

#[test]
fn an_unknown_committer_sees_every_hold_including_its_own() {
    let fx = Fx::new();
    let reader = fx.reader();
    let deadline = Instant::now() + Duration::from_secs(5);
    let check = reader.check(None, &fx.group(), deadline).unwrap();
    assert!(check.agent.is_none());
    assert!(check.conflicts.iter().any(|c| c.held.agent == "alice@shop"));

    let check = reader.check(Some("nobody"), &fx.group(), deadline).unwrap();
    assert!(check.agent.is_none());
    assert!(check.agent_note.as_deref().unwrap().starts_with("nobody"));
    assert!(check.conflicts.iter().any(|c| c.held.agent == "alice@shop"));
}

#[test]
fn glob_names_and_unregistered_directories_are_skipped_not_matched() {
    let fx = Fx::new();
    let reader = fx.reader();
    let groups = vec![
        CommitGroup {
            dir: path(&fx.main),
            paths: vec!["src/f*.rs".into(), "src/[ab].rs".into()],
        },
        CommitGroup {
            dir: path(&fx.main.parent().unwrap().join("elsewhere")),
            paths: vec!["x.rs".into()],
        },
    ];
    let check = reader
        .check(
            Some("alice"),
            &groups,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    assert!(check.conflicts.is_empty(), "{:?}", check.conflicts);
    assert_eq!(check.checked, 0);
    let reasons: Vec<&str> = check.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert_eq!(
        reasons,
        [
            "glob character in the name",
            "glob character in the name",
            "not in a registered project"
        ]
    );
}

#[test]
fn a_spent_budget_is_reported_as_incomplete() {
    let fx = Fx::new();
    // No cached links, so every candidate needs recomputing, and no time to do it.
    fx.db().execute("DELETE FROM project_repos", []).unwrap();
    let check = fx
        .reader()
        .check(Some("alice"), &fx.group(), Instant::now())
        .unwrap();
    assert!(!check.links_complete);
}

/// Review IMPL-1: a budget that runs out during matching (not before it starts) stops the
/// work there, and the result says which paths were not checked.
#[test]
fn a_budget_that_runs_out_while_matching_stops_the_check() {
    let fx = Fx::new();
    let root = fx.main.parent().unwrap();
    // 100 glob holds in a sibling checkout: each staged path is compared with all of them.
    let r = fx
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: path(&root.join("shop-current")),
            paths: (0..100).map(|i| format!("d{i}/**/*.rs")).collect(),
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert!(r.conflicts.is_empty());
    let paths: Vec<String> = (0..4096)
        .map(|i| format!("d{}/x/f{i}.rs", i % 100))
        .collect();
    let groups = vec![CommitGroup {
        dir: path(&fx.main),
        paths: paths.clone(),
    }];
    let reader = fx.reader();
    let started = Instant::now();
    let check = reader
        .check(Some("alice"), &groups, started + Duration::from_millis(100))
        .unwrap();
    let took = started.elapsed();
    // Unbounded, this takes minutes. The slack covers a slow CI runner.
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert!(!check.links_complete);
    assert!(check.checked < paths.len(), "{}", check.checked);
    let budget = check
        .skipped
        .iter()
        .filter(|s| s.reason == "time budget")
        .count();
    assert!(budget > 0);
    assert_eq!(check.checked + check.skipped.len(), paths.len());
    // Conflicts found before the deadline are kept: every checked path is under a hold.
    assert!(check.conflicts.len() >= check.checked, "{check:?}");
    assert!(check
        .conflicts
        .iter()
        .all(|c| c.held.agent == "bob@shop-current"));
}

#[test]
fn open_refuses_a_missing_database_and_a_schema_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let err = CommitReader::open(dir.path(), Duration::from_millis(50))
        .err()
        .unwrap();
    assert!(matches!(err, another_core::Error::NotFound(_)), "{err}");

    let fx = Fx::new();
    let version: i64 = fx
        .db()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    fx.db()
        .pragma_update(None, "user_version", version + 1)
        .unwrap();
    let err = CommitReader::open(&fx.home, Duration::from_millis(50))
        .err()
        .unwrap();
    assert!(err.to_string().contains("schema"), "{err}");
}
