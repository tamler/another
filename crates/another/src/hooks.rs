//! Hook packs: install a tiny POSIX wrapper into each client's hook config, and the
//! `another hook run` entry point that the wrapper calls to record sessions and print a
//! capped inbox + activity digest.
//!
//! Every client is treated the same way: session start registers the agent (if needed),
//! records `session.started` and prints a digest; each prompt renews holds and mentions
//! only mail that arrived since the last notice; session end releases holds.
//!
//! Two hooks run inside a task, between tool calls, where the client supports them:
//! `post-tool` mentions new mail mid-task (read only, nothing printed when nothing is new),
//! and `pre-edit` takes a narrow hold on each file about to be edited, or stops the edit
//! when another agent holds it. Both open the database without migrating and with a short
//! lock timeout, and both fail open: an error never blocks the agent.

use std::io::{IsTerminal, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use another_core::{
    HoldConflict, InboxQuery, Program, RegisterAgent, SessionInfo, SessionPhase, Since, Store,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Substring that identifies hook entries Another owns inside client configs.
pub const MARKER: &str = "another-hook.sh";
/// Hook script and Grok file names used before the project was renamed to Another
/// (see DECISIONS D35). They are recognised only so `hook install` / `hook uninstall`
/// replace or remove entries left by development builds; nothing new is written with them.
const LEGACY_MARKERS: &[&str] = &["wyres-hook.sh"];
const LEGACY_GROK_FILE: &str = "wyres.json";

fn mentions_ours(text: &str) -> bool {
    text.contains(MARKER) || LEGACY_MARKERS.iter().any(|m| text.contains(m))
}
/// Upper bound on injected digest text.
pub const MAX_DIGEST_CHARS: usize = 1500;
const MAX_STDIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Claude,
    Codex,
    Grok,
    Cursor,
}

impl Client {
    pub const ALL: [Client; 4] = [Client::Claude, Client::Codex, Client::Grok, Client::Cursor];

    pub fn as_str(self) -> &'static str {
        match self {
            Client::Claude => "claude",
            Client::Codex => "codex",
            Client::Grok => "grok",
            Client::Cursor => "cursor",
        }
    }

    fn program(self) -> Program {
        match self {
            Client::Claude => Program::Claude,
            Client::Codex => Program::Codex,
            Client::Grok => Program::Grok,
            Client::Cursor => Program::Cursor,
        }
    }

    /// The user-level config file this client reads hooks from.
    pub fn config_path(self, home: &Path) -> PathBuf {
        match self {
            Client::Claude => home.join(".claude").join("settings.json"),
            Client::Codex => home.join(".codex").join("hooks.json"),
            Client::Grok => home.join(".grok").join("hooks").join("another.json"),
            Client::Cursor => home.join(".cursor").join("hooks.json"),
        }
    }

    /// The hook entries Another installs for this client.
    fn specs(self) -> Vec<HookSpec> {
        let spec = |key, event, timeout, matcher| HookSpec {
            key,
            event,
            timeout,
            matcher,
        };
        match self {
            Client::Cursor => vec![
                spec("sessionStart", HookEvent::SessionStart, 10, None),
                spec("beforeSubmitPrompt", HookEvent::Prompt, 5, None),
                spec("sessionEnd", HookEvent::SessionEnd, 3, None),
                // No edit guard: Cursor blocks a tool call when a preToolUse reply is not
                // valid JSON, and a hook that fails open prints nothing.
                spec("postToolUse", HookEvent::PostTool, 5, None),
            ],
            _ => {
                let edits = if self == Client::Codex {
                    CODEX_EDIT_TOOLS
                } else {
                    EDIT_TOOLS
                };
                vec![
                    spec("SessionStart", HookEvent::SessionStart, 10, None),
                    spec("UserPromptSubmit", HookEvent::Prompt, 5, None),
                    spec("SessionEnd", HookEvent::SessionEnd, 3, None),
                    spec("PostToolUse", HookEvent::PostTool, 5, None),
                    spec("PreToolUse", HookEvent::PreEdit, 5, Some(edits)),
                ]
            }
        }
    }
}

/// Claude Code's file edit tools. Grok maps these names onto its own edit tools.
const EDIT_TOOLS: &str = "Edit|Write|MultiEdit|NotebookEdit";
/// Codex edits files with `apply_patch`, which its matchers also accept as `Edit` or `Write`.
const CODEX_EDIT_TOOLS: &str = "apply_patch|Edit|Write";

/// The `statusMessage` for each of our Codex hooks. Codex documents it as text shown while
/// the hook runs; it gives each hook a clear label next to its command. Whether `/hooks`
/// shows it too has not been checked against a live Codex.
fn codex_status(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "Another: registering this session",
        HookEvent::Prompt => "Another: checking for new messages",
        HookEvent::PostTool => "Another: delivering messages mid-task",
        HookEvent::PreEdit => "Another: checking file holds before an edit",
        HookEvent::SessionEnd => "Another: closing this session",
    }
}

/// Top-level `description` for Codex's hooks.json, set only when the file has none.
const CODEX_DESCRIPTION: &str =
    "Another coordination hooks (toanother.one): messages and file holds between coding agents";

/// One hook entry Another installs for a client.
#[derive(Debug, Clone, Copy)]
struct HookSpec {
    /// The client's event name.
    key: &'static str,
    event: HookEvent,
    /// Seconds the client waits for the hook.
    timeout: u32,
    /// Tool matcher for tool events; `None` matches every tool.
    matcher: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    Prompt,
    SessionEnd,
    /// After each tool call: mention mail that arrived mid-task.
    PostTool,
    /// Before a file edit: hold the file, or stop the edit if another agent holds it.
    PreEdit,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::Prompt => "prompt",
            HookEvent::SessionEnd => "session-end",
            HookEvent::PostTool => "post-tool",
            HookEvent::PreEdit => "pre-edit",
        }
    }

    /// Runs between tool calls inside a task (as opposed to once per session or prompt).
    pub fn is_tool_event(self) -> bool {
        matches!(self, HookEvent::PostTool | HookEvent::PreEdit)
    }

    /// How long a hook waits for a database lock before giving up (and failing open).
    pub fn busy_timeout(self) -> Duration {
        match self {
            // Runs on every tool call: even a fully locked database must cost under ~100 ms.
            HookEvent::PostTool => Duration::from_millis(50),
            HookEvent::PreEdit => Duration::from_millis(300),
            _ => Duration::from_secs(5),
        }
    }
}

