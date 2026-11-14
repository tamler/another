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
//! and the default `pre-advisory-edit` checks path holds before native edits. These
//! advisory checks fail open on unavailable coverage. The explicit strict pack instead
//! denies native edits and admits only the separate checked-handle tool, failing closed.
//! Hooks open the database without migrating and use a short lock timeout.
//!
//! No hook event creates an Another home, and every run is bounded: see [`run_guarded`].

use std::io::{IsTerminal, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use another_core::{HoldConflict, InboxQuery, Program, Since, Store};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Substring that identifies hook entries Another owns inside client configs.
#[cfg(not(windows))]
pub const MARKER: &str = "another-hook.sh";
#[cfg(windows)]
pub const MARKER: &str = "another-hook.ps1";
/// Hook script and Grok file names used before the project was renamed to Another
/// (see DECISIONS D35). They are recognised only so `hook install` / `hook uninstall`
/// replace or remove entries left by development builds; nothing new is written with them.
const LEGACY_MARKERS: &[&str] = &["wyres-hook.sh"];
const LEGACY_GROK_FILE: &str = "wyres.json";

fn mentions_ours(text: &str) -> bool {
    #[cfg(windows)]
    if let Some((_, encoded)) = text.split_once("-EncodedCommand ") {
        use base64::Engine;
        if let Some(token) = encoded.split_whitespace().next() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(token) {
                if bytes.len() % 2 == 0 {
                    let words: Vec<_> = bytes
                        .chunks_exact(2)
                        .map(|p| u16::from_le_bytes([p[0], p[1]]))
                        .collect();
                    if let Ok(command) = String::from_utf16(&words) {
                        if command.contains(MARKER) {
                            return true;
                        }
                    }
                }
            }
        }
    }
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
    fn specs(self, strict_controlled: bool) -> Vec<HookSpec> {
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
                let mut specs = vec![
                    spec("SessionStart", HookEvent::SessionStart, 10, None),
                    spec("UserPromptSubmit", HookEvent::Prompt, 5, None),
                    spec("SessionEnd", HookEvent::SessionEnd, 3, None),
                    spec("PostToolUse", HookEvent::PostTool, 5, None),
                    spec(
                        "PreToolUse",
                        if strict_controlled {
                            HookEvent::PreEdit
                        } else {
                            HookEvent::PreAdvisoryEdit
                        },
                        5,
                        Some(edits),
                    ),
                ];
                if strict_controlled {
                    specs.push(spec(
                        "PreToolUse",
                        HookEvent::PreControlledEdit,
                        5,
                        Some("^mcp__another__controlled_edit$"),
                    ));
                }
                specs
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
        HookEvent::PreEdit => "Another: refusing native edits in strict mode",
        HookEvent::PreAdvisoryEdit => "Another: checking advisory file holds",
        HookEvent::PreControlledEdit => "Another: admitting a controlled edit",
        HookEvent::SessionEnd => "Another: closing this session",
    }
}

/// Top-level `description` for Codex's hooks.json, set only when the file has none.
const CODEX_DESCRIPTION: &str =
    "Another coordination hooks (toanother.one): messages and file holds between coding agents";

