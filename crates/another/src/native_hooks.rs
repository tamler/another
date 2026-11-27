//! Reference native adapters. Private participant authority stays in Another's home;
//! native names and payload ids are correlation, never a replacement for that grant.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use another_core::{
    map_native_actor, EditOutcome, NativeProfile, ParticipantProof, RegisteredActor, Store,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::hooks::{Client, HookEnv, HookEvent, HookInput};

fn profile(client: Client) -> NativeProfile {
    match client {
        Client::Claude => NativeProfile::Claude21330,
        Client::Codex => NativeProfile::Codex0171,
        Client::Grok => NativeProfile::Grok1092,
        Client::Cursor => NativeProfile::Cursor20261122,
    }
}

fn private_dir(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("private adapter directory must not be a symlink");
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn enroll(store: &Store, home: &Path) -> Result<Value> {
    let instance = home
        .join("adapters/v1/custom")
        .join(another_core::crypto::request_nonce()?);
    private_dir(&instance)?;
    let path = instance.join("participant.json");
    let grant = participant(store, &path)?;
    Ok(
        json!({"contract":"another.agent/1", "participant_id":grant.participant_id, "private_grant_path":path}),
    )
}

/// Create once. A concurrent reader of an unfinished write refuses this event and
/// retries on a later hook; it never invents a second identity or overwrites proof.
///
/// Privacy check: on Unix a grant readable or writable by group or others is refused.
/// On Windows there is no mode bit to check and Another adds no ACL code: the grant
/// inherits the ACL of the Another home, which under the default per-user profile
/// (`%USERPROFILE%\.another`) only that user and administrators can read. A home
/// placed in a shared folder (`ANOTHER_HOME`) is not protected; that is a stated limit.
fn participant(store: &Store, path: &Path) -> Result<ParticipantProof> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            bail!("private adapter grant must not be a symlink");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                bail!("private adapter grant is not private");
            }
        }
    }
    if let Ok(raw) = fs::read(path) {
        return serde_json::from_slice(&raw)
            .map_err(|_| anyhow::anyhow!("invalid private adapter grant"));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            let proof = match store.enroll_participant() {
                Ok(proof) => proof,
                Err(error) => {
                    drop(file);
                    let _ = fs::remove_file(path);
                    return Err(error.into());
                }
            };
            file.write_all(&serde_json::to_vec(&proof)?)?;
            file.sync_all()?;
            Ok(proof)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            serde_json::from_slice(&fs::read(path)?)
                .map_err(|_| anyhow::anyhow!("private adapter grant is not ready"))
        }
        Err(e) => Err(e.into()),
    }
}

fn binding_key(client: Client, root: &str, child: Option<&str>) -> String {
    // Native ids can contain delimiters; encoding the tuple prevents aliasing.
    serde_json::to_string(&(client.as_str(), root, child)).expect("string tuple")
}

fn retain_actor(
    instance: &Path,
    key: &str,
    registered: RegisteredActor,
) -> Result<RegisteredActor> {
    let path = instance.join(format!(
        "{}.json",
        another_core::crypto::sha256_hex(key.as_bytes())
    ));
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            bail!("private native actor record must be a regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                bail!("private native actor record is not private");
            }
        }
    }
    if path.is_file() {
        // A callback without an epoch cannot borrow a newer run's proof.
        return serde_json::from_slice(&fs::read(path)?)
            .map_err(|_| anyhow::anyhow!("invalid private native actor record"));
    }
    if registered.actor.session_epoch != 1 {
        bail!("NO_CALL_CONTEXT: a native callback cannot infer a resumed epoch without private carriage");
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&serde_json::to_vec(&registered)?)?;
            file.sync_all()?;
            Ok(registered)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            serde_json::from_slice(&fs::read(path)?)
                .map_err(|_| anyhow::anyhow!("native actor record is not ready"))
        }
        Err(e) => Err(e.into()),
    }
}