/// Locations used by install/uninstall.
#[derive(Debug, Clone)]
pub struct HookEnv {
    /// The user's home directory (client configs live under it).
    pub home: PathBuf,
    /// The Another data directory (the wrapper script lives under it).
    pub another_home: PathBuf,
    /// The `another` binary the wrapper should call.
    pub exe: PathBuf,
}

impl HookEnv {
    pub fn detect(another_home: PathBuf) -> Result<HookEnv> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(HookEnv {
            home,
            another_home,
            exe: std::env::current_exe().context("locating the another binary")?,
        })
    }

    pub fn script_path(&self) -> PathBuf {
        self.another_home.join("hooks").join(MARKER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Create,
    Update,
    Delete,
    Unchanged,
}

/// One planned or applied file change.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub change: ChangeKind,
    /// Where the previous version was copied before writing (when applied).
    pub backup: Option<String>,
    #[serde(skip)]
    after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookReport {
    pub dry_run: bool,
    pub changes: Vec<FileChange>,
    pub notes: Vec<String>,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn script_text(env: &HookEnv) -> String {
    format!(
        "#!/bin/sh\n\
         # Another hook wrapper, written by `another hook install`; removed by `another hook uninstall`.\n\
         # It always exits 0, so a missing or failing another never blocks the agent.\n\
         ANOTHER_BIN={exe}\n\
         if [ ! -x \"$ANOTHER_BIN\" ]; then\n\
         \x20 ANOTHER_BIN=$(command -v another 2>/dev/null) || exit 0\n\
         fi\n\
         if [ -z \"${{ANOTHER_HOME:-}}\" ]; then\n\
         \x20 ANOTHER_HOME={home}\n\
         \x20 export ANOTHER_HOME\n\
         fi\n\
         \"$ANOTHER_BIN\" hook run \"$@\" 2>/dev/null\n\
         exit 0\n",
        exe = sh_quote(&env.exe.to_string_lossy()),
        home = sh_quote(&env.another_home.to_string_lossy()),
    )
}

fn command_for(env: &HookEnv, client: Client, event: HookEvent) -> String {
    format!(
        "{} {} --client {}",
        sh_quote(&env.script_path().to_string_lossy()),
        event.as_str(),
        client.as_str()
    )
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(mentions_ours)
}

fn read_json(path: &Path) -> Result<Option<(String, Map<String, Value>)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some((text, Map::new())));
    }
    let value: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON; not modifying it (fix or move it, then retry)",
            path.display()
        )
    })?;
    match value {
        Value::Object(map) => Ok(Some((text, map))),
        _ => bail!("{} is not a JSON object; not modifying it", path.display()),
    }
}

fn object_entry<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    let v = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    v.as_object_mut()
        .with_context(|| format!("\"{key}\" is not an object; not modifying it"))
}

fn array_entry<'a>(map: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Vec<Value>> {
    let v = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    v.as_array_mut()
        .with_context(|| format!("hooks.{key} is not an array; not modifying it"))
}

/// Add or refresh our entries in a config, leaving every other key and hook alone.
fn install_into(client: Client, env: &HookEnv, root: &mut Map<String, Value>) -> Result<()> {
    if client == Client::Cursor && !root.contains_key("version") {
        root.insert("version".into(), json!(1));
    }
    if client == Client::Codex {
        // Only fill a missing (or blank) description; never replace one the user wrote.
        let blank = match root.get("description") {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.trim().is_empty(),
            Some(_) => false,
        };
        if blank {
            root.insert("description".into(), json!(CODEX_DESCRIPTION));
        }
    }
    let hooks = object_entry(root, "hooks")?;
    for spec in client.specs() {
        let command = command_for(env, client, spec.event);
        let groups = array_entry(hooks, spec.key)?;
        let timeout = spec.timeout;
        let mut ours = json!({ "type": "command", "command": command, "timeout": timeout });
        if client == Client::Codex {
            ours["statusMessage"] = json!(codex_status(spec.event));
        }
        if client == Client::Cursor {
            // Cursor: a flat list of hook definitions per event.
            match groups.iter_mut().find(|h| is_ours(h)) {
                Some(h) => *h = json!({ "command": command, "timeout": timeout }),
                None => groups.push(json!({ "command": command, "timeout": timeout })),
            }
            let mut kept = false;
            groups.retain(|h| !is_ours(h) || !std::mem::replace(&mut kept, true));
            continue;
        }
        // Claude / Codex / Grok: matcher groups, each with a `hooks` array.
        let mut found = false;
        for group in groups.iter_mut() {
            let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let Some(h) = hs.iter_mut().find(|h| is_ours(h)) else {
                continue;
            };
            *h = ours.clone();
            // A group holding only our entry is ours: keep its tool matcher current. A group
            // someone else also uses keeps whatever matcher they gave it.
            if spec.event.is_tool_event() && hs.len() == 1 {
                if let Some(g) = group.as_object_mut() {
                    match spec.matcher {
                        Some(m) => {
                            g.insert("matcher".into(), json!(m));
                        }
                        None => {
                            g.remove("matcher");
                        }
                    }
                }
            }
            found = true;
            break;
        }
        if !found {
            groups.push(match spec.matcher {
                Some(m) => json!({ "matcher": m, "hooks": [ours] }),
                None => json!({ "hooks": [ours] }),
            });
        }
        // Keep one entry: drop older copies (e.g. pre-rename entries), and any group that
        // held only such a copy.
        let mut kept = false;
        groups.retain_mut(|g| {
            let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let n = hs.len();
            hs.retain(|h| !is_ours(h) || !std::mem::replace(&mut kept, true));
            !(hs.is_empty() && n > 0)
        });
    }
    Ok(())
}