/// Human trust is a Codex decision; finding config entries cannot establish coverage.
/// Codex's `--dangerously-bypass-hook-trust` runs enabled hooks without persisted trust for
/// one invocation, so the note describes the default flow and names the bypass without
/// recommending it.
pub const CODEX_TRUST_NOTE: &str = "Codex trust is not checked. In Codex's default trust flow, new or changed hooks provide no automatic mail or edit-hold coverage until you review and trust them yourself in Codex /hooks. Codex's --dangerously-bypass-hook-trust runs enabled hooks without persisted trust for that one invocation; Another does not recommend it. Another does not grant trust.";

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
    /// Strict pack: refuse native path edits.
    #[value(name = "pre-native-edit")]
    PreEdit,
    /// Advisory pack: check holds; the client performs its own path-based edit.
    #[value(alias = "pre-edit")]
    PreAdvisoryEdit,
    /// Admit only the Another-owned controlled tool; native edits remain denied.
    PreControlledEdit,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::Prompt => "prompt",
            HookEvent::SessionEnd => "session-end",
            HookEvent::PostTool => "post-tool",
            HookEvent::PreEdit => "pre-native-edit",
            HookEvent::PreAdvisoryEdit => "pre-advisory-edit",
            HookEvent::PreControlledEdit => "pre-controlled-edit",
        }
    }

    /// Runs between tool calls inside a task (as opposed to once per session or prompt).
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            HookEvent::PostTool
                | HookEvent::PreEdit
                | HookEvent::PreControlledEdit
                | HookEvent::PreAdvisoryEdit
        )
    }

    /// How long a hook waits for a database lock before giving up (and failing open).
    pub fn busy_timeout(self) -> Duration {
        match self {
            // Runs on every tool call: even a fully locked database must cost under ~100 ms.
            HookEvent::PostTool => Duration::from_millis(50),
            HookEvent::PreEdit | HookEvent::PreControlledEdit | HookEvent::PreAdvisoryEdit => {
                Duration::from_millis(300)
            }
            _ => Duration::from_secs(5),
        }
    }

    /// Wall-clock budget for the whole hook run: reading the envelope, opening and
    /// validating the home, identity, and the event's own work.
    pub fn budget(self) -> Duration {
        if self.is_tool_event() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(5)
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
            .or_else(|| std::env::var_os("USERPROFILE"))
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

#[cfg(not(windows))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(not(windows))]
fn script_text(env: &HookEnv) -> String {
    format!(
        "#!/bin/sh\n\
         # Another hook wrapper, written by `another hook install`; removed by `another hook uninstall`.\n\
         # Native edits are refused before invoking a binary or reading input.\n\
         if [ \"${{1:-}}\" = pre-native-edit ] && [ -n \"${{GROK_HOOK_EVENT:-}}\" ]; then\n\
         \x20 shift\n\
         \x20 set -- pre-advisory-edit \"$@\"\n\
         fi\n\
         if [ \"${{1:-}}\" = pre-native-edit ]; then\n\
         \x20 printf '%s\\n' {edit_denial}\n\
         \x20 exit 0\n\
         fi\n\
         # Other hook events continue when another cannot run.\n\
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
        edit_denial = sh_quote(&deny("Another stopped this edit because native path-based edits cannot use the file handle checked by Another.")),
    )
}

#[cfg(windows)]
fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(windows)]
fn script_text(env: &HookEnv) -> String {
    format!(
        r#"# Another hook wrapper, written by `another hook install`.
$Event = $args[0]
$Client = $args[1]
$Denial = {denial}
if ($Event -eq 'pre-native-edit' -and $env:GROK_HOOK_EVENT) {{ $Event = 'pre-advisory-edit' }}
if ($Event -eq 'pre-native-edit') {{ Write-Output $Denial; exit 0 }}
$ErrorActionPreference = 'Stop'
try {{
    $AnotherBin = {exe}
    if (-not (Test-Path -LiteralPath $AnotherBin -PathType Leaf)) {{
        $AnotherBin = (Get-Command another -ErrorAction Stop).Source
    }}
    if (-not $env:ANOTHER_HOME) {{ $env:ANOTHER_HOME = {home} }}
    $OutputEncoding = [System.Text.UTF8Encoding]::new($false)
    [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
    [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
    $Payload = [Console]::In.ReadToEnd()
    $Reply = $Payload | & $AnotherBin hook run $Event --client $Client 2>$null
    if ($LASTEXITCODE -ne 0) {{ throw 'Another hook failed' }}
    $Reply | Write-Output
}} catch {{
    if ($Event -eq 'pre-controlled-edit') {{ Write-Output $Denial }}
}}
exit 0
"#,
        exe = ps_quote(&env.exe.to_string_lossy()),
        home = ps_quote(&env.another_home.to_string_lossy()),
        denial = ps_quote(&deny(
            "Another stopped this edit because its checked-handle route is unavailable."
        ))
    )
}

fn command_for(env: &HookEnv, client: Client, event: HookEvent) -> String {
    #[cfg(windows)]
    {
        use base64::Engine;
        let command = format!(
            "& {} {} {}",
            ps_quote(&env.script_path().to_string_lossy()),
            ps_quote(event.as_str()),
            ps_quote(client.as_str())
        );
        let bytes: Vec<_> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!(
            "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }
    #[cfg(not(windows))]
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
fn install_into(
    client: Client,
    env: &HookEnv,
    root: &mut Map<String, Value>,
    strict_controlled: bool,
) -> Result<()> {
    // Refresh all our entries once. More than one route can share an event key;
    // refreshing per event would replace the native guard with the controlled hook.
    uninstall_from(client, root);
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
    for spec in client.specs(strict_controlled) {
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
        // Claude / Codex / Grok: each route owns its matcher group.
        groups.push(match spec.matcher {
            Some(m) => json!({ "matcher": m, "hooks": [ours] }),
            None => json!({ "hooks": [ours] }),
        });
    }
    Ok(())
}

/// Remove our entries; drop groups and event lists only when we emptied them.
fn uninstall_from(client: Client, root: &mut Map<String, Value>) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for spec in client.specs(false) {
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
    fn visit(value: &Value) -> bool {
        match value {
            Value::String(text) => mentions_ours(text),
            Value::Array(values) => values.iter().any(visit),
            Value::Object(values) => values.values().any(visit),
            _ => false,
        }
    }
    root.values().any(visit)
}

fn render(root: &Map<String, Value>) -> Result<String> {
    Ok(serde_json::to_string_pretty(root)? + "\n")
}

fn plan_config(
    client: Client,
    env: &HookEnv,
    install: bool,
    strict_controlled: bool,
) -> Result<FileChange> {
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
        install_into(client, env, &mut root, strict_controlled)?;
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

/// Detect owned hooks in JSON, including encoded PowerShell command entries.
pub fn contains_ours_text(text: &str) -> bool {
    serde_json::from_str::<Map<String, Value>>(text)
        .map_or_else(|_| mentions_ours(text), |root| contains_ours(&root))
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
    strict_controlled: bool,
) -> Result<HookReport> {
    if strict_controlled && (!install || clients.iter().any(|client| *client != Client::Claude)) {
        bail!("--strict-controlled is available only when installing the Claude pack; Codex, Grok and Cursor controlled edits are unsupported");
    }
    // Plan everything first so a bad config aborts before anything is written.
    let mut changes = clients
        .iter()
        .map(|c| plan_config(*c, env, install, strict_controlled))
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
        notes.push(if strict_controlled {
            "Strict controlled pack: every native edit is denied, including scratch files and creation. Only Claude Code 2.1.325 is recognized; one existing bounded file per call. Native Windows and integrated-client acceptance remain pending. Reinstall without --strict-controlled to restore advisory hooks."
        } else {
            "Default advisory pack: native edits and creation remain available. Conflicts are denied when checked; unavailable identity/database/wrapper means no coverage. The client reopens paths; no checked-object guarantee."
        }.to_owned());
        if clients.contains(&Client::Codex) {
            notes.push(CODEX_TRUST_NOTE.to_string());
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
#[derive(Default)]
pub struct HookInput {
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
    tool_use_id: Option<ToolUseId>,
    /// Tool events: the tool's name (`Edit`, `apply_patch`, `search_replace`, ...).
    pub tool_name: Option<String>,
    /// Tool events: the tool's arguments.
    pub tool_input: Option<Value>,
}

impl std::fmt::Debug for HookInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookInput")
            .field("cwd", &self.cwd)
            .field("session_present", &self.session_id.is_some())
            .field("tool_use_id", &self.tool_use_id)
            .field("tool_name", &self.tool_name)
            .field("input_present", &self.tool_input.is_some())
            .finish()
    }
}

#[derive(Clone)]
struct ToolUseId(String);

impl std::fmt::Debug for ToolUseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
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
        let mut session_id: Option<String> = None;
        for key in ["session_id", "sessionId", "conversation_id"] {
            if let Some(value) = v.get(key) {
                match value.as_str() {
                    Some(id) if session_id.as_deref().is_none_or(|prior| prior == id) => {
                        session_id = Some(id.to_owned());
                    }
                    _ => {
                        session_id = None;
                        break;
                    }
                }
            }
        }
        let tool_name = s("tool_name").or_else(|| s("toolName"));
        let tool_input = v
            .get("tool_input")
            .or_else(|| v.get("toolInput"))
            .filter(|t| !t.is_null())
            .cloned();
        HookInput {
            cwd,
            session_id,
            tool_use_id: s("tool_use_id")
                .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
                .map(ToolUseId),
            tool_name,
            tool_input,
        }
    }

    fn tool_use_id(&self) -> Option<&str> {
        self.tool_use_id.as_ref().map(|id| id.0.as_str())
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
        }
        out
    }

    /// Read the client's envelope from stdin, giving up at `deadline`. `None` when stdin
    /// was not closed in time: a launcher that keeps the pipe open cannot stall the hook.
    fn read_stdin(deadline: Instant) -> Option<HookInput> {
        if std::io::stdin().is_terminal() {
            return Some(HookInput::default());
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        // The reader may block forever on an open pipe; it is abandoned at the deadline
        // and ends with the process.
        std::thread::spawn(move || {
            let mut raw = String::new();
            let _ = std::io::stdin()
                .lock()
                .take(MAX_STDIN_BYTES)
                .read_to_string(&mut raw);
            let _ = sender.send(raw);
        });
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
            .map(|raw| HookInput::parse(&raw))
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

#[derive(Debug)]
enum SessionIdentityError {
    Environment,
    ClientSessionId,
    ProjectPath,
    Binding,
}

impl std::fmt::Display for SessionIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Environment => "Another cannot identify this session; set a valid nonempty ANOTHER_AGENT and ANOTHER_AGENT_MODE to base or session, or unset both, then relaunch",
            Self::ClientSessionId => "Another cannot identify this session: client session ID is missing, invalid or inconsistent; check client hook configuration and restart the session",
            Self::ProjectPath => "Another cannot identify this session: project path is not valid UTF-8; move the project to a UTF-8 path and restart the session",
            Self::Binding => "Another cannot resolve the session binding; check the session address, base and project, then restart or reconcile the session",
        })
    }
}
impl std::error::Error for SessionIdentityError {}

/// What a hook event claims about its session, validated without touching any store.
struct IdentityRequest<'a> {
    session_id: &'a str,
    /// Explicit actor from the environment and whether it is an exact session address.
    actor: Option<(String, bool)>,
    root: &'a str,
}

impl IdentityRequest<'_> {
    fn actor(&self) -> Option<(&str, bool)> {
        self.actor
            .as_ref()
            .map(|(name, exact)| (name.as_str(), *exact))
    }
}

