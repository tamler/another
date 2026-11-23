use another_core::{
    cursor_task_child, map_native_actor, EditOutcome, NativeProfile, CURSOR_COVERAGE,
};
use serde_json::json;

#[test]
fn stock_claude_root_without_child_indicators_is_a_root() {
    let root = map_native_actor(
        NativeProfile::Claude21330,
        &json!({"session_id":"root", "agent_type":"custom-root", "hook_event_name":"PreToolUse"}),
        None,
    )
    .unwrap();
    assert_eq!(root.root_session, "root");
    assert_eq!(root.child_id, None);
    assert!(!root.shell_proof_available);
    for payload in [
        json!({"session_id":"root", "is_subagent":true}),
        json!({"session_id":"root", "transcript_path":"/root/subagents/child.jsonl"}),
        json!({"session_id":"root", "agent_id":"other", "transcript_path":"/root/subagents/child.jsonl"}),
    ] {
        assert!(map_native_actor(NativeProfile::Claude21330, &payload, None).is_err());
    }
}

#[test]
fn claude_and_codex_children_share_the_root_session_but_not_the_actor() {
    for profile in [NativeProfile::Claude21330, NativeProfile::Codex0171] {
        let child = map_native_actor(
            profile,
            &json!({"session_id":"root", "agent_id":"child", "tool_use_id":"call"}),
            None,
        )
        .unwrap();
        assert_eq!(child.root_session, "root");
        assert_eq!(child.child_id.as_deref(), Some("child"));
        assert_eq!(child.call_id.as_deref(), Some("call"));
        assert!(!child.shell_proof_available);
    }
}

#[test]
fn cursor_live_root_child_and_task_corroboration() {
    let profile = NativeProfile::Cursor20261122;
    let root = map_native_actor(
        profile,
        &json!({"session_id":"root", "conversation_id":"root", "hook_event_name":"sessionStart"}),
        None,
    )
    .unwrap();
    assert_eq!(root.child_id, None);
    let child = map_native_actor(
        profile,
        &json!({
            "session_id":"child", "conversation_id":"child", "generation_id":"child",
            "transcript_path":"/agent-transcripts/root/subagents/child.jsonl",
            "tool_use_id":"first\nsecond", "hook_event_name":"preToolUse",
        }),
        None,
    )
    .unwrap();
    assert_eq!(child.root_session, "root");
    assert_eq!(child.child_id.as_deref(), Some("child"));
    assert_eq!(child.call_id.as_deref(), Some("first\nsecond"));
    assert!(!child.shell_proof_available);
    let task =
        json!({"tool_name":"Task", "tool_output":"{\"status\":\"success\",\"agentId\":\"child\"}"});
    assert_eq!(cursor_task_child(&task).as_deref(), Some("child"));
    assert!(CURSOR_COVERAGE.contains("NO_ACTOR"));
    assert!(CURSOR_COVERAGE.contains("unguarded"));
    assert!(map_native_actor(
        profile,
        &json!({"session_id":"child", "conversation_id":"other"}),
        Some("root")
    )
    .is_err());
}

#[test]
fn cursor_permission_denial_and_interrupt_do_not_imply_executed_edit() {
    let profile = NativeProfile::Cursor20261122;
    let deny = map_native_actor(profile, &json!({"session_id":"root", "hook_event_name":"postToolUseFailure", "failure_type":"permission_denied", "is_interrupt":false}), None).unwrap();
    assert_eq!(deny.outcome, Some(EditOutcome::PermissionDenied));
    let end = map_native_actor(
        profile,
        &json!({"session_id":"root", "hook_event_name":"sessionEnd", "reason":"error"}),
        None,
    )
    .unwrap();
    assert!(end.session_end);
    let late = map_native_actor(profile, &json!({"session_id":"root", "hook_event_name":"postToolUseFailure", "failure_type":"error", "is_interrupt":false, "hook_ppid":12345}), None).unwrap();
    assert_eq!(late.outcome, Some(EditOutcome::Unknown));
    assert!(!late.shell_proof_available);
}

#[test]
fn grok_remapped_child_requires_adapter_owned_parent_binding() {
    let child = map_native_actor(
        NativeProfile::Grok1092,
        &json!({"sessionId":"child", "hook_event_name":"SessionEnd"}),
        Some("root"),
    )
    .unwrap();
    assert_eq!(child.root_session, "root");
    assert_eq!(child.child_id.as_deref(), Some("child"));
    assert!(child.session_end);
    assert!(map_native_actor(
        NativeProfile::Grok1092,
        &json!({"sessionId":"child", "subagentId":"child"}),
        None
    )
    .is_err());
}

#[test]
fn codex_failed_execution_is_not_reported_as_success() {
    let binding = map_native_actor(NativeProfile::Codex0171, &json!({"session_id":"root", "hook_event_name":"PostToolUse", "tool_response":{"exit_code":1}}), None).unwrap();
    assert_eq!(binding.outcome, Some(EditOutcome::FailedExecution));
}