/// Remove our entries; drop groups and event lists only when we emptied them.
fn uninstall_from(client: Client, root: &mut Map<String, Value>) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for spec in client.specs() {
        let key = spec.key;
        let Some(groups) = hooks.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = groups.len();
        if client == Client::Cursor {
            groups.retain(|h| !is_ours(h));
        } else {
            groups.retain_mut(|g| {
                let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                    return true;
                };
                let n = hs.len();
                hs.retain(|h| !is_ours(h));
                !(hs.is_empty() && n > 0)
            });
        }
        if groups.is_empty() && before > 0 {
            hooks.remove(key);
        }
    }
}

fn contains_ours(root: &Map<String, Value>) -> bool {
    serde_json::to_string(root).is_ok_and(|s| mentions_ours(&s))
}

fn render(root: &Map<String, Value>) -> Result<String> {
    Ok(serde_json::to_string_pretty(root)? + "\n")
}

fn plan_config(client: Client, env: &HookEnv, install: bool) -> Result<FileChange> {
    let path = client.config_path(&env.home);
    let existing = read_json(&path)?;
    let (before, mut root) = match existing {
        Some((text, map)) => (Some(text), map),
        None => (None, Map::new()),
    };
    if client == Client::Grok && before.is_some() && !root.is_empty() && !contains_ours(&root) {
        bail!(
            "{} exists and was not written by another; not modifying it",
            path.display()
        );
    }
    let (change, after) = if install {
        install_into(client, env, &mut root)?;
        let after = render(&root)?;
        let change = match &before {
            None => ChangeKind::Create,
            Some(b) if b.trim() == after.trim() => ChangeKind::Unchanged,
            Some(_) => ChangeKind::Update,
        };
        (change, Some(after))
    } else {
        match &before {
            None => (ChangeKind::Unchanged, None),
            Some(b) => {
                uninstall_from(client, &mut root);
                let grok_now_empty = client == Client::Grok
                    && root
                        .get("hooks")
                        .and_then(Value::as_object)
                        .is_none_or(|h| h.is_empty());
                if grok_now_empty {
                    (ChangeKind::Delete, None)
                } else {
                    let after = render(&root)?;
                    if b.trim() == after.trim() || !contains_ours_text(b) {
                        (ChangeKind::Unchanged, None)
                    } else {
                        (ChangeKind::Update, Some(after))
                    }
                }
            }
        }
    };
    Ok(FileChange {
        path: path.display().to_string(),
        change,
        backup: None,
        after,
    })
}

fn contains_ours_text(text: &str) -> bool {
    mentions_ours(text)
}

fn timestamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    another_core::fmt_ts(ms)
        .chars()
        .filter(char::is_ascii_digit)
        .collect()
}