/// The safety gate every hook route passes before a home is opened: a usable client
/// session ID, consistent provenance variables and a UTF-8 project root.
fn identity_request<'a>(
    root: &'a Path,
    input: &'a HookInput,
) -> std::result::Result<IdentityRequest<'a>, SessionIdentityError> {
    let session_id = input
        .session_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
        .ok_or(SessionIdentityError::ClientSessionId)?;
    let agent = std::env::var_os("ANOTHER_AGENT");
    let mode = std::env::var_os("ANOTHER_AGENT_MODE");
    let actor = match (agent.as_deref(), mode.as_deref()) {
        (None, None) => None,
        (Some(agent), Some(mode)) => {
            let agent = agent
                .to_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or(SessionIdentityError::Environment)?;
            match mode.to_str() {
                Some("base") => Some((agent.to_owned(), false)),
                Some("session") => Some((agent.to_owned(), true)),
                _ => return Err(SessionIdentityError::Environment),
            }
        }
        _ => return Err(SessionIdentityError::Environment),
    };
    let root = root.to_str().ok_or(SessionIdentityError::ProjectPath)?;
    Ok(IdentityRequest {
        session_id,
        actor,
        root,
    })
}

/// Resolve the same actor for all events from a given client session.
fn hook_agent(
    store: &Store,
    client: Client,
    root: &Path,
    input: &HookInput,
    create: bool,
) -> Result<String> {
    let request = identity_request(root, input)?;
    match store.session_actor(
        client.as_str(),
        client.program(),
        request.root,
        request.actor(),
        request.session_id,
        create,
    ) {
        Ok((address, _)) => Ok(address),
        Err(
            another_core::Error::Invalid(_)
            | another_core::Error::Forbidden(_)
            | another_core::Error::NotFound(_)
            | another_core::Error::Ambiguous(_),
        ) => Err(SessionIdentityError::Binding.into()),
        // Budget and availability failures keep their first cause.
        Err(e) => Err(e.into()),
    }
}

