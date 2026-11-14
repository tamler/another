//! Orphan listing and human-approved reaping.
//!
//! Agents (MCP) can only list orphans and *request* a reap; they never terminate anything.
//! A reap runs only from the CLI with `--confirm`, only after a grace period, only for
//! processes owned by the current user, and never for system processes.

use another_core::{Resource, ResourceFilter, ResourceRef, Store};
use serde::Serialize;

use crate::scan::{self, Listener, ScanResult};

/// Default minimum time an orphan must have been orphaned (or alive) before a reap.
pub const DEFAULT_GRACE_SECONDS: i64 = 10 * 60;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Orphan {
    /// `lapsed_lease` (registered, no live lease) or `unregistered_listener` (scan only).
    pub source: String,
    /// What to pass to `another reap`.
    pub target: String,
    pub resource: Option<Resource>,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub command: Option<String>,
    /// Seconds since the lease lapsed, or the process age for listeners.
    pub age_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct OrphanReport {
    pub orphans: Vec<Orphan>,
    /// Listener scan details (omitted when scanning was skipped).
    pub scan: Option<ScanResult>,
}

/// Registered resources without a live lease, plus (optionally) listening processes that no
/// registered resource accounts for.
pub fn orphans(store: &Store, scan_listeners: bool) -> another_core::Result<OrphanReport> {
    let registered = store.find_resources(ResourceFilter::default())?;
    let mut orphans = Vec::new();
    for r in registered.iter().filter(|r| r.status == "lapsed") {
        orphans.push(Orphan {
            source: "lapsed_lease".into(),
            target: r.resource_id.clone(),
            resource: Some(r.clone()),
            pid: r.pid,
            port: r.port,
            command: r.pid.and_then(scan::process_info).map(|p| p.command),
            age_seconds: store.lapsed_for_ms(&r.resource_id)?.map(|ms| ms / 1000),
        });
    }
    let scan = scan_listeners.then(scan::scan_listeners);
    if let Some(result) = &scan {
        let me = std::process::id();
        for l in &result.listeners {
            let known = registered
                .iter()
                .any(|r| r.pid == Some(l.pid) || (r.port.is_some() && r.port == Some(l.port)));
            if known || l.pid == me || scan::is_system(&l.command) {
                continue;
            }
            let info = scan::process_info(l.pid);
            orphans.push(Orphan {
                source: "unregistered_listener".into(),
                target: format!("pid:{}", l.pid),
                resource: None,
                pid: Some(l.pid),
                port: Some(l.port),
                command: Some(
                    info.as_ref()
                        .map_or(l.command.clone(), |i| i.command.clone()),
                ),
                age_seconds: info.map(|i| i.elapsed_seconds),
            });
        }
    }
    Ok(OrphanReport { orphans, scan })
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReapPlan {
    pub target: String,
    pub resource_id: Option<String>,
    pub pid: Option<u32>,
    pub command: Option<String>,
    pub age_seconds: Option<i64>,
    pub grace_seconds: i64,
    /// True when every safety check passes; a human still has to confirm.
    pub eligible: bool,
    /// Why the reap cannot run (empty when eligible).
    pub blockers: Vec<String>,
    pub executed: bool,
    pub outcome: Option<String>,
    #[cfg(windows)]
    #[serde(skip)]
    #[schemars(skip)]
    windows_identity: Option<scan::WindowsProcessIdentity>,
}

fn listener_for(pid: u32, listeners: &[Listener]) -> Option<&Listener> {
    listeners.iter().find(|l| l.pid == pid)
}

/// Work out what a reap of `target` (a resource id, `kind:name`, or `pid:N`) would do.
pub fn plan(store: &Store, target: &str, grace_seconds: i64) -> anyhow::Result<ReapPlan> {
    let mut plan = ReapPlan {
        target: target.to_string(),
        resource_id: None,
        pid: None,
        command: None,
        age_seconds: None,
        grace_seconds,
        eligible: false,
        blockers: Vec::new(),
        executed: false,
        outcome: None,
        #[cfg(windows)]
        windows_identity: None,
    };
    if grace_seconds < 0 {
        anyhow::bail!("grace must not be negative");
    }
    if let Some(raw) = target.strip_prefix("pid:") {
        let pid: u32 = raw
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid pid {raw:?}"))?;
        plan.pid = Some(pid);
        let scan = scan::scan_listeners();
        let registered = store.find_resources(ResourceFilter::default())?;
        match listener_for(pid, &scan.listeners) {
            None => plan.blockers.push(format!(
                "pid {pid} is not a listening process found by {} (only unregistered listeners can be reaped by pid; registered resources are reaped by id)",
                scan.scanner
            )),
            Some(l) => {
                if registered
                    .iter()
                    .any(|r| r.pid == Some(pid) || r.port == Some(l.port))
                {
                    plan.blockers.push(format!(
                        "pid {pid} belongs to a registered resource; reap that resource by id"
                    ));
                }
            }
        }
        match scan::process_info(pid) {
            Some(info) => {
                plan.age_seconds = Some(info.elapsed_seconds);
                plan.command = Some(info.command);
            }
            None => plan.blockers.push(format!("pid {pid} is not running")),
        }
    } else {
        let resource = store.resource(&ResourceRef::parse(target)?)?;
        plan.resource_id = Some(resource.resource_id.clone());
        if let Some(node) = &resource.node {
            plan.blockers.push(format!(
                "{} belongs to node {node}; reaping happens only on the owning node",
                resource.name
            ));
            plan.eligible = false;
            return Ok(plan);
        }
        plan.pid = resource.pid;
        if resource.status != "lapsed" {
            plan.blockers.push(format!(
                "{} {} is {} (leases: {}); only resources with no live lease can be reaped",
                resource.kind.as_str(),
                resource.name,
                resource.status,
                resource
                    .leases
                    .iter()
                    .map(|l| l.agent.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        plan.age_seconds = store
            .lapsed_for_ms(&resource.resource_id)?
            .map(|ms| ms / 1000);
        match resource.pid {
            None => plan.blockers.push(
                "no pid is registered for this resource; stop it by hand, then `another release --stopped`"
                    .into(),
            ),
            Some(pid) => match scan::process_info(pid) {
                Some(info) => plan.command = Some(info.command),
                None => plan.blockers.push(format!(
                    "pid {pid} is not running; mark it gone with `another release {} --stopped`",
                    resource.resource_id
                )),
            },
        }
    }
    if plan.age_seconds.unwrap_or(0) < grace_seconds {
        plan.blockers.push(format!(
            "orphaned for {}s, grace period is {grace_seconds}s",
            plan.age_seconds.unwrap_or(0)
        ));
    }
    if let Some(pid) = plan.pid {
        if pid <= 1 || pid == std::process::id() {
            plan.blockers.push(format!("refusing to signal pid {pid}"));
        }
        if let Some(info) = scan::process_info(pid) {
            if scan::is_system(&info.command) {
                plan.blockers
                    .push(format!("{} is a protected system process", info.command));
            }
            match scan::current_user() {
                Some(me) if me == info.user => {}
                _ => plan.blockers.push(format!(
                    "pid {pid} is owned by {}, not the current user",
                    info.user
                )),
            }
        }
    }
    #[cfg(windows)]
    if let Some(pid) = plan.pid {
        match scan::windows_process_identity(pid) {
            Some(identity)
                if scan::current_user().as_deref() == Some(identity.owner.as_str())
                    && !scan::is_system(&identity.image) =>
            {
                plan.windows_identity = Some(identity)
            }
            _ => plan
                .blockers
                .push("process identity or ownership could not be verified".into()),
        }
    }
    plan.eligible = plan.blockers.is_empty();
    Ok(plan)
}

/// Stop the verified process on Windows. This is a hard termination.
#[cfg(windows)]
pub fn execute(store: &Store, mut plan: ReapPlan, _force: bool) -> anyhow::Result<ReapPlan> {
    if !plan.eligible {
        anyhow::bail!("not eligible for reaping: {}", plan.blockers.join("; "));
    }
    let pid = plan
        .pid
        .ok_or_else(|| anyhow::anyhow!("no pid to terminate"))?;
    let identity = plan
        .windows_identity
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("process identity was not verified"))?;
    scan::windows_terminate_exact(pid, identity)?;
    let detail = format!("pid {pid} {}", plan.command.as_deref().unwrap_or_default());
    store.record_reap(
        "human (another reap --confirm)",
        plan.resource_id.as_deref(),
        Some(pid),
        detail.trim(),
    )?;
    plan.executed = true;
    plan.outcome = Some(format!("pid {pid} terminated (Windows hard stop)"));
    Ok(plan)
}

/// Terminate the planned process (SIGTERM, then SIGKILL only with `force`). Callers must
/// only invoke this on explicit human confirmation.
#[cfg(not(windows))]
pub fn execute(store: &Store, mut plan: ReapPlan, force: bool) -> anyhow::Result<ReapPlan> {
    #[cfg(target_os = "macos")]
    let _ = force;
    if !plan.eligible {
        anyhow::bail!("not eligible for reaping: {}", plan.blockers.join("; "));
    }
    let pid = plan
        .pid
        .ok_or_else(|| anyhow::anyhow!("no pid to signal"))?;
    let signal = |sig: &str| {
        std::process::Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    };
    if !signal("-TERM") && scan::alive(pid) {
        anyhow::bail!("could not signal pid {pid}");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while scan::alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    #[cfg(target_os = "macos")]
    let how = "terminated (SIGTERM)";
    #[cfg(not(target_os = "macos"))]
    let mut how = "terminated (SIGTERM)";
    if scan::alive(pid) {
        #[cfg(target_os = "macos")]
        {
            plan.outcome = Some(format!(
                "pid {pid} is still running after SIGTERM; stop it manually"
            ));
            return Ok(plan);
        }
        #[cfg(not(target_os = "macos"))]
        {
            if !force {
                plan.outcome = Some(format!(
                    "pid {pid} is still running after SIGTERM; re-run with --force to send SIGKILL"
                ));
                return Ok(plan);
            }
            signal("-KILL");
            how = "killed (SIGKILL)";
        }
    }
    let detail = format!("pid {pid} {}", plan.command.as_deref().unwrap_or_default());
    store.record_reap(
        "human (another reap --confirm)",
        plan.resource_id.as_deref(),
        Some(pid),
        detail.trim(),
    )?;
    plan.executed = true;
    plan.outcome = Some(format!("pid {pid} {how}"));
    Ok(plan)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn termination_without_verified_identity_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::open(home.path()).unwrap();
        let plan = ReapPlan {
            target: "pid:123".into(),
            resource_id: None,
            pid: Some(123),
            command: None,
            age_seconds: Some(100),
            grace_seconds: 0,
            eligible: true,
            blockers: vec![],
            executed: false,
            outcome: None,
            windows_identity: None,
        };
        assert!(execute(&store, plan, false).is_err());
    }
}