fn apply(change: &mut FileChange) -> Result<()> {
    let path = PathBuf::from(&change.path);
    if matches!(change.change, ChangeKind::Update | ChangeKind::Delete) && path.exists() {
        let backup = PathBuf::from(format!("{}.another-backup-{}", change.path, timestamp()));
        std::fs::copy(&path, &backup).with_context(|| format!("backing up {}", path.display()))?;
        change.backup = Some(backup.display().to_string());
    }
    match change.change {
        ChangeKind::Unchanged => {}
        ChangeKind::Delete => std::fs::remove_file(&path)?,
        ChangeKind::Create | ChangeKind::Update => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_preserving(&path, change.after.as_deref().unwrap_or_default())
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Atomically replace `path` with `text`, keeping the existing file's permissions (and, on
/// unix, its owner where allowed). New files are created private (0600). A symlinked config
/// is written through to its target so dotfile links survive.
fn write_preserving(path: &Path, text: &str) -> Result<()> {
    let path = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let existing = std::fs::metadata(&path).ok();
    let tmp = path.with_extension("another-tmp");
    // A stale temp file would keep its old mode; start from a fresh one.
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    if let Some(meta) = &existing {
        std::fs::set_permissions(&tmp, meta.permissions())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Only succeeds when allowed (e.g. same owner, or root); otherwise the new file
            // keeps the current user's ownership, as before.
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn plan_script(env: &HookEnv, install: bool, still_used: bool) -> FileChange {
    let path = env.script_path();
    let current = std::fs::read_to_string(&path).ok();
    let (change, after) = if install {
        let text = script_text(env);
        match &current {
            None => (ChangeKind::Create, Some(text)),
            Some(c) if *c == text => (ChangeKind::Unchanged, None),
            Some(_) => (ChangeKind::Update, Some(text)),
        }
    } else if current.is_some() && !still_used {
        (ChangeKind::Delete, None)
    } else {
        (ChangeKind::Unchanged, None)
    };
    FileChange {
        path: path.display().to_string(),
        change,
        backup: None,
        after,
    }
}

/// Install (or with `install = false`, uninstall) hooks for `clients`.
pub fn install(
    env: &HookEnv,
    clients: &[Client],
    install: bool,
    dry_run: bool,
) -> Result<HookReport> {
    // Plan everything first so a bad config aborts before anything is written.
    let mut changes = clients
        .iter()
        .map(|c| plan_config(*c, env, install))
        .collect::<Result<Vec<_>>>()?;
    let still_used = !install
        && Client::ALL
            .iter()
            .filter(|c| !clients.contains(c))
            .any(|c| {
                std::fs::read_to_string(c.config_path(&env.home))
                    .is_ok_and(|t| contains_ours_text(&t))
            });
    let script = plan_script(env, install, still_used);
    // A Grok hook file left by a pre-rename build would run alongside the new one.
    if clients.contains(&Client::Grok) {
        let legacy = env.home.join(".grok").join("hooks").join(LEGACY_GROK_FILE);
        if std::fs::read_to_string(&legacy).is_ok_and(|t| mentions_ours(&t)) {
            changes.push(FileChange {
                path: legacy.display().to_string(),
                change: ChangeKind::Delete,
                backup: None,
                after: None,
            });
        }
    }
    // The script is written before configs reference it, and removed after.
    if install {
        changes.insert(0, script);
    } else {
        changes.push(script);
    }
    if !dry_run {
        for change in &mut changes {
            apply(change)?;
        }
        if install {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    env.script_path(),
                    std::fs::Permissions::from_mode(0o755),
                )?;
            }
        }
    }
    let mut notes = Vec::new();
    if install {
        if clients.contains(&Client::Codex) {
            notes.push(
                "Codex runs new or changed hooks only after review: open Codex and trust them under /hooks."
                    .to_string(),
            );
        }
        if clients.contains(&Client::Grok) {
            notes.push(
                "Grok also reads Claude and Cursor hook files; another skips those copies when the Grok pack is installed, so each event runs once."
                    .to_string(),
            );
        }
        if clients.contains(&Client::Cursor) {
            notes.push(
                "Cursor takes injected context at session start and after tool calls; its prompt hook just renews holds, and the edit guard is not installed there."
                    .to_string(),
            );
        }
    }
    Ok(HookReport {
        dry_run,
        changes,
        notes,
    })
}

// ---------------------------------------------------------------------------
// `another hook run`

/// What the client told us on stdin (all optional; clients differ).
#[derive(Debug, Default)]
pub struct HookInput {
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
    /// Tool events: the tool's name (`Edit`, `apply_patch`, `search_replace`, ...).
    pub tool_name: Option<String>,
    /// Tool events: the tool's arguments.
    pub tool_input: Option<Value>,
}

impl HookInput {
    pub fn parse(raw: &str) -> HookInput {
        let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let cwd = s("cwd")
            .or_else(|| s("workspaceRoot"))
            .or_else(|| {
                v.get("workspace_roots")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .map(PathBuf::from);
        let session_id = s("session_id")
            .or_else(|| s("sessionId"))
            .or_else(|| s("conversation_id"));
        let tool_name = s("tool_name").or_else(|| s("toolName"));
        let tool_input = v
            .get("tool_input")
            .or_else(|| v.get("toolInput"))
            .filter(|t| !t.is_null())
            .cloned();
        HookInput {
            cwd,
            session_id,
            tool_name,
            tool_input,
        }
    }

    /// Files an edit tool is about to change, as given (absolute or relative to `cwd`).
    /// Reads the path keys the supported clients use, and the file headers of a Codex
    /// `apply_patch` patch. Empty when the tool names no file.
    pub fn edit_paths(&self) -> Vec<String> {
        let Some(input) = &self.tool_input else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        let mut add = |p: &str| {
            let p = p.trim();
            if !p.is_empty() && !out.iter().any(|q| q == p) {
                out.push(p.to_string());
            }
        };
        let patch = input
            .get("command")
            .or_else(|| input.get("patch"))
            .or_else(|| input.get("input"))
            .and_then(Value::as_str)
            .or_else(|| input.as_str());
        if let Some(patch) = patch.filter(|p| p.contains("*** Begin Patch")) {
            for line in patch.lines() {
                for header in [
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ] {
                    if let Some(p) = line.strip_prefix(header) {
                        add(p);
                    }
                }
            }
        } else {
            for key in [
                "file_path",
                "filePath",
                "notebook_path",
                "path",
                "target_notebook",
            ] {
                if let Some(p) = input.get(key).and_then(Value::as_str) {
                    add(p);
                }
            }
        }
        out
    }

    fn read_stdin() -> HookInput {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            return HookInput::default();
        }
        let mut raw = String::new();
        let _ = stdin.lock().take(MAX_STDIN_BYTES).read_to_string(&mut raw);
        HookInput::parse(&raw)
    }
}

/// Nearest ancestor containing `.git`, else `dir` itself.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Grok also executes Claude/Cursor hook files. When the Grok pack is installed, those
/// copies stand down so each event runs once; otherwise they act on Grok's behalf.
fn effective_client(client: Client, env: &HookEnv) -> Option<Client> {
    if client == Client::Grok || std::env::var_os("GROK_HOOK_EVENT").is_none() {
        return Some(client);
    }
    let grok_pack = std::fs::read_to_string(Client::Grok.config_path(&env.home))
        .is_ok_and(|t| contains_ours_text(&t));
    if grok_pack {
        None
    } else {
        Some(Client::Grok)
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        let cut = s
            .char_indices()
            .nth(max.saturating_sub(1))
            .map_or(s.len(), |(i, _)| i);
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// Resolve (and register on first use) the agent a hook acts for.
fn hook_agent(store: &Store, client: Client, root: &Path) -> Result<String> {
    if let Some(agent) = std::env::var("ANOTHER_AGENT")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        return Ok(agent);
    }
    let project = store.ensure_project(client.as_str(), &root.to_string_lossy())?;
    let address = format!("{}@{}", client.as_str(), project.slug);
    if store.agent(&address).is_err() {
        store.register_agent(RegisterAgent {
            name: client.as_str().to_string(),
            program: client.program(),
            project: Some(project.abs_path),
            model: None,
            description: None,
        })?;
    }
    Ok(address)
}

/// Inbox + activity digest for session start, capped at [`MAX_DIGEST_CHARS`].
pub fn session_digest(store: &Store, agent: &str) -> Result<String> {
    let inbox = store.inbox(
        agent,
        InboxQuery {
            unread_only: true,
            urgent_only: false,
            limit: 5,
        },
    )?;
    let mut out = format!(
        "Another: {} has {} unread ({} urgent), {} awaiting ack, {} open request(s).",
        inbox.agent,
        inbox.unread,
        inbox.urgent_unread,
        inbox.awaiting_ack,
        inbox.requests.len()
    );
    for item in &inbox.messages {
        let m = &item.message;
        let tag = match m.importance {
            another_core::Importance::Urgent => "[urgent] ",
            another_core::Importance::High => "[high] ",
            _ => "",
        };
        out.push_str(&format!(
            "\n- {tag}from {}: \"{}\" (message {})",
            m.from, m.subject, m.message_id
        ));
    }
    for r in inbox.requests.iter().take(5) {
        out.push_str(&format!(
            "\n- request {} \"{}\" from {} ({}{})",
            r.request_id,
            r.title,
            r.created_by,
            r.status.as_str(),
            r.owner
                .as_deref()
                .map(|o| format!(", owner {o}"))
                .unwrap_or_default()
        ));
    }
    let summary = store.activity_summary(Some(Since::Time(store.now() - 3_600_000)))?;
    out.push_str("\nLast hour: ");
    out.push_str(summary.text.lines().next().unwrap_or_default());
    for h in summary.highlights.iter().rev().take(3).rev() {
        out.push_str("\n- ");
        out.push_str(h);
    }
    out.push_str(&format!(
        "\nThe Another MCP tools fetch_inbox, acknowledge and claim_request (or `another inbox --agent {}`) show details.",
        inbox.agent
    ));
    Ok(truncate(out, MAX_DIGEST_CHARS))
}

/// A per-agent state file. The name encodes every character outside `[A-Za-z0-9-]` as
/// `_XX` hex, so distinct agent addresses (`a.b`, `a_b`) never share a file.
fn state_path(another_home: &Path, agent: &str, ext: &str) -> PathBuf {
    let mut safe = String::new();
    for b in agent.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            safe.push(b as char);
        } else {
            safe.push_str(&format!("_{b:02x}"));
        }
    }
    another_home
        .join("hooks")
        .join("state")
        .join(format!("{safe}.{ext}"))
}

/// A notice and the messages it announced.
#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub message_ids: Vec<String>,
}

/// Most messages one announcement claims. A larger burst is announced in batches on the
/// following calls, so every message is announced, and counted, exactly once.
pub const NOTICE_BATCH: u32 = 1000;

/// One line about unread mail no channel has announced yet (or nothing).
///
/// Candidates are read without writing; the caller then claims them in one transaction
/// ([`Store::claim_announcement`]) and shows only what it won, so concurrent hooks and the
/// daemon announce each message once. `channel` is `post_tool`, `prompt` or
/// `session_start`.
pub fn prompt_notice(
    store: &Store,
    agent: &str,
    channel: &str,
    client: Client,
) -> Result<Option<Notice>> {
    let unread = store.unannounced(agent, "", NOTICE_BATCH)?;
    if unread.items.is_empty() {
        return Ok(None);
    }
    let ids: Vec<String> = unread.items.iter().map(|i| i.message_id.clone()).collect();
    let claimed = store.claim_announcement(agent, &ids, channel, Some(client.as_str()))?;
    if claimed.is_empty() {
        return Ok(None);
    }
    let items: Vec<_> = unread
        .items
        .iter()
        .filter(|i| claimed.contains(&i.message_id))
        .collect();
    let urgent = items
        .iter()
        .filter(|i| {
            matches!(
                i.importance,
                another_core::Importance::High | another_core::Importance::Urgent
            )
        })
        .count();
    let subjects: Vec<String> = items
        .iter()
        .take(3)
        .map(|i| {
            let request = i
                .request_id
                .as_deref()
                .map(|r| format!(" (opens request {r})"))
                .unwrap_or_default();
            format!("\"{}\" from {}{request}", i.subject, i.from)
        })
        .collect();
    let line = format!(
        "Another: {} new message(s) for {} ({} urgent): {}. Read them with fetch_inbox (or `another inbox --agent {} --unread`).",
        claimed.len(),
        unread.agent,
        urgent,
        subjects.join(", "),
        unread.agent,
    );
    Ok(Some(Notice {
        text: truncate(line, 500),
        message_ids: claimed,
    }))
}

/// The registered agent a hook acts for, without registering anything: `None` when the
/// project or agent is not known yet (session start registers them).
fn known_agent(store: &Store, client: Client, root: &Path) -> Result<Option<String>> {
    if let Some(agent) = std::env::var("ANOTHER_AGENT")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        return Ok(Some(agent));
    }
    let project = match store.project(&root.to_string_lossy()) {
        Ok(p) => p,
        Err(another_core::Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match store.agent(&format!("{}@{}", client.as_str(), project.slug)) {
        Ok(a) => Ok(Some(a.address)),
        Err(another_core::Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Seconds between mid-task keepalives for one agent.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(60);

/// Presence and hold renewal while a task runs, at most once a minute per agent, best
/// effort: a busy database just skips this round.
fn keepalive_throttled(store: &Store, another_home: &Path, agent: &str) {
    let stamp = state_path(another_home, agent, "beat");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < KEEPALIVE_EVERY);
    if fresh {
        return;
    }
    // Stamp first so parallel tool calls do not all write.
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&stamp, b"");
    if let Err(e) = store.keepalive(agent) {
        tracing::debug!(error = %e, "keepalive skipped");
    }
}

/// `post-tool`: mention mail that arrived mid-task, in the client's context format.
/// Prints nothing when nothing is new or the agent is not registered yet.
fn post_tool(store: &Store, env: &HookEnv, client: Client, dir: &Path) -> Result<String> {
    let root = project_root(dir);
    let Some(agent) = known_agent(store, client, &root)? else {
        return Ok(String::new());
    };
    keepalive_throttled(store, &env.another_home, &agent);
    // The claim and its `push.notified` evidence are one transaction: a notice is shown
    // only if it was recorded, and only by the one caller that won it.
    let Some(notice) = prompt_notice(store, &agent, "post_tool", client)? else {
        return Ok(String::new());
    };
    Ok(match client {
        Client::Cursor => json!({ "additional_context": notice.text }).to_string(),
        _ => json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": notice.text,
            }
        })
        .to_string(),
    })
}

