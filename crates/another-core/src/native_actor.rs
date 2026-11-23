//! Validated native identity mapping. This parses evidence; it does not mint an
//! actor, prove shell ancestry, or authorize a mutation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EditOutcome, Error, Result};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeProfile {
    Claude21330,
    Codex0171,
    Grok1092,
    Cursor20261122,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeBinding {
    pub root_session: String,
    pub child_id: Option<String>,
    pub call_id: Option<String>,
    pub session_end: bool,
    pub outcome: Option<EditOutcome>,
    /// Payload correlation alone never proves which shell invocation is calling.
    pub shell_proof_available: bool,
}

pub const CURSOR_COVERAGE: &str = "unguarded; live 2026.11.22-e9042b2 mapping; CLI mutations NO_ACTOR until a tested caller-proof carriage exists";

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| v.get(key)?.as_str().filter(|s| !s.is_empty()))
}

fn missing() -> Error {
    Error::Forbidden("NO_ACTOR: native identity is missing or contradictory".into())
}

/// Extract a dependent-child transcript binding of the live form
/// `.../<parent>/subagents/<child>.jsonl`. Neither a remapped id nor a pid alone
/// establishes the parent or an independent lifetime.
fn transcript_child(path: &str) -> Option<(&str, &str)> {
    let (parent_path, leaf) = path.rsplit_once("/subagents/")?;
    let child = leaf.strip_suffix(".jsonl")?;
    let parent = parent_path.rsplit('/').next()?;
    (!parent.is_empty() && !child.is_empty() && !child.contains('/')).then_some((parent, child))
}

/// `known_parent` is adapter-owned corroboration from a previously observed Task
/// result or child-start binding, never a model-selected parent name.
pub fn map_native_actor(
    profile: NativeProfile,
    payload: &Value,
    known_parent: Option<&str>,
) -> Result<NativeBinding> {
    let session =
        field(payload, &["session_id", "sessionId", "conversation_id"]).ok_or_else(missing)?;
    let event = field(payload, &["hook_event_name", "event", "eventName"]).unwrap_or("");
    let transcript = field(payload, &["transcript_path", "transcriptPath"]);
    let transcript_binding = transcript.and_then(transcript_child);
    let explicit_child = field(payload, &["agent_id", "subagentId", "subagent_id"]);
    let child_marker = payload.get("is_subagent").and_then(Value::as_bool) == Some(true)
        || payload.get("parent_agent_id").is_some_and(|v| !v.is_null())
        || transcript_binding.is_some();
    let (root_session, child_id) = match profile {
        NativeProfile::Claude21330 | NativeProfile::Codex0171 => {
            if let Some(child) = explicit_child {
                if transcript_binding
                    .is_some_and(|(_, t)| t != child && t != format!("agent-{child}"))
                {
                    return Err(missing());
                }
                (session.to_owned(), Some(child.to_owned()))
            } else if child_marker {
                return Err(missing());
            } else {
                // A registered stock root with no child indicators is a root. No
                // claim is made about a child byte-identical to that valid root.
                (session.to_owned(), None)
            }
        }
        NativeProfile::Cursor20261122 => {
            if let Some(conversation) = field(payload, &["conversation_id"]) {
                if conversation != session {
                    return Err(missing());
                }
            }
            if explicit_child.is_some() {
                return Err(missing());
            }
            if let Some((parent, child)) = transcript_binding {
                if child != session || known_parent.is_some_and(|p| p != parent) {
                    return Err(missing());
                }
                if field(payload, &["generation_id"]).is_some_and(|g| g != child) {
                    return Err(missing());
                }
                (parent.to_owned(), Some(child.to_owned()))
            } else if let Some(parent) = known_parent {
                if parent == session {
                    return Err(missing());
                }
                (parent.to_owned(), Some(session.to_owned()))
            } else if child_marker {
                return Err(missing());
            } else {
                (session.to_owned(), None)
            }
        }
        NativeProfile::Grok1092 => {
            if let Some(parent) = known_parent {
                (
                    parent.to_owned(),
                    Some(explicit_child.unwrap_or(session).to_owned()),
                )
            } else if explicit_child.is_some() || child_marker {
                return Err(missing());
            } else {
                (session.to_owned(), None)
            }
        }
    };
    let failure = matches!(event, "PostToolUseFailure" | "postToolUseFailure");
    let outcome = if failure {
        Some(
            if field(payload, &["failure_type"]) == Some("permission_denied") {
                EditOutcome::PermissionDenied
            } else {
                // is_interrupt was false in the actual Cursor SIGINT fixture. It
                // cannot turn an unknown or denied execution into renewal evidence.
                EditOutcome::Unknown
            },
        )
    } else if matches!(event, "PostToolUse" | "postToolUse") {
        let response = payload
            .get("tool_response")
            .or_else(|| payload.get("tool_output"));
        let failed = response.is_some_and(|r| {
            r.get("is_error").and_then(Value::as_bool) == Some(true)
                || r.get("success").and_then(Value::as_bool) == Some(false)
                || field(r, &["status"]).is_some_and(|s| matches!(s, "failed" | "error"))
                || r.get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0)
        });
        Some(if failed {
            EditOutcome::FailedExecution
        } else {
            EditOutcome::Success
        })
    } else {
        None
    };
    Ok(NativeBinding {
        root_session,
        child_id,
        call_id: field(payload, &["tool_use_id", "toolUseId", "tool_call_id"]).map(str::to_owned),
        session_end: matches!(event, "SessionEnd" | "sessionEnd"),
        outcome,
        shell_proof_available: false,
    })
}

/// Corroboration supplied by a parent Task result. The returned id is a native
/// binding hint, never an actor capability.
pub fn cursor_task_child(payload: &Value) -> Option<String> {
    if field(payload, &["tool_name"]) != Some("Task") {
        return None;
    }
    let output = payload.get("tool_output")?;
    let parsed;
    let output = if let Some(raw) = output.as_str() {
        parsed = serde_json::from_str::<Value>(raw).ok()?;
        &parsed
    } else {
        output
    };
    field(output, &["agentId"]).map(str::to_owned)
}
