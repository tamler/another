//! A remote connection acts as its token's agent and no one else (`remote::authorize`,
//! `acting_agent`). The refusal is covered end to end in remote.rs for two tools; this pins
//! the rule itself across every tool that names an acting agent: an omitted agent is filled
//! in, the token's own agent may be named in any spelling that resolves to it, and any other
//! agent, including a same-named one in another project or an ambiguous name, is refused.

use another::remote::authorize;
use another_core::{Agent, Program, RegisterAgent, Store, TokenScope};
use serde_json::{json, Map, Value};

/// Tools whose acting agent is checked, and the argument that names it.
const ACTING: &[(&str, &str)] = &[
    ("send_message", "from"),
    ("fetch_inbox", "agent"),
    ("acknowledge", "agent"),
    ("claim_request", "agent"),
    ("heartbeat", "agent"),
    ("resource_borrow", "agent"),
    ("resource_release", "agent"),
    ("resource_heartbeat", "agent"),
    ("whats_new", "agent"),
    ("hold_paths", "agent"),
    ("resource_register", "agent"),
    ("threads", "agent"),
];

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    shop: String,
    me: Agent,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    let mut projects = Vec::new();
    for name in ["shop", "other"] {
        let p = dir.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        projects.push(p.to_string_lossy().into_owned());
    }
    let register = |name: &str, project: &str| {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: Some(project.into()),
                model: None,
                description: None,
            })
            .unwrap()
    };
    let me = register("bot", &projects[0]);
    // Same name in another project, and an unrelated agent.
    register("bot", &projects[1]);
    register("alice", &projects[0]);
    Fx {
        _dir: dir,
        store,
        shop: projects[0].clone(),
        me,
    }
}

fn scope(fx: &Fx) -> TokenScope {
    TokenScope {
        token_id: "tok".into(),
        agent_id: fx.me.agent_id.clone(),
        agent: fx.me.address.clone(),
        project_globs: vec![format!("{}/**", fx.shop), fx.shop.clone()],
    }
}

/// Arguments for `tool` that pass every other check, with the acting field set to `who`
/// (or left out).
fn args(fx: &Fx, tool: &str, field: &str, who: Option<Value>) -> Map<String, Value> {
    let mut a = match tool {
        "hold_paths" => json!({ "project": fx.shop, "paths": ["src/a.rs"] }),
        "resource_register" => json!({ "project": fx.shop }),
        _ => json!({}),
    }
    .as_object()
    .unwrap()
    .clone();
    if let Some(who) = who {
        a.insert(field.into(), who);
    }
    a
}

#[test]
fn an_omitted_agent_is_the_tokens_own() {
    let fx = fx();
    for (tool, field) in ACTING {
        let mut a = args(&fx, tool, field, None);
        authorize(&fx.store, &scope(&fx), tool, &mut a).unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert_eq!(a[*field], json!(fx.me.address), "{tool}");
    }
}

#[test]
fn the_tokens_own_agent_may_be_named_in_full() {
    let fx = fx();
    // The token's agent is `bot@shop`; its full address resolves to it.
    assert_eq!(fx.me.address, "bot@shop");
    for (tool, field) in ACTING {
        let mut a = args(&fx, tool, field, Some(json!("bot@shop")));
        authorize(&fx.store, &scope(&fx), tool, &mut a).unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert_eq!(a[*field], json!("bot@shop"), "{tool}: left as given");
    }
}

#[test]
fn any_other_agent_is_refused_including_a_namesake() {
    let fx = fx();
    // alice is someone else; bot@other shares the name; bare "bot" is ambiguous between
    // the two bots, so it is not taken to mean the token's own; "nobody" does not exist.
    for who in ["alice@shop", "alice", "bot@other", "bot", "nobody", ""] {
        for (tool, field) in ACTING {
            let mut a = args(&fx, tool, field, Some(json!(who)));
            let err = authorize(&fx.store, &scope(&fx), tool, &mut a)
                .expect_err(&format!("{tool} acted as {who:?}"));
            assert!(
                err.contains("this connection acts as bot@shop"),
                "{tool} as {who:?}: {err}"
            );
        }
    }
}

/// A value that is not a string names no one: the token's own agent is put in its place,
/// so the call can only ever act as the token's agent.
#[test]
fn a_non_string_agent_becomes_the_tokens_own() {
    let fx = fx();
    for odd in [
        json!(5),
        json!(["alice"]),
        json!({ "name": "alice" }),
        Value::Null,
    ] {
        for (tool, field) in ACTING {
            let mut a = args(&fx, tool, field, Some(odd.clone()));
            authorize(&fx.store, &scope(&fx), tool, &mut a)
                .unwrap_or_else(|e| panic!("{tool} with {odd}: {e}"));
            assert_eq!(a[*field], json!("bot@shop"), "{tool} with {odd}");
        }
    }
}

#[test]
fn a_tool_off_the_list_is_refused() {
    let fx = fx();
    for tool in [
        "resource_reap",
        "release_holds",
        "project_notes",
        "no_such_tool",
    ] {
        let mut a = Map::new();
        assert!(
            authorize(&fx.store, &scope(&fx), tool, &mut a).is_err(),
            "{tool} was allowed"
        );
    }
}