/// Symlinks followed while resolving one path before giving up (a loop).
const MAX_SYMLINKS: usize = 40;

/// `path` as the filesystem resolves it, the way `open` follows it:
///
/// - components are taken left to right and each symlink is followed before any later
///   `..` is applied, so `link/../a` is "the parent of where `link` points, then `a`",
///   not the lexical `a`;
/// - a dangling symlink is read and followed to its (not yet existing) target;
/// - once a component does not exist, the rest is applied lexically, since a missing
///   directory cannot be a link;
/// - the existing part is then canonicalized, which also restores on-disk letter case.
///
/// On a symlink loop (or a chain longer than the OS allows), `path` is used as given with
/// its longest resolvable leading part canonicalized (it stops at the loop, even before a
/// later `..`), so it still names the registered project.
/// `path` should be absolute.
pub fn resolve_existing(path: &Path) -> PathBuf {
    let mut budget = MAX_SYMLINKS;
    canonical_prefix(&follow_links(path, &mut budget).unwrap_or_else(|| path.to_path_buf()))
}

fn follow_links(path: &Path, budget: &mut usize) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut missing = false;
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `out` is fully resolved (or a missing, so link-free, tail): its parent is
                // the physical parent. Climbing out of a missing tail can land on an
                // existing directory again, whose entries may be links.
                out.pop();
                missing = std::fs::symlink_metadata(&out).is_err();
            }
            Component::Normal(name) => {
                let next = out.join(name);
                if missing {
                    out = next;
                    continue;
                }
                match std::fs::symlink_metadata(&next) {
                    Err(_) => {
                        missing = true;
                        out = next;
                    }
                    Ok(meta) if meta.file_type().is_symlink() => {
                        *budget = budget.checked_sub(1)?;
                        let target = std::fs::read_link(&next).ok()?;
                        out = follow_links(&out.join(target), budget)?;
                        missing = std::fs::symlink_metadata(&out).is_err();
                    }
                    Ok(_) => out = next,
                }
            }
        }
    }
    Some(out)
}