fn actor(
    store: &Store,
    env: &HookEnv,
    client: Client,
    event: HookEvent,
    input: &HookInput,
) -> Result<RegisteredActor> {
    let instance = env.another_home.join("adapters/v1").join(client.as_str());
    private_dir(&instance)?;
    let native_session = input
        .session_id
        .as_deref()
        .context("NO_ACTOR: no native session")?;
    let parent_path = instance.join(format!(
        "parent-{}.json",
        another_core::crypto::sha256_hex(native_session.as_bytes())
    ));
    let known_parent: Option<String> = if event == HookEvent::ChildStart && client == Client::Grok {
        Some(native_session.into())
    } else if parent_path.is_file() {
        Some(
            serde_json::from_slice(&fs::read(&parent_path)?)
                .map_err(|_| anyhow::anyhow!("invalid private child binding"))?,
        )
    } else {
        None
    };
    let binding = map_native_actor(profile(client), &input.raw, known_parent.as_deref())?;
    if event == HookEvent::ChildStart && binding.child_id.is_none() {
        bail!("NO_ACTOR: dependent start has no child discriminator");
    }
    let grant_path = instance.join("participant.json");
    // Late/unknown Post or End must not enroll a new participant/root.
    if matches!(event, HookEvent::PostTool | HookEvent::SessionEnd) && !grant_path.is_file() {
        bail!("NO_ACTOR: native event has no private adapter registration");
    }
    let participant = participant(store, &grant_path)?;
    let root_key = binding_key(client, &binding.root_session, None);
    if (matches!(event, HookEvent::PostTool | HookEvent::SessionEnd)
        || (client == Client::Grok && event == HookEvent::PreEdit))
        && !store.has_native_actor(&participant, &root_key)?
    {
        bail!("NO_ACTOR: native event has no registered root");
    }
    let root = retain_actor(
        &instance,
        &root_key,
        store.register_actor(&participant, &root_key, client.as_str(), None)?,
    )?;
    let Some(child) = binding.child_id.as_deref() else {
        return Ok(root);
    };
    let child_key = binding_key(client, &binding.root_session, Some(child));
    if matches!(event, HookEvent::PostTool | HookEvent::SessionEnd) {
        // Resolve retained proof even after the parent closed, without reopening it.
        return retain_actor(
            &instance,
            &child_key,
            store.resolve_registered_actor(&participant, &child_key)?,
        );
    }
    let label = format!(
        "child-{}",
        &another_core::crypto::sha256_hex(child.as_bytes())[..12]
    );
    let registered = store.register_actor(&participant, &child_key, &label, Some(&root.proof))?;
    let registered = retain_actor(&instance, &child_key, registered)?;
    remember_parent(&instance, child, &binding.root_session)?;
    Ok(registered)
}

fn remember_parent(instance: &Path, child: &str, parent: &str) -> Result<()> {
    let path = instance.join(format!(
        "parent-{}.json",
        another_core::crypto::sha256_hex(child.as_bytes())
    ));
    if path.is_file() {
        let existing: String = serde_json::from_slice(&fs::read(&path)?)
            .map_err(|_| anyhow::anyhow!("invalid private child binding"))?;
        if existing != parent {
            bail!("native child has contradictory parent binding");
        }
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(parent)?)?;
    file.sync_all()?;
    Ok(())
}

pub fn deny(client: Client, reason: &str) -> String {
    if client == Client::Cursor {
        json!({"permission":"deny", "user_message":reason, "agent_message":reason}).to_string()
    } else {
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse", "permissionDecision":"deny", "permissionDecisionReason":reason}}).to_string()
    }
}

/// Write tools whose every target is one of the fields `HookInput::edit_paths` reads:
/// a single file path (Claude, Grok, Cursor edit tools) or the file headers of an
/// `apply_patch` patch (Codex).
fn known_write_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            | "apply_patch"
            | "search_replace"
            | "StrReplace"
            | "EditNotebook"
    )
}

fn shell(tool: &str) -> bool {
    matches!(
        tool,
        "Bash"
            | "Shell"
            | "exec_command"
            | "run_terminal_command"
            | "shell_command"
            | "functions.exec"
            | "functions.exec_command"
            | "mcp__exec_command"
    )
}

