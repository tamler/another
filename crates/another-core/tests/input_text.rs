//! Free text that is printed on a terminal is refused at the door when it has a control
//! or bidi character, as subjects are (D75): hold reasons, agent models and descriptions,
//! resource names, endpoints and purposes. FINDINGS-main-r2 T2/T3 and FINDINGS-161. Held
//! paths name real files and are kept as given.

use another_core::*;

/// OSC 52 (write the clipboard) then clear-screen; `{:?}` escapes it in test logs.
const EVIL: &str = "fine\u{1b}]52;c;ZXZpbA==\u{7}\u{1b}[2J";
/// A right-to-left override.
const BIDI: &str = "fine\u{202e}gpj.rs";

#[test]
fn unicode_line_separators_are_flattened() {
    for separator in ['\u{2028}', '\u{2029}', '\u{85}'] {
        let text = format!("first{separator}second");
        assert_eq!(single_line(&text, 100), "first second");
        assert!(is_unsafe_text_char(separator));
    }
}

#[test]
fn subjects_with_unicode_line_separators_are_refused() {
    let fx = fx();
    for separator in ['\u{2028}', '\u{2029}', '\u{85}'] {
        let result = fx.store.send_message(SendMessage {
            from: "alice".into(),
            to: vec!["alice".into()],
            subject: format!("first{separator}second"),
            body: "body".into(),
            ..Default::default()
        });
        assert_eq!(result.unwrap_err().code(), "invalid");
    }
}

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

/// What a refusal says it found.
const CONTROL: &str = "control characters";
const DIRECTION: &str = "text-direction character";

/// The refusal names the field and what it found there.
fn refused(r: Result<impl std::fmt::Debug>, field: &str, found: &str) {
    match r {
        Err(e) => {
            assert_eq!(e.code(), "invalid", "{e}");
            let msg = e.to_string();
            assert!(msg.contains(field), "{msg}");
            assert!(msg.contains(found), "{msg}");
            let other = if found == CONTROL { DIRECTION } else { CONTROL };
            assert!(!msg.contains(other), "{msg}");
            assert!(!msg.chars().any(is_unsafe_text_char), "{msg:?}");
        }
        Ok(v) => panic!("{field} with a control character was accepted: {v:?}"),
    }
}

#[test]
fn hold_reasons_with_control_characters_are_refused() {
    let fx = fx();
    refused(hold(&fx, "src/a.rs", Some(EVIL)), "a hold reason", CONTROL);
    refused(
        hold(&fx, "src/a.rs", Some(BIDI)),
        "a hold reason",
        DIRECTION,
    );
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

/// Q19b review R1: a summary is read by a model too (`whats_new`). A zero-width space
/// cannot be seen, and a Unicode tag character is read by a model as a letter no person
/// sees; both pass `single_line`, so the summary is made a shown line (`display_line`).
#[test]
fn a_feed_line_shows_no_zero_width_or_tag_characters() {
    let fx = fx();
    let odd = "src/a\u{200b}b\u{e0041}\u{e0042}c.rs";
    let ok = hold(&fx, odd, None).unwrap();
    assert_eq!(ok.granted.len(), 1, "{ok:?}");
    assert_eq!(ok.granted[0].pattern, odd, "the held path is kept as given");
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
    assert!(line.summary.contains("src/a b  c.rs"), "{:?}", line.summary);
    assert!(
        !line
            .summary
            .chars()
            .any(|c| matches!(c, '\u{200b}' | '\u{e0000}'..='\u{e007f}')),
        "{:?}",
        line.summary
    );
}

#[test]
fn agent_models_and_descriptions_with_control_characters_are_refused() {
    let fx = fx();
    refused(register(&fx, Some(EVIL), None), "an agent model", CONTROL);
    refused(
        register(&fx, None, Some(EVIL)),
        "an agent description",
        CONTROL,
    );
    refused(
        register(&fx, None, Some(BIDI)),
        "an agent description",
        DIRECTION,
    );
    let a = register(&fx, Some("opus 5.5"), Some("reviews the café module")).unwrap();
    assert_eq!(a.description.as_deref(), Some("reviews the café module"));
}

#[test]
fn resource_text_with_control_characters_is_refused() {
    let fx = fx();
    refused(
        resource(&fx, "ollama", Some(EVIL), None),
        "a resource endpoint",
        CONTROL,
    );
    refused(
        resource(&fx, "ollama", None, Some(EVIL)),
        "a resource purpose",
        CONTROL,
    );
    // Names already refused C0/C1 controls; bidi overrides now too.
    refused(resource(&fx, EVIL, None, None), "a resource name", CONTROL);
    refused(
        resource(&fx, BIDI, None, None),
        "a resource name",
        DIRECTION,
    );
    resource(
        &fx,
        "ollama@11434",
        Some("http://127.0.0.1:11434"),
        Some("llama3 für Tests"),
    )
    .unwrap();
}