/// Canonicalize the longest leading run of `path`'s components that resolves, and keep the
/// rest as written. For a link-free path that is its deepest existing ancestor; for the
/// fallback path of a symlink loop, which may still hold `..`, it stops at the loop.
fn canonical_prefix(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let comps: Vec<Component> = path.components().collect();
    let mut cur = PathBuf::new();
    let mut real = None;
    let mut kept = 0;
    for (i, comp) in comps.iter().enumerate() {
        cur.push(comp.as_os_str());
        match std::fs::canonicalize(&cur) {
            Ok(r) => {
                real = Some(r);
                kept = i + 1;
            }
            Err(_) => break,
        }
    }
    let Some(mut out) = real else {
        return path.to_path_buf();
    };
    for comp in &comps[kept..] {
        out.push(comp.as_os_str());
    }
    out
}

/// What an edit of `file` can touch: the file reached through every symlink (a dangling
/// link included), and, when the file is itself a symlink (an operation may replace the
/// link instead of following it), the link inside its resolved directory. Usually both
/// are the same path. `..` is never applied before the symlinks it follows are resolved.
fn edit_targets(file: &Path) -> Vec<PathBuf> {
    let mut out = vec![resolve_existing(file)];
    if let (Some(parent), Some(name)) = (file.parent(), file.file_name()) {
        let link = resolve_existing(parent).join(name);
        if !out.contains(&link) {
            out.push(link);
        }
    }
    out
}

