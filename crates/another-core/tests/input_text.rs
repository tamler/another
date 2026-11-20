//! Free text that is printed on a terminal is refused at the door when it has a control
//! or bidi character, as subjects are (D75): hold reasons, agent models and descriptions,
//! resource names, endpoints and purposes. FINDINGS-main-r2 T2/T3 and FINDINGS-161. Held
//! paths name real files and are kept as given.

use another_core::*;

/// OSC 52 (write the clipboard) then clear-screen; `{:?}` escapes it in test logs.
const EVIL: &str = "fine\u{1b}]52;c;ZXZpbA==\u{7}\u{1b}[2J";
/// A right-to-left override.
const BIDI: &str = "fine\u{202e}gpj.rs";

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    project: String,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    let project = dir.path().join("shop");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_string_lossy().into_owned();
    store.ensure_project("shop", &project).unwrap();
    let fx = Fx {
        _dir: dir,
        store,
        project,
    };
    register(&fx, None, None).unwrap();
    fx
}

fn register(fx: &Fx, model: Option<&str>, description: Option<&str>) -> Result<Agent> {
    fx.store.register_agent(RegisterAgent {
        name: "alice".into(),
        program: Program::Claude,
        project: Some(fx.project.clone()),
        model: model.map(Into::into),
        description: description.map(Into::into),
    })
}

fn hold(fx: &Fx, path: &str, reason: Option<&str>) -> Result<HoldReport> {
    fx.store.acquire_holds(AcquireHolds {
        agent: "alice".into(),
        project: fx.project.clone(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: reason.map(Into::into),
        repo_wide: false,
    })
}

fn resource(fx: &Fx, name: &str, endpoint: Option<&str>, purpose: Option<&str>) -> Result<()> {
    fx.store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::ModelServer,
            name: name.into(),
            endpoint: endpoint.map(Into::into),
            port: None,
            pid: None,
            project: None,
            purpose: purpose.map(Into::into),
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .map(|_| ())
}

fn refused(r: Result<impl std::fmt::Debug>, field: &str) {
    match r {
        Err(e) => {
            assert_eq!(e.code(), "invalid", "{e}");
            let msg = e.to_string();
            assert!(msg.contains(field), "{msg}");
            assert!(msg.contains("one line of text"), "{msg}");
            assert!(!msg.chars().any(is_unsafe_text_char), "{msg:?}");
        }
        Ok(v) => panic!("{field} with a control character was accepted: {v:?}"),
    }
}

#[test]
fn hold_reasons_with_control_characters_are_refused() {
    let fx = fx();
    refused(hold(&fx, "src/a.rs", Some(EVIL)), "a hold reason");
    refused(hold(&fx, "src/a.rs", Some(BIDI)), "a hold reason");
    // Nothing was granted on the way.
    let held = fx
        .store
        .holds(HoldFilter {
            project: None,
            agent: None,
        })
        .unwrap();
    assert!(held.is_empty(), "{held:?}");

    // Ordinary text, non-ASCII included, still works.
    let ok = hold(
        &fx,
        "src/café/naïve file.rs",
        Some("refactor: über-cleanup"),
    )
    .unwrap();
    assert_eq!(ok.granted.len(), 1);
}

/// A held path names a real file, and a file name may hold any of these (D75,
/// generated_subjects.rs): it is still held, so the edit stays coordinated. Printing it is
/// the output's job (D97, `another holds`) and the feed's (activity summaries are one line).
#[test]
fn a_held_path_with_control_characters_is_still_held_and_the_feed_line_is_clean() {
    let fx = fx();
    let odd = "src/x\u{1b}]0;title\u{7}.rs";
    let ok = hold(&fx, odd, None).unwrap();
    assert_eq!(ok.granted.len(), 1, "{ok:?}");
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: None,
            limit: 100,
            kinds: vec![],
        })
        .unwrap();
    let line = feed
        .items
        .iter()
        .find(|i| i.kind == "hold.acquired")
        .unwrap();
    assert!(
        !line.summary.chars().any(is_unsafe_text_char),
        "{:?}",
        line.summary
    );
    assert!(
        line.summary.contains("src/x ]0;title .rs"),
        "{:?}",
        line.summary
    );
}

#[test]
fn agent_models_and_descriptions_with_control_characters_are_refused() {
    let fx = fx();
    refused(register(&fx, Some(EVIL), None), "an agent model");
    refused(register(&fx, None, Some(EVIL)), "an agent description");
    refused(register(&fx, None, Some(BIDI)), "an agent description");
    let a = register(&fx, Some("opus 5.5"), Some("reviews the café module")).unwrap();
    assert_eq!(a.description.as_deref(), Some("reviews the café module"));
}

#[test]
fn resource_text_with_control_characters_is_refused() {
    let fx = fx();
    refused(
        resource(&fx, "ollama", Some(EVIL), None),
        "a resource endpoint",
    );
    refused(
        resource(&fx, "ollama", None, Some(EVIL)),
        "a resource purpose",
    );
    // Names already refused C0/C1 controls; bidi overrides now too.
    let err = resource(&fx, BIDI, None, None).unwrap_err();
    assert_eq!(err.code(), "invalid");
    resource(
        &fx,
        "ollama@11434",
        Some("http://127.0.0.1:11434"),
        Some("llama3 für Tests"),
    )
    .unwrap();
}
