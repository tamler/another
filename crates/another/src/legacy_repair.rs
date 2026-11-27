//! Explicit operator import/repair of pre-contract records. Never selected by an
//! installed hook, never a fallback, and never authority over a minted actor.

use std::path::Path;

use another_core::{Program, RegisterAgent, SessionInfo, SessionPhase, Store};
use anyhow::Result;
use serde_json::json;

use crate::hooks::{self, Client, HookEnv, HookEvent, HookInput};

pub(crate) fn event(
    store: &Store,
    env: &HookEnv,
    client: Client,
    event: HookEvent,
    input: &HookInput,
) -> Result<String> {
    let Some(client) = hooks::effective_client(client, env) else {
        return Ok(String::new());
    };
    let cwd = input.cwd.clone().unwrap_or(std::env::current_dir()?);
    let root = hooks::project_root(&cwd);
    let project = store.ensure_project("operator", &root.to_string_lossy())?;
    let agent = std::env::var("ANOTHER_AGENT")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| format!("{}@{}", client.as_str(), project.slug));
    if store.agent(&agent).is_err() {
        store.register_agent(RegisterAgent {
            name: client.as_str().into(),
            program: match client {
                Client::Claude => Program::Claude,
                Client::Codex => Program::Codex,
                Client::Grok => Program::Grok,
                Client::Cursor => Program::Cursor,
            },
            project: Some(project.abs_path),
            model: None,
            description: None,
        })?;
    }
    let session = input.session_id.as_deref();
    let info = SessionInfo {
        client: Some(client.as_str().into()),
        session_id: input.session_id.clone(),
        keep_resources: false,
    };
    if event == HookEvent::SessionEnd {
        store.session(&agent, SessionPhase::End, info)?;
        crate::wake::release_session(store, &agent, session);
        return Ok(String::new());
    }
    if event == HookEvent::SessionStart || event == HookEvent::Prompt {
        store.session(
            &agent,
            if event == HookEvent::SessionStart {
                SessionPhase::Start
            } else {
                SessionPhase::Beat
            },
            info,
        )?;
    } else if event == HookEvent::PostTool {
        store.keepalive(&agent, &info)?;
    }
    crate::wake::stamp_session(store, &agent, session, client.as_str(), &cwd);
    if event == HookEvent::PreEdit {
        let projects = store.projects()?;
        let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
        let mut conflicts = Vec::new();
        let mut first = false;
        let mut wait = true;
        let mut retry = 0;
        for raw in input.edit_paths() {
            for target in hooks::edit_targets(&cwd.join(&raw)) {
                if target.is_dir() || raw.ends_with('/') {
                    continue;
                }
                let registered = projects
                    .iter()
                    .filter(|p| target.starts_with(hooks::resolve_existing(Path::new(&p.abs_path))))
                    .max_by_key(|p| p.abs_path.len());
                // The file's own entry never counts, only the directories above it.
                let git = another_core::git_root(&target, false);
                let base = match (registered, git) {
                    (Some(p), Some(g))
                        if hooks::resolve_existing(Path::new(&p.abs_path))
                            .components()
                            .count()
                            > g.components().count() =>
                    {
                        hooks::resolve_existing(Path::new(&p.abs_path))
                    }
                    (_, Some(g)) => g,
                    (Some(p), None) => hooks::resolve_existing(Path::new(&p.abs_path)),
                    (None, None) => continue,
                };
                let key = base.to_string_lossy();
                store.ensure_project("operator", &key)?;
                let rel = target.strip_prefix(&base)?.to_string_lossy().into_owned();
                groups.entry(key.into_owned()).or_default().push(rel);
            }
        }
        let mut blocked = Vec::new();
        for (key, paths) in &groups {
            if !store.check_edit_paths(&agent, key, paths)?.is_empty() {
                blocked.push(key.clone());
            }
        }
        for (key, paths) in groups {
            if blocked.is_empty() || blocked.contains(&key) {
                let guard = store.guard_edit(&agent, &key, &paths, hooks::AUTO_HOLD_REASON)?;
                if !guard.conflicts.is_empty() {
                    first |= guard.first_notice;
                    wait &= guard.wait_recorded;
                    retry = retry.max(guard.retry_after_seconds);
                    conflicts.extend(guard.conflicts);
                }
            }
        }
        if conflicts.is_empty() {
            return Ok(String::new());
        }
        let reason = if first {
            hooks::deny_message(&agent, &conflicts, wait, retry)
        } else {
            format!("Another: this edit is still blocked. {} is held by {}. Back off for {retry} seconds. {}", conflicts[0].path, conflicts[0].held.agent, if wait { "Another will ping when the holds clear." } else { "no inbox ping will come. Retry after 30 seconds, or work on other files." })
        };
        return Ok(crate::native_hooks::deny(client, &reason));
    }
    if client == Client::Cursor {
        return Ok(if event == HookEvent::Prompt {
            json!({"continue":true}).to_string()
        } else {
            String::new()
        });
    }
    // These are requested operator views, not routine native hook injections.
    if event == HookEvent::SessionStart {
        let digest = hooks::session_digest(store, &agent, client, &[])?;
        let _ = hooks::prompt_notice(store, &agent, "session_start", client);
        return Ok(digest);
    }
    if input.agent_id.is_some() {
        return Ok(String::new());
    }
    let notice = hooks::prompt_notice(
        store,
        &agent,
        if event == HookEvent::PostTool {
            "post_tool"
        } else {
            "prompt"
        },
        client,
    )?;
    Ok(match notice {
        Some(notice) if event == HookEvent::PostTool => json!({"hookSpecificOutput":{"hookEventName":"PostToolUse", "additionalContext":notice.text}}).to_string(),
        Some(notice) => notice.text,
        None => String::new(),
    })
}