/// Nearest ancestor of `path` (not `path` itself) that contains `.git`.
fn git_root_above(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Reason given on auto-acquired holds.
pub const AUTO_HOLD_REASON: &str = "editing (taken by the edit hook)";

/// The text shown when an edit is stopped.
pub fn deny_message(agent: &str, conflicts: &[HoldConflict]) -> String {
    let Some(first) = conflicts.first() else {
        return String::new();
    };
    let mut out = String::from("Another stopped this edit: ");
    let lines: Vec<String> = conflicts
        .iter()
        .take(3)
        .map(|c| {
            let h = &c.held;
            let kind = if h.exclusive { "exclusive" } else { "shared" };
            let reason = h
                .reason
                .as_deref()
                .map(|r| format!(", \"{r}\""))
                .unwrap_or_default();
            format!(
                "{} is held by {} ({kind} hold on {}:{}{reason}, until {})",
                c.path, h.agent, h.project, h.pattern, h.expires_at
            )
        })
        .collect();
    out.push_str(&truncate(lines.join("; "), 450));
    if conflicts.len() > 3 {
        out.push_str(&format!("; and {} more", conflicts.len() - 3));
    }
    out.push_str(". Worktrees of the same repository still conflict at merge");
    out.push_str(". You'll be pinged in your Another inbox when the file is free. Do not retry this edit while waiting");
    out.push_str(&format!(
        ". Leave it alone until the hold is released. Ask the holder with `another hold request-release --agent {agent} {}`, or work on other files.",
        first.held.hold_id
    ));
    truncate(out, 1000)
}

/// `pre-edit`: hold each file about to be edited, or stop the edit when another agent
/// holds it. Prints a deny only for a real conflict; nothing otherwise (printing "allow"
/// would skip the client's own permission prompts).
fn pre_edit(store: &Store, client: Client, dir: &Path, input: &HookInput) -> Result<String> {
    let paths = input.edit_paths();
    if paths.is_empty() {
        return Ok(String::new());
    }
    let root = project_root(dir);
    let agent = match known_agent(store, client, &root)? {
        Some(a) => a,
        None => hook_agent(store, client, &root)?,
    };
    // Registered projects with their directories resolved, so a project registered under a
    // symlinked spelling (/tmp vs /private/tmp) is still recognised.
    let projects: Vec<(String, PathBuf)> = store
        .projects()?
        .into_iter()
        .map(|p| {
            let real = resolve_existing(Path::new(&p.abs_path));
            (p.abs_path, real)
        })
        .collect();
    // Group files by the project they belong to: the deeper of their git checkout and the
    // closest registered project, judged on the resolved path, so an edit through a symlink
    // is checked against the holds on the file it really reaches. Files in neither (scratch
    // space, /tmp) are not held.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for raw in &paths {
        // Not normalized lexically: `..` must apply after symlinks (review N1).
        for target in edit_targets(&dir.join(raw)) {
            // Some tool envelopes name a working directory rather than an edited file.
            // Never widen an automatic hold to that directory.
            if target.is_dir() || raw.ends_with('/') {
                continue;
            }
            let git = git_root_above(&target);
            let registered = projects
                .iter()
                .filter(|(_, real)| target.starts_with(real) && target != *real)
                .max_by_key(|(_, real)| real.components().count());
            let root = match (git, registered) {
                (Some(g), Some((_, r))) if r.components().count() > g.components().count() => {
                    r.clone()
                }
                (Some(g), _) => g,
                (None, Some((_, r))) => r.clone(),
                (None, None) => continue,
            };
            let Ok(rel) = target.strip_prefix(&root) else {
                continue;
            };
            let rel = rel.to_string_lossy().into_owned();
            // Use the registered spelling of that directory when there is one.
            let key = projects.iter().find(|(_, real)| *real == root).map_or_else(
                || root.to_string_lossy().into_owned(),
                |(abs, _)| abs.clone(),
            );
            match groups.iter_mut().find(|(p, _)| *p == key) {
                Some((_, files)) if files.contains(&rel) => {}
                Some((_, files)) => files.push(rel),
                None => groups.push((key, vec![rel])),
            }
        }
    }
    let mut conflicts = Vec::new();
    let mut first_notice = false;
    let mut retry_after = 0;
    for (project_dir, files) in &groups {
        let project = match store.project(project_dir) {
            Ok(p) => p.abs_path,
            Err(another_core::Error::NotFound(_)) => {
                store.ensure_project(&agent, project_dir)?.abs_path
            }
            Err(e) => return Err(e.into()),
        };
        let guard = store.guard_edit(&agent, &project, files, AUTO_HOLD_REASON)?;
        first_notice |= guard.first_notice;
        retry_after = retry_after.max(guard.retry_after_seconds);
        conflicts.extend(guard.conflicts);
    }
    if conflicts.is_empty() {
        return Ok(String::new());
    }
    Ok(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": if first_notice { deny_message(&agent, &conflicts) } else {
                format!("Another: this edit is still blocked. {} is held by {}. Back off for {retry_after} seconds. Wait for the hold-free inbox ping or work on other files.", truncate(conflicts[0].path.clone(), 200), conflicts[0].held.agent)
            },
        }
    })
    .to_string())
}

/// Run a hook event. Returns the text to print on stdout (possibly empty).
pub fn run(
    store: &Store,
    env: &HookEnv,
    event: HookEvent,
    client: Client,
    input: HookInput,
) -> Result<String> {
    let Some(client) = effective_client(client, env) else {
        return Ok(String::new());
    };
    let dir = match &input.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    match event {
        HookEvent::PostTool => return post_tool(store, env, client, &dir),
        HookEvent::PreEdit => return pre_edit(store, client, &dir, &input),
        _ => {}
    }
    let root = project_root(&dir);
    let agent = hook_agent(store, client, &root)?;
    let info = SessionInfo {
        client: Some(client.as_str().to_string()),
        session_id: input.session_id,
        ..Default::default()
    };
    let text = match event {
        HookEvent::SessionStart => {
            store.session(&agent, SessionPhase::Start, info)?;
            crate::wake::capture_target(store, &agent);
            let digest = session_digest(store, &agent)?;
            // New mail shown here should not be announced again on the next prompt.
            let _ = prompt_notice(store, &agent, "session_start", client);
            if client == Client::Cursor {
                json!({ "additional_context": digest }).to_string()
            } else {
                digest
            }
        }
        HookEvent::Prompt => {
            store.session(&agent, SessionPhase::Beat, info)?;
            if client == Client::Cursor {
                // Cursor's prompt hook cannot add context; just let the prompt through.
                json!({ "continue": true }).to_string()
            } else {
                prompt_notice(store, &agent, "prompt", client)?
                    .map(|n| n.text)
                    .unwrap_or_default()
            }
        }
        HookEvent::SessionEnd => {
            store.session(&agent, SessionPhase::End, info)?;
            crate::wake::release_target(store, &agent);
            String::new()
        }
        HookEvent::PostTool | HookEvent::PreEdit => unreachable!("handled above"),
    };
    Ok(text)
}

/// Largest hook log before it is rotated to `hooks.log.1`.
const MAX_HOOK_LOG: u64 = 256 * 1024;

