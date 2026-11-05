//! `another doctor`: a read-mostly health check with a fix hint for every problem.
//!
//! Checks the store (path writable, schema current), the local server (reachable, ours),
//! tokens and their scopes, client hooks (Claude Code, Codex, Grok; and whether Codex has
//! trusted them), and port / resource conflicts. It never changes client configs, Codex's
//! trust state, tokens or resources; the only write is a probe file in the Another home.

use std::path::{Path, PathBuf};
use std::time::Duration;

use another_core::{ResourceFilter, Store};
use serde::Serialize;
use serde_json::Value;

use crate::hooks::{Client, HookEnv, MARKER};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Pass => "PASS",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
            Level::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub area: String,
    pub level: Level,
    pub detail: String,
    /// What to do about a warning or failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub home: String,
    pub checks: Vec<Check>,
    pub failed: usize,
    pub warnings: usize,
}

fn check(area: &str, level: Level, detail: impl Into<String>, fix: Option<String>) -> Check {
    Check {
        area: area.into(),
        level,
        detail: detail.into(),
        fix,
    }
}

/// Where the checks look. `server` is the HTTP base URL of `another serve`.
pub struct DoctorOptions {
    pub another_home: PathBuf,
    pub user_home: Option<PathBuf>,
    pub server: String,
}

pub fn run(opts: &DoctorOptions) -> Report {
    let mut checks = Vec::new();
    let store = check_store(&opts.another_home, &mut checks);
    let node_id = store.as_ref().map(|s| s.node_id().to_string());
    checks.push(check_server(&opts.server, node_id.as_deref()));
    match &store {
        Some(s) => {
            checks.extend(check_tokens(s));
            checks.extend(check_resources(s, &opts.server));
        }
        None => {
            for area in ["tokens", "resources"] {
                checks.push(check(area, Level::Skip, "store unavailable", None));
            }
        }
    }
    checks.extend(check_hooks(opts));
    let failed = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warnings = checks.iter().filter(|c| c.level == Level::Warn).count();
    Report {
        home: opts.another_home.display().to_string(),
        checks,
        failed,
        warnings,
    }
}

fn check_store(home: &Path, out: &mut Vec<Check>) -> Option<Store> {
    if !home.join("another.db").exists() {
        out.push(check(
            "store",
            Level::Fail,
            format!("no database in {}", home.display()),
            Some("run `another init` (or set ANOTHER_HOME to your Another home)".into()),
        ));
        return None;
    }
    let probe = home.join(format!(".doctor-probe-{}", std::process::id()));
    if let Err(e) = std::fs::write(&probe, b"ok") {
        out.push(check(
            "store",
            Level::Fail,
            format!("{} is not writable: {e}", home.display()),
            Some(format!(
                "fix the permissions: `chmod u+rwx {}` (it should be yours, mode 0700)",
                home.display()
            )),
        ));
        return None;
    }
    let _ = std::fs::remove_file(&probe);
    match Store::open(home) {
        Ok(s) => {
            out.push(check(
                "store",
                Level::Pass,
                format!(
                    "{} is writable; schema {}",
                    home.display(),
                    Store::schema_version()
                ),
                None,
            ));
            Some(s)
        }
        Err(e) => {
            out.push(check(
                "store",
                Level::Fail,
                format!("cannot open the database: {e}"),
                Some(
                    "check that another binary of the same version is not using it, then retry; \
                     a newer database needs a newer `another`"
                        .into(),
                ),
            ));
            None
        }
    }
}

fn check_server(base: &str, node_id: Option<&str>) -> Check {
    let url = format!("{}/health", base.trim_end_matches('/'));
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .http_status_as_error(false)
            .proxy(None)
            .build(),
    );
    let start_hint = || {
        Some(
            "start it with `another serve` (MCP clients using `another serve --stdio` don't need it, but hooks' wake and webhooks do)"
                .into(),
        )
    };
    let mut resp = match agent.get(&url).call() {
        Ok(r) => r,
        Err(e) => {
            return check(
                "server",
                Level::Fail,
                format!("{base} is not reachable ({e})"),
                start_hint(),
            )
        }
    };
    let body: Option<Value> = resp
        .body_mut()
        .read_to_string()
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok());
    match body {
        Some(v) if v["ok"] == true => {
            let theirs = v["node_id"].as_str().unwrap_or_default();
            let auth = if v["auth"] == true {
                ", token auth on"
            } else {
                ""
            };
            match node_id {
                Some(mine) if mine != theirs => check(
                    "server",
                    Level::Warn,
                    format!("{base} answers, but for another Another home (node {theirs})"),
                    Some(
                        "point ANOTHER_HOME at the home that server uses, or start one for this home on another port: `another serve --bind 127.0.0.1:7467`"
                            .into(),
                    ),
                ),
                _ => check(
                    "server",
                    Level::Pass,
                    format!("{base} is up{auth}"),
                    None,
                ),
            }
        }
        _ => check(
            "server",
            Level::Fail,
            format!("{base} answers, but it is not Another (port taken by another program)"),
            Some(
                "stop that program, or run Another on a free port: `another serve --bind 127.0.0.1:7467`"
                    .into(),
            ),
        ),
    }
}

fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn check_tokens(store: &Store) -> Vec<Check> {
    let reach = match store.token_reach() {
        Ok(r) => r,
        Err(e) => {
            return vec![check(
                "tokens",
                Level::Fail,
                format!("cannot read tokens: {e}"),
                None,
            )]
        }
    };
    let now = store.now();
    let mut out = Vec::new();
    let mut active = 0;
    for (t, projects) in reach {
        if t.revoked_at.is_some() {
            continue;
        }
        let expires = t.expires_at.as_deref().and_then(parse_ts);
        if expires.is_some_and(|e| e <= now) {
            out.push(check(
                "tokens",
                Level::Warn,
                format!("token {} ({}) has expired", t.token_id, t.agent),
                Some(format!(
                    "revoke it (`another token revoke {}`) and create a new one if still needed",
                    t.token_id
                )),
            ));
            continue;
        }
        active += 1;
        if projects == 0 {
            out.push(check(
                "tokens",
                Level::Warn,
                format!(
                    "token {} ({}) matches no registered project: {}",
                    t.token_id,
                    t.agent,
                    t.project_globs.join(", ")
                ),
                Some(
                    "register the project (`another project ensure PATH`) or create a token with the right --project glob"
                        .into(),
                ),
            ));
        } else if expires.is_some_and(|e| e - now < 72 * 3_600_000) {
            out.push(check(
                "tokens",
                Level::Warn,
                format!("token {} ({}) expires within 3 days", t.token_id, t.agent),
                Some("create a replacement before it lapses: `another token create`".into()),
            ));
        }
    }
    if !out.iter().any(|c| c.level != Level::Pass) {
        out.push(check(
            "tokens",
            Level::Pass,
            if active == 0 {
                "no active tokens (local use only)".to_string()
            } else {
                format!("{active} active token(s), each reaching at least one project")
            },
            None,
        ));
    }
    out
}