/// No implicit allow decision bypasses the client's own permission prompt.
pub fn event(
    store: &Store,
    env: &HookEnv,
    event: HookEvent,
    client: Client,
    input: &HookInput,
) -> Result<String> {
    let registered = actor(store, env, client, event, input)?;
    let proof = &registered.proof;
    if event == HookEvent::SessionEnd {
        let id = input
            .raw
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or("native-session-end");
        store.end_actor_session(proof, id)?;
        return Ok(String::new());
    }
    if event == HookEvent::PostTool {
        if let Some(call_id) = input.call_id.as_deref() {
            let outcome = map_native_actor(profile(client), &input.raw, None)?
                .outcome
                .unwrap_or(EditOutcome::Unknown);
            // A read, shell or denied Pre has no admitted operation. It cannot renew.
            if let Err(e) = store.complete_actor_edit(proof, call_id, outcome) {
                if !matches!(e, another_core::Error::Forbidden(_)) {
                    return Err(e.into());
                }
            }
        }
        // Ended epochs may record late outcomes, never activity/presence.
        if registered.actor.closed_at.is_some() {
            return Ok(String::new());
        }
    }
    let reminder = store.actor_activity(proof)?;
    if client == Client::Cursor && event == HookEvent::PostTool {
        if let Some(child) = another_core::cursor_task_child(&input.raw) {
            if let Some(parent) = input.session_id.as_deref() {
                remember_parent(&env.another_home.join("adapters/v1/cursor"), &child, parent)?;
            }
        }
    }
    if event == HookEvent::PreEdit {
        if client == Client::Cursor {
            return Ok(deny(
                client,
                "NO_ACTOR: Cursor is unguarded and has no tested shell-proof route",
            ));
        }
        let tool = input
            .tool_name
            .as_deref()
            .context("NO_CALL_CONTEXT: missing native tool")?;
        if shell(tool) {
            return Ok(deny(client, "NO_CALL_CONTEXT: guarded shell execution needs an authenticated exact-target call declaration; no native shell carriage is implemented"));
        }
        let paths = input.edit_paths();
        // Only a tool whose whole write set `edit_paths` is known to parse is admitted
        // from its extracted paths. Any other tool that happens to carry a familiar
        // path key may write elsewhere too, so it is refused, never guessed.
        if !paths.is_empty() && known_write_tool(tool) {
            let call_id = input
                .call_id
                .as_deref()
                .context("NO_CALL_CONTEXT: edit has no native call id")?;
            let cwd = input
                .cwd
                .as_ref()
                .context("NO_CALL_CONTEXT: edit has no working directory")?;
            let mut targets = Vec::new();
            for path in paths {
                for target in crate::hooks::edit_targets(&cwd.join(path)) {
                    if target.is_dir() {
                        bail!("exact file targets required");
                    }
                    let target = target.to_string_lossy().into_owned();
                    if !targets.contains(&target) {
                        targets.push(target);
                    }
                }
            }
            store.begin_actor_edit(proof, call_id, &targets, None)?;
            store.check_actor_edit(proof, call_id)?;
        } else if !matches!(
            tool,
            "Read" | "read_file" | "Glob" | "Grep" | "Search" | "list_dir" | "Task" | "Agent"
        ) {
            return Ok(deny(
                client,
                "NO_CALL_CONTEXT: unsupported tool with unknown write targets",
            ));
        }
    }
    Ok(match (client, reminder) {
        (Client::Cursor, _) if event == HookEvent::Prompt => json!({"continue":true}).to_string(),
        (Client::Cursor, Some(text)) => json!({"additional_context":text}).to_string(),
        (_, Some(text)) if event.is_tool_event() => json!({"hookSpecificOutput":{"hookEventName":if event == HookEvent::PreEdit {"PreToolUse"} else {"PostToolUse"},"additionalContext":text}}).to_string(),
        (_, Some(text)) => text,
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use another_core::Clock;

    #[cfg(unix)]
    #[test]
    fn a_grant_readable_by_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory(Clock::System).unwrap();
        let path = dir.path().join("participant.json");
        participant(&store, &path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let Err(refused) = participant(&store, &path) else {
            panic!("a group- or world-readable grant was accepted");
        };
        let refused = refused.to_string();
        assert!(refused.contains("not private"), "{refused}");
    }

    /// The stated Windows limit: no mode check exists, so the grant is created once,
    /// read back unchanged and kept inside the Another home whose ACL it inherits.
    #[cfg(windows)]
    #[test]
    fn a_windows_grant_stays_inside_the_home_and_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory(Clock::System).unwrap();
        let path = dir.path().join("adapters").join("participant.json");
        private_dir(path.parent().unwrap()).unwrap();
        let first = participant(&store, &path).unwrap();
        let again = participant(&store, &path).unwrap();
        assert_eq!(first.participant_id, again.participant_id);
        assert!(path.starts_with(dir.path()));
    }
}