/// Append one line to `$ANOTHER_HOME/logs/hooks.log` (best effort, size capped).
pub fn log_hook_error(another_home: &Path, event: HookEvent, client: Client, error: &str) {
    use std::io::Write;
    let dir = another_home.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("hooks.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_HOOK_LOG) {
        let _ = std::fs::rename(&path, dir.join("hooks.log.1"));
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let line = format!(
        "{} {} --client {}: {} (allowed; failing open)\n",
        another_core::fmt_ts(ms),
        event.as_str(),
        client.as_str(),
        error.replace('\n', " ")
    );
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Entry point used by the CLI: never fails, so hooks always fail open.
pub fn run_fail_open(
    store: Result<Store>,
    env: Result<HookEnv>,
    another_home: Option<&Path>,
    event: HookEvent,
    client: Client,
) -> String {
    let input = HookInput::read_stdin();
    let attempt = || -> Result<String> { run(&store?, &env?, event, client, input) };
    match attempt() {
        Ok(text) => text,
        Err(e) => {
            let error = format!("{e:#}");
            tracing::warn!(error = %error, "another hook failed; continuing");
            // Tool hooks run silently inside a task; leave a trace for whoever wonders
            // why an edit was not held or a notice did not arrive.
            if event.is_tool_event() {
                if let Some(home) = another_home {
                    log_hook_error(home, event, client, &error);
                }
            }
            match (event, client) {
                (HookEvent::Prompt, Client::Cursor) => json!({ "continue": true }).to_string(),
                _ => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_payloads() {
        let claude = HookInput::parse(
            r#"{"session_id":"a","cwd":"/w/repo","hook_event_name":"SessionStart"}"#,
        );
        assert_eq!(claude.cwd.as_deref(), Some(Path::new("/w/repo")));
        assert_eq!(claude.session_id.as_deref(), Some("a"));
        let grok = HookInput::parse(r#"{"sessionId":"b","workspaceRoot":"/w/g"}"#);
        assert_eq!(grok.cwd.as_deref(), Some(Path::new("/w/g")));
        assert_eq!(grok.session_id.as_deref(), Some("b"));
        let cursor = HookInput::parse(r#"{"conversation_id":"c","workspace_roots":["/w/c"]}"#);
        assert_eq!(cursor.cwd.as_deref(), Some(Path::new("/w/c")));
        assert_eq!(cursor.session_id.as_deref(), Some("c"));
        let junk = HookInput::parse("not json");
        assert!(junk.cwd.is_none() && junk.session_id.is_none());
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("ééééé".into(), 3), "éé…");
        assert_eq!(truncate("abc".into(), 3), "abc");
    }

    #[test]
    fn finds_edited_files_in_every_client_shape() {
        let paths = |raw: &str| HookInput::parse(raw).edit_paths();
        // Claude Edit / Write / MultiEdit, and NotebookEdit.
        assert_eq!(
            paths(r#"{"tool_name":"Edit","tool_input":{"file_path":"/w/a.rs","old_string":"x"}}"#),
            ["/w/a.rs"]
        );
        assert_eq!(
            paths(r#"{"tool_name":"NotebookEdit","tool_input":{"notebook_path":"n.ipynb"}}"#),
            ["n.ipynb"]
        );
        // Grok's camelCase envelope, and its Cursor-preset keys.
        assert_eq!(
            paths(r#"{"toolName":"search_replace","toolInput":{"file_path":"b.rs"}}"#),
            ["b.rs"]
        );
        assert_eq!(
            paths(r#"{"tool_name":"StrReplace","tool_input":{"path":"c.rs"}}"#),
            ["c.rs"]
        );
        assert_eq!(
            paths(r#"{"tool_name":"EditNotebook","tool_input":{"target_notebook":"d.ipynb"}}"#),
            ["d.ipynb"]
        );
        // Codex apply_patch: every file the patch adds, updates, deletes or moves to.
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/new.rs\n+z\n\
                     *** Delete File: old.rs\n*** Update File: src/m.rs\n*** Move to: src/n.rs\n*** End Patch";
        let raw = json!({ "tool_name": "apply_patch", "tool_input": { "command": patch } });
        assert_eq!(
            paths(&raw.to_string()),
            ["src/a.rs", "src/new.rs", "old.rs", "src/m.rs", "src/n.rs"]
        );
        // No path, or text that is not a patch: nothing to hold.
        assert!(paths(r#"{"tool_name":"Edit","tool_input":{}}"#).is_empty());
        assert!(paths(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#).is_empty());
        assert!(paths("not json").is_empty());
    }

    #[test]
    fn deny_message_names_holder_and_next_step() {
        let held = another_core::Hold {
            hold_id: "h1".into(),
            project: "shop".into(),
            agent: "codex@shop".into(),
            pattern: "src/**".into(),
            exclusive: true,
            reason: Some("refactor api".into()),
            ttl_seconds: 1800,
            created_at: "2026-11-10T10:00:00.000Z".into(),
            expires_at: "2026-11-10T10:30:00.000Z".into(),
        };
        let msg = deny_message(
            "claude@shop-wt",
            &[HoldConflict {
                path: "src/a.rs".into(),
                held: held.clone(),
            }],
        );
        assert!(msg.starts_with("Another stopped this edit: src/a.rs is held by codex@shop"));
        assert!(msg.contains("exclusive hold on shop:src/**, \"refactor api\""));
        assert!(msg.contains("until 2026-11-10T10:30:00.000Z"));
        assert!(msg.contains("another hold request-release --agent claude@shop-wt h1"));
        assert!(!msg.contains('\u{2014}') && !msg.contains('\u{2013}'));
        let msg = deny_message(
            "claude@shop",
            &[HoldConflict {
                path: "long/".repeat(300),
                held,
            }],
        );
        assert!(msg.contains("Worktrees of the same repository still conflict at merge"));
    }

    #[test]
    fn every_client_gets_post_tool_and_only_safe_clients_get_the_edit_guard() {
        for client in Client::ALL {
            let specs = client.specs();
            assert!(specs.iter().any(|s| s.event == HookEvent::PostTool));
            let guard = specs.iter().find(|s| s.event == HookEvent::PreEdit);
            match client {
                Client::Cursor => assert!(guard.is_none()),
                Client::Codex => assert_eq!(guard.unwrap().matcher, Some(CODEX_EDIT_TOOLS)),
                _ => assert_eq!(guard.unwrap().matcher, Some(EDIT_TOOLS)),
            }
        }
    }
}