/// Inbox + activity digest for session start, capped at [`MAX_DIGEST_CHARS`].
pub fn session_digest(store: &Store, agent: &str) -> Result<String> {
    let inbox = store.inbox_without_sweep(
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
    let summary =
        store.activity_summary_without_sweep(Some(Since::Time(store.now() - 3_600_000)))?;
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

/// `post-tool`: mention mail that arrived mid-task, in the client's context format.
/// Prints nothing when nothing is new or the agent is not registered yet.
fn post_tool(store: &Store, client: Client, dir: &Path, input: &HookInput) -> Result<String> {
    let root = project_root(dir);
    let agent = hook_agent(store, client, &root, input, false)?;
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
    let mut out = String::from("Another stopped this edit: ");
    let mut holders: Vec<&str> = Vec::new();
    let lines: Vec<String> = conflicts
        .iter()
        .take(3)
        .map(|c| {
            let h = &c.held;
            if !holders.contains(&h.agent.as_str()) {
                holders.push(&h.agent);
            }
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
    out.push_str(&lines.join("; "));
    if conflicts.len() > 3 {
        out.push_str(&format!("; and {} more", conflicts.len() - 3));
    }
    let first = conflicts.first().map(|c| c.path.as_str()).unwrap_or("it");
    out.push_str(&format!(
        ". Leave it alone until the hold is released. Ask the holder with `another send --from {agent} --to {} --request --subject \"Release {first}?\"`, or work on other files and try again later.",
        holders.join(",")
    ));
    truncate(out, 1000)
}

/// Native client edits reopen paths outside Another's control, so they cannot use an
/// open file retained by this process. Refuse them until a controlled route exists.
fn pre_edit(store: &Store, _client: Client, dir: &Path, input: &HookInput) -> Result<String> {
    let paths = input.edit_paths();
    if paths.is_empty() {
        return Ok(deny(
            "Another stopped this edit because its file targets could not be established.",
        ));
    }
    if paths.len() > MAX_EDIT_PATHS {
        bail!(
            "one edit names {} files; at most {MAX_EDIT_PATHS} can be protected at once",
            paths.len()
        );
    }
    let scope = group_edit(dir, &paths, &store.projects()?);
    let reason = if scope.groups.is_empty() {
        "Another stopped this edit because its file scope cannot be proven safe for a native path-based editor."
    } else {
        "Another stopped this edit because the native editor cannot use the file handle checked by Another."
    };
    Ok(deny(reason))
}

/// Advisory policy only. The client later reopens paths; no checked-object guarantee.
fn pre_advisory_edit(
    store: &Store,
    client: Client,
    dir: &Path,
    input: &HookInput,
) -> Result<String> {
    let paths = input.edit_paths();
    if paths.is_empty() {
        return Ok(String::new());
    }
    if paths.len() > MAX_EDIT_PATHS {
        bail!("too many advisory edit targets");
    }
    let scope = group_edit(dir, &paths, &store.projects()?);
    if scope.groups.is_empty() {
        return Ok(String::new());
    }
    let agent = hook_agent(store, client, &project_root(dir), input, false)?;
    let mut conflicts = Vec::new();
    for (project, paths) in scope.groups {
        let project = match store.project(&project) {
            Ok(project) => project,
            Err(another_core::Error::NotFound(_)) => store.ensure_project(&agent, &project)?,
            Err(error) => return Err(error.into()),
        };
        conflicts.extend(
            store
                .guard_edit(&agent, &project.abs_path, &paths, AUTO_HOLD_REASON)?
                .conflicts,
        );
    }
    Ok(if conflicts.is_empty() {
        String::new()
    } else {
        deny(&deny_message(&agent, &conflicts))
    })
}

fn pre_controlled_edit(
    store: &Store,
    client: Client,
    dir: &Path,
    input: &HookInput,
) -> Result<String> {
    if client != Client::Claude {
        bail!("controlled edit client is unsupported");
    }
    if input.tool_name.as_deref() != Some("mcp__another__controlled_edit") {
        bail!("unsupported controlled tool name");
    }
    let edit: another_core::ControlledEdit =
        serde_json::from_value(input.tool_input.clone().unwrap_or(Value::Null))
            .map_err(|_| anyhow::anyhow!("invalid controlled edit arguments"))?;
    let root = project_root(dir);
    let request = identity_request(&root, input)?;
    let id = input
        .tool_use_id()
        .context("controlled edit requires a client call ID")?;
    crate::controlled_edit::preflight(&edit)?;
    store.prepare_controlled_edit(
        client.as_str(),
        client.program(),
        request.root,
        request.actor(),
        request.session_id,
        id,
        &edit,
    )?;
    Ok(String::new())
}

/// Most files one edit may name.
const MAX_EDIT_PATHS: usize = 128;

/// The pre-edit response that stops the edit.
fn deny(reason: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Group the files of an edit by the project they belong to, as `(project directory,
/// files relative to it)`, and list every target that belongs to none. No target is
/// dropped: admission decides on all of them. Reads the filesystem only.
fn group_edit(
    dir: &Path,
    paths: &[String],
    registered: &[another_core::Project],
) -> another_core::EditScope {
    // Registered projects with their directories resolved, so a project registered under a
    // symlinked spelling (/tmp vs /private/tmp) is still recognised.
    let projects: Vec<(String, PathBuf)> = registered
        .iter()
        .map(|p| {
            let real = resolve_existing(Path::new(&p.abs_path));
            (p.abs_path.clone(), real)
        })
        .collect();
    // Group files by the project they belong to: the deeper of their git checkout and the
    // closest registered project, judged on the resolved path, so an edit through a symlink
    // is checked against the holds on the file it really reaches. Files in neither (scratch
    // space, /tmp) cannot be held and are reported as unguarded.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut unguarded: Vec<String> = Vec::new();
    let mut outside = |target: &Path| {
        let target = target.to_string_lossy().into_owned();
        if !unguarded.contains(&target) {
            unguarded.push(target);
        }
    };
    for raw in paths {
        // Not normalized lexically: `..` must apply after symlinks (review N1).
        for target in edit_targets(&dir.join(raw)) {
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
                (None, None) => {
                    outside(&target);
                    continue;
                }
            };
            let Ok(rel) = target.strip_prefix(&root) else {
                outside(&target);
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
    another_core::EditScope { groups, unguarded }
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
    run_event(store, event, client, &dir, &input)
}

/// Run one event for the client that acts on it, in the directory it happened in.
fn run_event(
    store: &Store,
    event: HookEvent,
    client: Client,
    dir: &Path,
    input: &HookInput,
) -> Result<String> {
    match event {
        HookEvent::PostTool => return post_tool(store, client, dir, input),
        HookEvent::PreEdit => return pre_edit(store, client, dir, input),
        HookEvent::PreAdvisoryEdit => return pre_advisory_edit(store, client, dir, input),
        HookEvent::PreControlledEdit => return pre_controlled_edit(store, client, dir, input),
        _ => {}
    }
    let root = project_root(dir);
    let agent = hook_agent(
        store,
        client,
        &root,
        input,
        event == HookEvent::SessionStart,
    )?;
    let text = match event {
        HookEvent::SessionStart => {
            let digest = session_digest(store, &agent)?;
            if client == Client::Cursor {
                json!({ "additional_context": digest }).to_string()
            } else {
                digest
            }
        }
        HookEvent::Prompt => {
            if client == Client::Cursor {
                // Cursor's prompt hook cannot add context; just let the prompt through.
                json!({ "continue": true }).to_string()
            } else {
                prompt_notice(store, &agent, "prompt", client)?
                    .map(|n| n.text)
                    .unwrap_or_default()
            }
        }
        HookEvent::SessionEnd => String::new(),
        HookEvent::PostTool
        | HookEvent::PreEdit
        | HookEvent::PreControlledEdit
        | HookEvent::PreAdvisoryEdit => {
            unreachable!("handled above")
        }
    };
    Ok(text)
}

/// Largest hook log before it is rotated to `hooks.log.1`.
const MAX_HOOK_LOG: u64 = 256 * 1024;

/// Append one line to `$ANOTHER_HOME/logs/hooks.log` (best effort, size capped).
///
/// Never creates the home itself: a hook on a machine without one leaves no trace.
pub fn log_hook_error(another_home: &Path, event: HookEvent, client: Client, error: &str) {
    use std::io::Write;
    let dir = another_home.join("logs");
    if !another_home.is_dir() {
        return;
    }
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return,
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

/// An identity or budget diagnostic in the shape `client` shows for `event`. Pre-edit
/// stops the edit; routes without a verified context channel stay silent.
fn refusal(event: HookEvent, client: Client, message: &str) -> String {
    match (event, client) {
        (HookEvent::PreEdit | HookEvent::PreControlledEdit, _) => deny(message),
        (HookEvent::PreAdvisoryEdit, _) => String::new(),
        (HookEvent::SessionStart | HookEvent::PostTool, Client::Cursor) => {
            json!({"additional_context": message}).to_string()
        }
        (HookEvent::SessionStart, _)
        | (HookEvent::Prompt, Client::Claude | Client::Codex | Client::Grok) => message.to_owned(),
        (HookEvent::PostTool, _) => json!({"hookSpecificOutput": {
            "hookEventName": "PostToolUse", "additionalContext": message
        }})
        .to_string(),
        (HookEvent::Prompt, Client::Cursor) | (HookEvent::SessionEnd, _) => String::new(),
    }
}

/// Open the home a hook event acts on. Never creates or migrates one: a hook on a machine
/// without an initialized home is refused, whatever the event.
fn open_home(home: &Path, event: HookEvent, deadline: Instant) -> Result<Store> {
    if std::fs::symlink_metadata(home.join(another_core::DB_FILE)).is_err() {
        return Err(another_core::Error::IdentityUnavailable(
            "there is no Another database in the home directory; run `another init`, then restart the session"
                .into(),
        )
        .into());
    }
    Ok(Store::open_existing(home, event.busy_timeout(), deadline)?)
}

/// Entry point used by the CLI. It never fails, never creates an Another home, and
/// finishes within [`HookEvent::budget`].
///
/// Every event passes the same gates, in order, before its route runs:
///
/// 1. the client's envelope arrives complete before the deadline;
/// 2. a duplicate route stands down;
/// 3. the session claims are valid on their own (session ID, provenance, project path);
/// 4. an initialized home of the current schema opens, bounded by the same deadline.
///
/// `pre-edit` fails closed: whenever the edit cannot be protected it is stopped. The
/// passive events mutate nothing they cannot verify; an identity failure is reported
/// where the client shows it, and any other failure prints nothing.
pub fn run_guarded(another_home: Option<&Path>, event: HookEvent, client: Client) -> String {
    let deadline = Instant::now() + event.budget();
    let Some(input) = HookInput::read_stdin(deadline) else {
        return refusal(
            event,
            client,
            "Another did not receive the complete hook input in time; this event was not processed",
        );
    };
    // The client that acts, once known: a refusal is shaped for it.
    let mut acting = client;
    // Whether a valid home was opened: only then may a failure be logged into it.
    let mut opened = false;
    let outcome = (|| -> Result<String> {
        let home = another_home.context("no Another home directory")?;
        let env = HookEnv::detect(home.to_path_buf())?;
        let Some(client) = effective_client(client, &env) else {
            return Ok(String::new());
        };
        acting = client;
        let dir = match &input.cwd {
            Some(d) => d.clone(),
            None => std::env::current_dir()?,
        };
        identity_request(&project_root(&dir), &input)?;
        let store = open_home(home, event, deadline)?;
        opened = true;
        run_event(&store, event, client, &dir, &input)
    })();
    let error = match outcome {
        Ok(text) => return text,
        Err(error) => error,
    };
    let typed = error.downcast_ref::<SessionIdentityError>().is_some()
        || matches!(
            error.downcast_ref::<another_core::Error>(),
            Some(
                another_core::Error::IdentityScanBudgetExceeded(_)
                    | another_core::Error::IdentityUnavailable(_)
            )
        );
    if matches!(event, HookEvent::PreEdit | HookEvent::PreControlledEdit) {
        let message = if typed {
            error.to_string()
        } else {
            format!("Another stopped this edit because it could not be protected: {error:#}")
        };
        // The reason reaches the agent in the denial itself; a refusal writes nothing.
        return deny(&truncate(message, 1000));
    }
    // Post-tool runs on every tool call. A session that has not started here, or a home
    // that is missing, busy or over budget, is not news there: session start, each prompt
    // and every edit already report it. A misconfigured launcher is still reported.
    let quiet = event == HookEvent::PostTool
        && !matches!(
            error.downcast_ref::<SessionIdentityError>(),
            Some(
                SessionIdentityError::Environment
                    | SessionIdentityError::ClientSessionId
                    | SessionIdentityError::ProjectPath
            )
        );
    if typed && !quiet {
        return refusal(event, acting, &error.to_string());
    }
    let error = format!("{error:#}");
    tracing::warn!(error = %error, "another hook failed; continuing");
    // The post-tool hook runs silently inside a task; leave a trace for whoever wonders
    // why a notice did not arrive. A home that could not be opened is left untouched.
    if event.is_tool_event() && opened {
        if let Some(home) = another_home {
            log_hook_error(home, event, acting, &error);
        }
    }
    match (event, acting) {
        (HookEvent::Prompt, Client::Cursor) => json!({ "continue": true }).to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pre_edit_wrapper_denies_even_if_another_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let env = HookEnv {
            home: temp.path().to_path_buf(),
            another_home: temp.path().join("home"),
            exe: temp.path().join("missing-another"),
        };
        let script = temp.path().join("wrapper.sh");
        std::fs::write(&script, script_text(&env)).unwrap();
        for client in ["claude", "codex", "grok"] {
            let output = std::process::Command::new("/bin/sh")
                .arg(&script)
                .args(["pre-native-edit", "--client", client])
                .env_clear()
                .env("PATH", temp.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            let value: Value = serde_json::from_slice(&output.stdout)
                .expect("wrapper must deny without the binary");
            assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
        }
        use std::os::unix::fs::PermissionsExt;
        // Grok also loads Claude configs. Its native calls use advisory policy.
        let grok = std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(["pre-native-edit", "--client", "claude"])
            .env_clear()
            .env("PATH", temp.path())
            .env("GROK_HOOK_EVENT", "PreToolUse")
            .output()
            .unwrap();
        assert!(grok.status.success());
        assert!(grok.stdout.is_empty());
        let marker = temp.path().join("executed");
        std::fs::write(
            &env.exe,
            format!(
                "#!/bin/sh\nprintf called > {}\nexit 1\n",
                sh_quote(&marker.to_string_lossy())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&env.exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(["pre-native-edit", "--client", "claude"])
            .env_clear()
            .env("PATH", temp.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            !marker.exists(),
            "pre-edit must not depend on the executable"
        );
    }

    #[test]
    fn tool_use_id_is_bounded_top_level_and_redacted() {
        let input = HookInput::parse(
            r#"{"tool_use_id":"private-call-42","hook_event_name":"PostToolUse"}"#,
        );
        assert_eq!(input.tool_use_id(), Some("private-call-42"));
        assert!(!format!("{input:?}").contains("private-call-42"));
        for raw in [
            json!({"tool_use_id":""}),
            json!({"tool_use_id":"x\ny"}),
            json!({"tool_use_id":"x".repeat(513)}),
            json!({"tool_input":{"tool_use_id":"nested"}}),
        ] {
            assert_eq!(HookInput::parse(&raw.to_string()).tool_use_id(), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_identity_root_is_rejected_before_resolution() {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        let root = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
        let input = HookInput {
            session_id: Some("fixture-session".into()),
            ..HookInput::default()
        };
        let error = hook_agent(&store, Client::Claude, &root, &input, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("project path") && error.contains("UTF-8"),
            "{error}"
        );
        assert!(!error.contains("fixture-session"));
        assert!(store.projects().unwrap().is_empty());
    }

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
    #[cfg(not(windows))]
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
    fn recognises_owned_commands_inside_serialized_config() {
        let env = HookEnv {
            home: PathBuf::from("/test/profile"),
            another_home: PathBuf::from("/test/data"),
            exe: PathBuf::from("/test/another"),
        };
        let mut root = Map::new();
        install_into(Client::Grok, &env, &mut root, false).unwrap();
        assert!(contains_ours(&root));
        assert!(contains_ours_text(&render(&root).unwrap()));
        uninstall_from(Client::Grok, &mut root);
        assert!(!contains_ours(&root));
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
                held,
            }],
        );
        assert!(msg.starts_with("Another stopped this edit: src/a.rs is held by codex@shop"));
        assert!(msg.contains("exclusive hold on shop:src/**, \"refactor api\""));
        assert!(msg.contains("until 2026-11-10T10:30:00.000Z"));
        assert!(msg.contains(
            "another send --from claude@shop-wt --to codex@shop --request --subject \"Release src/a.rs?\""
        ));
        assert!(!msg.contains('\u{2014}') && !msg.contains('\u{2013}'));
    }

    #[test]
    fn every_client_gets_post_tool_and_only_safe_clients_get_the_edit_guard() {
        for client in Client::ALL {
            let specs = client.specs(false);
            assert!(specs.iter().any(|s| s.event == HookEvent::PostTool));
            let guard = specs.iter().find(|s| s.event == HookEvent::PreAdvisoryEdit);
            match client {
                Client::Cursor => assert!(guard.is_none()),
                Client::Codex => assert_eq!(guard.unwrap().matcher, Some(CODEX_EDIT_TOOLS)),
                _ => assert_eq!(guard.unwrap().matcher, Some(EDIT_TOOLS)),
            }
        }
    }
}
