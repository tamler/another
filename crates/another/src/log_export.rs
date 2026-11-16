//! `another log export`: the event log or the audit log as JSON Lines on stdout.
//!
//! Rows are read in pages keyed on `seq` (or `audit_id`) and written as they are read, so
//! memory stays flat and no lock is held across the export. The last line on stderr names
//! the cursor (`--since <event id>`) that continues the export without gaps or repeats.
//! Opening reads only: no create, no migration, and the export writes no event.

use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::Duration;

use another_core::{ExportFilter, Since, Store};
use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Rows per read.
const PAGE: u32 = 1_000;

/// What to export.
#[derive(Debug, Clone, Default)]
pub struct ExportArgs {
    pub since: Option<String>,
    pub until: Option<String>,
    pub kinds: Vec<String>,
    /// Peers' events stored here as well as this node's own.
    pub all_nodes: bool,
    /// Keep message bodies in peers' events (redacted by default, as on the dashboard, D41).
    pub include_foreign_bodies: bool,
    pub audit: bool,
    pub limit: Option<u64>,
}

/// A peer's message body, replaced by its size unless asked for (D41: bodies of messages
/// from other nodes stay redacted outside their own node's tools).
fn redact_foreign(line: &mut Value, local_node: &str) {
    if line["node_id"].as_str() == Some(local_node) {
        return;
    }
    let Some(payload) = line.get_mut("payload").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(body) = payload.get("body").and_then(Value::as_str) {
        let bytes = body.len();
        payload.insert("body".into(), Value::Null);
        payload.insert("body_redacted_bytes".into(), Value::from(bytes));
    }
}

/// Where an export starts: after a key, and/or from a time.
#[derive(Debug, PartialEq, Eq)]
struct Start {
    after: i64,
    since_ms: Option<i64>,
}

fn parse_until(raw: Option<&str>) -> Result<Option<i64>> {
    raw.map(|u| {
        another_core::parse_ts(u)
            .with_context(|| format!("invalid --until {u:?}: use an RFC 3339 time"))
    })
    .transpose()
}

fn start(store: &Store, args: &ExportArgs) -> Result<Start> {
    let Some(raw) = args.since.as_deref() else {
        return Ok(Start {
            after: 0,
            since_ms: None,
        });
    };
    if args.audit {
        // Audit rows have numeric ids; an event id means nothing there.
        if let Ok(id) = raw.trim().parse::<i64>() {
            return Ok(Start {
                after: id,
                since_ms: None,
            });
        }
    }
    Ok(match Since::parse(raw, store.now())? {
        Since::Time(ms) => Start {
            after: 0,
            since_ms: Some(ms),
        },
        Since::After(_) if args.audit => {
            bail!("--since with --audit takes a duration, an RFC 3339 time or an audit id")
        }
        Since::After(id) => Start {
            after: store.event_seq(&id)?,
            since_ms: None,
        },
    })
}

/// Run the export into `out`; returns the number of lines and the last key written
/// (event id, or audit id). A closed `out` (as with `| head`) ends the export quietly.
pub fn export(
    store: &Store,
    args: &ExportArgs,
    out: &mut impl Write,
) -> Result<(u64, Option<String>)> {
    let Start {
        mut after,
        since_ms,
    } = start(store, args)?;
    let until_ms = parse_until(args.until.as_deref())?;
    let filter = ExportFilter {
        since_ms,
        until_ms,
        kinds: args.kinds.clone(),
        local_only: !args.all_nodes,
    };
    let mut written = 0u64;
    let mut last: Option<String> = None;
    loop {
        let want = match args.limit {
            Some(l) if written >= l => break,
            Some(l) => u32::try_from((l - written).min(u64::from(PAGE))).unwrap_or(PAGE),
            None => PAGE,
        };
        let page = if args.audit {
            store.export_audit(after, since_ms, until_ms, want)?
        } else {
            store.export_events(after, &filter, want)?
        };
        if page.is_empty() {
            break;
        }
        for mut line in page.iter().cloned() {
            let key = if args.audit { "audit_id" } else { "seq" };
            after = line[key].as_i64().unwrap_or(after);
            if !args.audit && !args.include_foreign_bodies {
                redact_foreign(&mut line, store.node_id());
            }
            let text = serde_json::to_string(&line)?;
            if let Err(e) = writeln!(out, "{text}") {
                if e.kind() == ErrorKind::BrokenPipe {
                    return Ok((written, last));
                }
                return Err(e.into());
            }
            written += 1;
            last = Some(match line.get("event_id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => after.to_string(),
            });
        }
        if (page.len() as u32) < want {
            break;
        }
    }
    match out.flush() {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
        r => r?,
    }
    Ok((written, last))
}

/// The CLI entry: open read-only (no create, no migration), export to stdout, and print
/// the continuation cursor on stderr.
pub fn run(home: &Path, args: &ExportArgs) -> Result<()> {
    let store = Store::open_quick(home, Duration::from_secs(5))?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let (n, last) = export(&store, args, &mut out)?;
    let what = if args.audit {
        "audit row(s)"
    } else {
        "event(s)"
    };
    match last {
        Some(key) if args.audit => eprintln!("another: exported {n} {what}, last audit id {key}"),
        Some(key) => eprintln!("another: exported {n} {what}, last event_id {key}"),
        None => eprintln!("another: exported 0 {what}"),
    }
    // A misspelt kind looks like an empty log; say which kinds this log has never held.
    if n == 0 && !args.audit {
        let unknown: Vec<&str> = args
            .kinds
            .iter()
            .filter(|k| {
                let any = ExportFilter {
                    kinds: vec![(*k).clone()],
                    ..ExportFilter::default()
                };
                store.export_events(0, &any, 1).is_ok_and(|p| p.is_empty())
            })
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "another: this log has no event of kind {}; `another events` shows recent kinds",
                unknown.join(", ")
            );
        }
    }
    Ok(())
}