fn check_resources(store: &Store, server: &str) -> Vec<Check> {
    let mut out = Vec::new();
    let live = match store.find_resources(ResourceFilter {
        local_only: true,
        ..Default::default()
    }) {
        Ok(r) => r,
        Err(e) => {
            return vec![check(
                "resources",
                Level::Fail,
                format!("cannot read resources: {e}"),
                None,
            )]
        }
    };
    let server_port: Option<u16> = server
        .rsplit(':')
        .next()
        .and_then(|p| p.trim_end_matches('/').parse().ok());
    let mut by_port: std::collections::BTreeMap<u16, Vec<String>> = Default::default();
    for r in live.iter().filter(|r| r.status == "live") {
        if let Some(p) = r.port {
            by_port
                .entry(p)
                .or_default()
                .push(format!("{}:{}", r.kind.as_str(), r.name));
        }
    }
    for (port, claims) in &by_port {
        if claims.len() > 1 {
            out.push(check(
                "ports",
                Level::Fail,
                format!("port {port} is claimed by {}", claims.join(" and ")),
                Some(
                    "keep one: the other owner releases it (`another release KIND:NAME --agent AGENT --stopped` once it is shut down)"
                        .into(),
                ),
            ));
        }
        if Some(*port) == server_port {
            out.push(check(
                "ports",
                Level::Fail,
                format!(
                    "port {port} is Another's server port but is registered for {}",
                    claims.join(", ")
                ),
                Some("move one of them: `another serve --bind 127.0.0.1:<free port>`".into()),
            ));
        }
    }
    if !out.iter().any(|c| c.area == "ports") {
        out.push(check(
            "ports",
            Level::Pass,
            format!("{} claimed port(s), no conflicts", by_port.len()),
            None,
        ));
    }
    match store.lapsed_resources() {
        Ok(l) if !l.is_empty() => out.push(check(
            "resources",
            Level::Warn,
            format!(
                "{} lapsed resource(s) with no live lease: {}",
                l.len(),
                l.iter()
                    .take(5)
                    .map(|r| format!("{}:{}", r.kind.as_str(), r.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(
                "check them with `another orphans`; borrow to adopt one, or have a person reap it"
                    .into(),
            ),
        )),
        Ok(_) => out.push(check(
            "resources",
            Level::Pass,
            format!(
                "{} live resource(s), none lapsed",
                live.iter().filter(|r| r.status == "live").count()
            ),
            None,
        )),
        Err(e) => out.push(check(
            "resources",
            Level::Fail,
            format!("cannot read lapsed resources: {e}"),
            None,
        )),
    }
    out
}

fn client_name(c: Client) -> &'static str {
    match c {
        Client::Claude => "Claude Code",
        Client::Codex => "Codex",
        Client::Grok => "Grok",
        Client::Cursor => "Cursor",
    }
}

fn check_hooks(opts: &DoctorOptions) -> Vec<Check> {
    let Some(home) = opts.user_home.clone().or_else(|| {
        HookEnv::detect(opts.another_home.clone())
            .ok()
            .map(|e| e.home)
    }) else {
        return vec![check(
            "hooks",
            Level::Skip,
            "HOME is not set; can't find client configs",
            None,
        )];
    };
    let mut out = Vec::new();
    for c in [Client::Claude, Client::Codex, Client::Grok] {
        let area = format!("hooks:{}", c.as_str());
        let path = c.config_path(&home);
        let client_dir = path
            .ancestors()
            .find(|a| {
                a.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let repair = Some(format!(
            "repair the file (or restore the `{}.another-backup-*` copy the installer made), then run `another hook install --client {}`",
            path.display(),
            c.as_str()
        ));
        // Only a missing file means not installed. The installer refuses a file it can't
        // read, so sending the person there would go in a circle.
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                out.push(check(
                    &area,
                    Level::Fail,
                    format!(
                        "{} hook config {} can't be read ({e})",
                        client_name(c),
                        path.display()
                    ),
                    repair,
                ));
                continue;
            }
        };
        if !text.contains(MARKER) {
            if !client_dir.exists() {
                out.push(check(
                    &area,
                    Level::Skip,
                    format!(
                        "{} not set up for this user ({} missing)",
                        client_name(c),
                        client_dir.display()
                    ),
                    None,
                ));
            } else {
                out.push(check(
                    &area,
                    Level::Warn,
                    format!(
                        "{} hooks not installed ({})",
                        client_name(c),
                        path.display()
                    ),
                    Some(format!("`another hook install --client {}`", c.as_str())),
                ));
            }
            continue;
        }
        // A config that mentions the wrapper but can't be read as hooks is broken, not
        // installed: the client won't run anything from it.
        let parsed = serde_json::from_str::<Value>(&text);
        let mut commands = Vec::new();
        if let Ok(v) = &parsed {
            our_commands(v, &mut commands);
        }
        if parsed.is_err() || commands.is_empty() {
            let why = match &parsed {
                Err(e) => format!("is not valid JSON ({e})"),
                Ok(_) => "mentions another-hook.sh but no hook command runs it".to_string(),
            };
            out.push(check(
                &area,
                Level::Fail,
                format!("{} hook config {} {why}", client_name(c), path.display()),
                repair,
            ));
            continue;
        }
        // The config naming the wrapper is not enough: every hook runs the wrapper by its
        // absolute path, so it must exist and be executable.
        let broken: Vec<PathBuf> = wrapper_paths(&text)
            .into_iter()
            .filter(|w| !is_executable(w))
            .collect();
        if let Some(w) = broken.first() {
            out.push(check(
                &area,
                Level::Fail,
                format!(
                    "{} hooks point to {}, which is missing or not executable",
                    client_name(c),
                    w.display()
                ),
                Some(format!(
                    "reinstall them (this rewrites the wrapper for the current Another home): `another hook install --client {}`",
                    c.as_str()
                )),
            ));
            continue;
        }
        // Commands doctor can't check are reported, never dropped. Their text stays out of
        // the report: a person's own hook command may hold a secret.
        let unchecked = parsed
            .as_ref()
            .map_or(0, |v| unchecked_commands(v, &commands));
        out.push(if unchecked > 0 {
            check(
                &area,
                Level::Warn,
                format!(
                    "{} hooks installed, but doctor can't check {unchecked} other command(s) that mention {MARKER} ({})",
                    client_name(c),
                    path.display()
                ),
                Some(format!(
                    "in that file, make each command run the wrapper by its absolute path as the first word, inside a hook group's `hooks` list, or remove it; `another hook install --client {}` doesn't rewrite every such command",
                    c.as_str()
                )),
            )
        } else {
            check(
                &area,
                Level::Pass,
                format!("{} hooks installed ({})", client_name(c), path.display()),
                None,
            )
        });
        if c == Client::Codex {
            out.push(codex_trust(&home, &path, &text));
        }
    }
    out
}

/// Installer-shaped hook commands that directly run Another's wrapper.
fn our_commands(v: &Value, out: &mut Vec<String>) {
    if let Some(events) = v.get("hooks").and_then(Value::as_object) {
        for groups in events.values() {
            for group in groups.as_array().into_iter().flatten() {
                for hook in group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if hook.get("type").and_then(Value::as_str) == Some("command") {
                        if let Some(command) = hook.get("command").and_then(Value::as_str) {
                            if wrapper_path(command).is_some() {
                                out.push(command.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Every `command` string anywhere in the JSON that mentions the wrapper.
fn marker_commands<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| marker_commands(i, out)),
        Value::Object(map) => {
            for (key, value) in map {
                match value.as_str() {
                    Some(s) if key == "command" && s.contains(MARKER) => out.push(s),
                    _ => marker_commands(value, out),
                }
            }
        }
        _ => {}
    }
}

/// How many commands mention the wrapper beyond those [`our_commands`] located.
fn unchecked_commands(v: &Value, located: &[String]) -> usize {
    let mut mentions = Vec::new();
    marker_commands(v, &mut mentions);
    for command in located {
        if let Some(i) = mentions.iter().position(|m| m == command) {
            mentions.swap_remove(i);
        }
    }
    mentions.len()
}

/// Decode the installer's `sh_quote` form or a plain absolute first word, without
/// evaluating shell syntax. Concatenated quoted segments preserve literal apostrophes.
fn wrapper_path(command: &str) -> Option<PathBuf> {
    let command = command.trim_start_matches([' ', '\t', '\n']);
    let word = if let Some(mut rest) = command.strip_prefix('\'') {
        let mut word = String::new();
        loop {
            let (part, tail) = rest.split_once('\'')?;
            word.push_str(part);
            if let Some(next) = tail.strip_prefix("\\''") {
                word.push('\'');
                rest = next;
            } else {
                if !tail.is_empty() && !tail.starts_with([' ', '\t', '\n']) {
                    return None;
                }
                break;
            }
        }
        word
    } else {
        let word = command.split([' ', '\t', '\n']).next()?;
        if word.contains([
            '\'', '"', '\\', ';', '&', '|', '<', '>', '(', ')', '$', '`', '*', '?', '[', ']', '{',
            '}',
        ]) {
            return None;
        }
        word.to_string()
    };
    if word.ends_with('/') || word.ends_with("/.") {
        return None;
    }
    let path = PathBuf::from(word);
    (path.is_absolute() && path.ends_with(MARKER)).then_some(path)
}

/// The wrapper paths Another's hook commands run: the first shell word of each command
/// (`'…/another-hook.sh' session-start --client codex`).
fn wrapper_paths(text: &str) -> Vec<PathBuf> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut cmds = Vec::new();
    our_commands(&v, &mut cmds);
    let mut out: Vec<PathBuf> = Vec::new();
    for c in cmds {
        if let Some(p) = wrapper_path(&c) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn is_executable(p: &Path) -> bool {
    match std::fs::metadata(p) {
        #[cfg(unix)]
        Ok(m) => {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        Ok(m) => m.is_file(),
        Err(_) => false,
    }
}

/// `SessionStart` -> `session_start` (how Codex names events in trust keys).
fn snake(event: &str) -> String {
    let mut out = String::new();
    for (i, ch) in event.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The trust keys Codex would use for Another's own hooks in `hooks.json`:
/// `<hooks.json path>:<event>:<group index>:<hook index>`, one per Another hook entry.
fn our_codex_hook_keys(text: &str, hooks_json: &Path) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let path = hooks_json.display().to_string();
    let mut keys = Vec::new();
    if let Some(events) = v.get("hooks").and_then(Value::as_object) {
        for (event, groups) in events {
            for (g, group) in groups.as_array().into_iter().flatten().enumerate() {
                for (h, hook) in group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.contains(MARKER))
                    {
                        keys.push(format!("{path}:{}:{g}:{h}", snake(event)));
                    }
                }
            }
        }
    }
    keys
}

/// Whether Codex has recorded trust for the hooks in `hooks.json`. Codex keeps trust in
/// `~/.codex/config.toml` as `[hooks.state."<hooks.json path>:<event>:…"]` tables with a
/// `trusted_hash`. That layout isn't formally documented, so this only reads it and never
/// writes it; the hash itself can't be checked here.
fn codex_trust(home: &Path, hooks_json: &Path, hooks_text: &str) -> Check {
    let ours = our_codex_hook_keys(hooks_text, hooks_json);
    let config =
        std::fs::read_to_string(home.join(".codex").join("config.toml")).unwrap_or_default();
    // Trusted keys: `[hooks.state."<key>"]` tables that hold a `trusted_hash`.
    let mut trusted: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in config.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            current = l
                .strip_prefix("[hooks.state.\"")
                .and_then(|r| r.strip_suffix("\"]"))
                .map(str::to_string);
            continue;
        }
        if l.starts_with("trusted_hash") {
            if let Some(k) = &current {
                trusted.push(k.clone());
            }
        }
    }
    // Only records for Another's own entries count: other hooks in the same file don't.
    let missing: Vec<&String> = ours.iter().filter(|k| !trusted.contains(k)).collect();
    let fix = Some(
        "open Codex interactively and trust them under `/hooks` (needed again after each `another hook install`); `codex exec` skips untrusted hooks, so until then holds are not enforced there (or run `codex exec --dangerously-bypass-hook-trust`)"
            .to_string(),
    );
    let path = hooks_json.display();
    if ours.is_empty() {
        check(
            "hooks:codex-trust",
            Level::Warn,
            format!("found none of Another's hooks in {path} to check for Codex trust"),
            Some(format!(
                "reinstall them, then trust them under `/hooks`: `another hook install --client {}`",
                Client::Codex.as_str()
            )),
        )
    } else if missing.is_empty() {
        check(
            "hooks:codex-trust",
            Level::Pass,
            format!(
                "Codex trust recorded for all {} of Another's hooks in {path} (the hash itself isn't checked; reinstalling resets it)",
                ours.len()
            ),
            None,
        )
    } else if missing.len() == ours.len() {
        check(
            "hooks:codex-trust",
            Level::Warn,
            format!(
                "no Codex trust recorded for Another's {} hook(s) in {path}: Codex will skip them",
                ours.len()
            ),
            fix,
        )
    } else {
        check(
            "hooks:codex-trust",
            Level::Warn,
            format!(
                "Codex trust recorded for {} of Another's {} hook(s) in {path}; untrusted: {}",
                ours.len() - missing.len(),
                ours.len(),
                missing
                    .iter()
                    .map(|k| k
                        .rsplit_once(&format!("{path}:"))
                        .map_or(k.as_str(), |(_, e)| e))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix,
        )
    }
}

pub fn render(r: &Report) -> String {
    let mut out = format!("another doctor (home {})\n", r.home);
    for c in &r.checks {
        out.push_str(&format!(
            "  {}  {:<18} {}\n",
            c.level.label(),
            c.area,
            c.detail
        ));
        if let Some(f) = &c.fix {
            out.push_str(&format!("        fix: {f}\n"));
        }
    }
    out.push_str(&if r.failed == 0 && r.warnings == 0 {
        "All checks passed.".to_string()
    } else {
        format!("{} failed, {} warning(s).", r.failed, r.warnings)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_trust_with_no_hooks_to_check_is_not_a_pass() {
        let home = tempfile::tempdir().unwrap();
        let hooks = home.path().join(".codex/hooks.json");
        let c = codex_trust(home.path(), &hooks, r#"{"hooks":{}}"#);
        assert!(matches!(c.level, Level::Warn), "{c:?}");
        assert!(!c.detail.contains("for all 0"), "{c:?}");
    }
}
