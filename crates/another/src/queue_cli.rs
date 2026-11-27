//! `another queue`: the work queue on the command line.
//!
//! An agent acts with `--agent` (or `$ANOTHER_AGENT`). A delegate adds its key in
//! `$ANOTHER_ADMIT_KEY`, read only from that variable. The owner acts with `--owner`, which
//! needs a terminal on standard input and standard error and a typed yes; no flag, variable
//! or config skips that.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use another_core::{
    AdmitItem, DoneItem, EditItem, Error, PointerKind, ProposeItem, QueueCaller, QueueFilter,
    QueueGrace, QueueItem, QueueKind, QueueLimits, QueuePriority, QueueState, ReleaseItem, Since,
    Store,
};
use clap::{Args, Subcommand};

use crate::cli::{body_arg, emit, parse_secs, EXIT_CONFLICT};
use crate::output;
use crate::templates::UsageError;

/// Who acts on the queue: an agent, or the owner in person.
#[derive(Debug, Args)]
pub struct QueueWho {
    /// Acting agent: name or name@project (default: $ANOTHER_AGENT).
    #[arg(long, short = 'a', env = "ANOTHER_AGENT")]
    agent: Option<String>,
    /// Act as the owner. Needs a terminal and a typed yes; ignores --agent.
    #[arg(long)]
    owner: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum QueueKindArg {
    Work,
    Security,
    DataLoss,
}

impl From<QueueKindArg> for QueueKind {
    fn from(kind: QueueKindArg) -> QueueKind {
        match kind {
            QueueKindArg::Work => QueueKind::Work,
            QueueKindArg::Security => QueueKind::Security,
            QueueKindArg::DataLoss => QueueKind::DataLoss,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum QueuePriorityArg {
    High,
    Normal,
}

impl From<QueuePriorityArg> for QueuePriority {
    fn from(priority: QueuePriorityArg) -> QueuePriority {
        match priority {
            QueuePriorityArg::High => QueuePriority::High,
            QueuePriorityArg::Normal => QueuePriority::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum QueueStateArg {
    Proposed,
    Ready,
    Claimed,
    Done,
}

impl From<QueueStateArg> for QueueState {
    fn from(state: QueueStateArg) -> QueueState {
        match state {
            QueueStateArg::Proposed => QueueState::Proposed,
            QueueStateArg::Ready => QueueState::Ready,
            QueueStateArg::Claimed => QueueState::Claimed,
            QueueStateArg::Done => QueueState::Done,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum QueueCmd {
    /// Propose an item. Anyone may; it waits for the owner or a delegate to admit it.
    Propose {
        #[command(flatten)]
        who: QueueWho,
        /// One line, up to 200 characters.
        #[arg(long)]
        title: String,
        /// Details; `-` reads stdin.
        #[arg(long, default_value = "")]
        body: String,
        /// The scope id the work belongs to, for example M1.
        #[arg(long)]
        scope: Option<String>,
        /// The done condition: what must be true for the item to be finished.
        #[arg(long)]
        done_when: Option<String>,
        /// security and data-loss items may be admitted by a keyed delegate;
        /// they are listed for the owner afterwards.
        #[arg(long, value_enum, default_value = "work")]
        kind: QueueKindArg,
    },
    /// List items: ready and claimed ones unless a filter says otherwise. Writes nothing.
    List {
        /// The agent for --mine (default: $ANOTHER_AGENT).
        #[arg(long, short = 'a', env = "ANOTHER_AGENT")]
        agent: Option<String>,
        #[arg(long, value_enum)]
        state: Option<QueueStateArg>,
        /// Only items with this scope id.
        #[arg(long)]
        scope: Option<String>,
        /// Items the agent proposed or holds.
        #[arg(long)]
        mine: bool,
        /// Every item a delegate admitted for itself, in any state.
        #[arg(long)]
        self_admitted: bool,
        /// What needs the owner: decisions waiting, quiet claims, ready items nobody can
        /// take, failed notices, security and data-loss items a delegate admitted, and
        /// self-admitted items.
        #[arg(
            long,
            conflicts_with_all = ["state", "scope", "mine", "self_admitted"]
        )]
        needs_owner: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// Show one item with every field, who admitted it and when, and its history.
    Show {
        /// The item: its number (Q12) or its id.
        item: String,
    },
    /// Admit a proposed item: it becomes ready. The owner (--owner) or a delegate with its
    /// key in ANOTHER_ADMIT_KEY.
    Admit {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        done_when: Option<String>,
        #[arg(long, value_enum)]
        priority: Option<QueuePriorityArg>,
        /// Offer the item to this agent first.
        #[arg(long = "for", value_name = "AGENT")]
        for_agent: Option<String>,
        /// Required when a keyed delegate admits a security or data-loss item.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject a proposed or ready item, with a reason.
    Reject {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        #[arg(long)]
        reason: String,
    },
    /// Edit a proposed or ready item.
    Edit {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        done_when: Option<String>,
        #[arg(long, value_enum)]
        priority: Option<QueuePriorityArg>,
        /// Offer the item to this agent first. Also clears who it is not offered to.
        #[arg(long = "for", value_name = "AGENT", conflicts_with = "for_anyone")]
        for_agent: Option<String>,
        /// Offer the item to anyone. Also clears who it is not offered to.
        #[arg(long)]
        for_anyone: bool,
        #[arg(long, value_enum)]
        kind: Option<QueueKindArg>,
    },
    /// Take the next ready item. Exit code 3 when the agent is at its limit.
    Next {
        #[command(flatten)]
        who: QueueWho,
    },
    /// Add a note to an item that is not done.
    Note {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        #[arg(long)]
        body: String,
    },
    /// Give a claimed item back, hand it to another agent with --to, or, as an admitter,
    /// release another agent's claim with --reason.
    Release {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        /// The claimer's note, kept in the item's history.
        #[arg(long)]
        note: Option<String>,
        /// Hand the item to this agent, which takes it with `another queue next`.
        #[arg(long, value_name = "AGENT", conflicts_with = "reason")]
        to: Option<String>,
        /// An admitter's reason for releasing another agent's claim.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Finish a claimed item with its result: a summary and one pointer.
    #[command(group(
        clap::ArgGroup::new("pointer").required(true).args(["pr", "commit", "file"])
    ))]
    Done {
        item: String,
        #[command(flatten)]
        who: QueueWho,
        /// What was done, up to 500 characters.
        #[arg(long)]
        summary: String,
        /// A pull request: its number or an https URL.
        #[arg(long, value_name = "N_OR_URL")]
        pr: Option<String>,
        /// A commit: 7 to 64 hex characters.
        #[arg(long, value_name = "SHA")]
        commit: Option<String>,
        /// A file path.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },
    /// How many claimed, unfinished items an agent may hold.
    #[command(subcommand)]
    Limit(QueueLimitCmd),
    /// The agents that may admit work items with a key.
    #[command(subcommand)]
    Delegate(QueueDelegateCmd),
    /// Show the grace period, or set it (2h to 30d; --owner only).
    Grace {
        /// For example 24h or 3d.
        duration: Option<String>,
        /// Act as the owner. Needs a terminal and a typed yes.
        #[arg(long)]
        owner: bool,
    },
    /// Check the queue's events against themselves and the tables. Exit code 1 on a finding.
    Audit {
        /// Report only findings since a duration (bare seconds, 2d) or an RFC 3339 time.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, hide = true)]
        owner: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum QueueLimitCmd {
    /// Set one agent's limit, 0 to 20; 0 means no seat (--owner only).
    Set {
        agent: String,
        limit: i64,
        /// Act as the owner. Needs a terminal and a typed yes.
        #[arg(long)]
        owner: bool,
    },
    /// Set the limit of agents with none of their own (--owner only).
    Default {
        limit: i64,
        /// Act as the owner. Needs a terminal and a typed yes.
        #[arg(long)]
        owner: bool,
    },
    /// Show the default limit and each limit the owner has set.
    List,
}

#[derive(Debug, Subcommand)]
pub enum QueueDelegateCmd {
    /// Name a delegate and print its key once (--owner only).
    Add {
        agent: String,
        /// Act as the owner. Needs a terminal and a typed yes.
        #[arg(long)]
        owner: bool,
    },
    /// End a delegation; its key stops working (--owner only).
    Remove {
        agent: String,
        /// Act as the owner. Needs a terminal and a typed yes.
        #[arg(long)]
        owner: bool,
    },
    /// List the delegates.
    List,
}

/// The variable a delegate's key is read from, and nowhere else.
const ADMIT_KEY_VAR: &str = "ANOTHER_ADMIT_KEY";

fn usage(message: String) -> anyhow::Error {
    UsageError(message).into()
}

/// An owner action needs a terminal on standard input and standard error.
pub(crate) fn owner_terminal() -> anyhow::Result<()> {
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        return Ok(());
    }
    Err(Error::Forbidden("owner actions need an interactive terminal".into()).into())
}

/// `text` as it is safe to print at the prompt: line by line, with every escape, control
/// or text-direction character turned into a space, so nothing in a flag or an item can
/// redraw what the owner is asked to approve.
fn safe_lines(text: &str) -> String {
    text.trim_end()
        .split('\n')
        .map(|line| another_core::single_line(line, usize::MAX))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Print what is about to happen and proceed only on a typed `yes` or `y`.
pub(crate) fn owner_confirm(what: &str) -> anyhow::Result<()> {
    let mut err = std::io::stderr();
    writeln!(err, "{}", safe_lines(what))?;
    write!(err, "Type yes to confirm: ")?;
    err.flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    match answer.trim() {
        "yes" | "y" => Ok(()),
        _ => Err(Error::Forbidden("not confirmed: nothing was changed".into()).into()),
    }
}

/// The owner in person: a terminal, then the typed yes to `what`. `what` is built after the
/// terminal check, so nothing is read for a caller with no terminal.
fn as_owner(what: impl FnOnce() -> anyhow::Result<String>) -> anyhow::Result<QueueCaller> {
    owner_terminal()?;
    owner_confirm(&what()?)?;
    Ok(QueueCaller::Owner)
}

/// An owner-only command: refused without `--owner`, whatever key or name is presented.
/// `valid` is the check of the command's own values; a value out of range is refused
/// before the owner is asked anything.
fn owner_only(
    owner: bool,
    action: &str,
    valid: another_core::Result<()>,
    what: impl FnOnce() -> String,
) -> anyhow::Result<QueueCaller> {
    if !owner {
        return Err(Error::Forbidden(format!(
            "{action} is the owner's alone: run the command with --owner at a terminal; a delegate key does not cover it"
        ))
        .into());
    }
    valid?;
    as_owner(|| Ok(what()))
}

/// The caller of a command an admitter may run on an item: the owner, or an agent with the
/// key it presents, if any. For the owner, `shown` reads the item and returns what to print
/// with the item's revision; the revision comes back so the write applies only to the item
/// as the owner saw it.
fn admitter(
    who: QueueWho,
    shown: impl FnOnce() -> anyhow::Result<(String, String)>,
) -> anyhow::Result<(QueueCaller, Option<String>)> {
    if who.owner {
        let mut revision = None;
        let caller = as_owner(|| {
            let (text, at) = shown()?;
            revision = Some(at);
            Ok(text)
        })?;
        return Ok((caller, revision));
    }
    let agent = who.agent.ok_or_else(|| {
        usage("no agent: pass --agent, set ANOTHER_AGENT, or act as the owner with --owner".into())
    })?;
    Ok((
        QueueCaller::Agent {
            agent,
            admit_key: std::env::var(ADMIT_KEY_VAR).ok().filter(|k| !k.is_empty()),
        },
        None,
    ))
}

/// The agent of a command only an agent runs.
fn agent_only(who: QueueWho, command: &str) -> anyhow::Result<String> {
    if who.owner {
        return Err(usage(format!(
            "--owner does not apply to queue {command}: it is run as an agent; pass --agent or set ANOTHER_AGENT"
        )));
    }
    who.agent
        .ok_or_else(|| usage("no agent: pass --agent or set ANOTHER_AGENT".into()))
}

/// The lines the owner is shown before deciding an item.
fn describe(action: &str, item: &QueueItem, extra: &[(&str, Option<String>)]) -> String {
    let mut out = format!("{action} {} as the owner:\n", item.label);
    let mut line = |name: &str, value: &str| {
        let mut lines = value.lines();
        out.push_str(&format!(
            "  {name:<15} {}\n",
            lines.next().unwrap_or_default()
        ));
        for more in lines {
            out.push_str(&format!("  {:<15} {more}\n", ""));
        }
    };
    line("title", &item.title);
    line("kind", item.kind.flag());
    line("state", item.state.as_str());
    line("scope", item.scope.as_deref().unwrap_or("(not set)"));
    line(
        "done condition",
        item.done_condition.as_deref().unwrap_or("(not set)"),
    );
    line("proposed by", &item.proposed_by);
    if let Some(claim) = &item.claim {
        line("claimed by", &claim.agent);
    }
    for (name, value) in extra {
        if let Some(value) = value {
            line(name, value);
        }
    }
    out
}

/// After an agent finished or gave back its own item: one more line, last, when another
/// item is ready that `next` would hand it. Only a name the owner gave a seat is told
/// (`QueueOffer::announce`), as in the hook packs. The line goes to standard output, or
/// with `--json` to standard error, so the JSON on standard output is the item alone. It
/// only reads, and a failed read prints nothing and leaves the exit status as it was.
fn say_what_is_next(store: &Store, agent: &str, json: bool) {
    let Ok(offer) = store.queue_offer(agent) else {
        return;
    };
    if offer.announce().is_none() {
        return;
    }
    let Some(me) = crate::hooks::plain_address(&offer.agent) else {
        return;
    };
    let line = format!("Another item is ready that {me} may take: another queue next --agent {me}");
    if json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Run one `another queue` command.
pub fn run(store: &Store, cmd: QueueCmd, json: bool) -> anyhow::Result<ExitCode> {
    let code = match cmd {
        QueueCmd::Propose {
            who,
            title,
            body,
            scope,
            done_when,
            kind,
        } => {
            let input = ProposeItem {
                title,
                body: body_arg(body)?,
                scope,
                done_when,
                kind: kind.into(),
            };
            input.validate()?;
            // A proposal needs no key: an agent proposes as itself.
            let caller = if who.owner {
                as_owner(|| {
                    Ok(format!(
                        "Propose an item as the owner:\n  title           {}\n  kind            {}",
                        input.title,
                        input.kind.flag()
                    ))
                })?
            } else {
                QueueCaller::Agent {
                    agent: who.agent.ok_or_else(|| {
                        usage(
                            "no agent: pass --agent, set ANOTHER_AGENT, or act as the owner with --owner"
                                .into(),
                        )
                    })?,
                    admit_key: None,
                }
            };
            let item = store.queue_propose(&caller, input)?;
            emit(json, &item, output::queue_item)
        }
        QueueCmd::List {
            agent,
            state,
            scope,
            mine,
            self_admitted,
            needs_owner,
            limit,
        } => {
            if needs_owner {
                let list = store.queue_needs_owner()?;
                return Ok(emit(json, &list, output::queue_needs_owner));
            }
            let mine = match (mine, agent) {
                (false, _) => None,
                (true, Some(agent)) => Some(agent),
                (true, None) => {
                    return Err(usage(
                        "--mine needs an agent: pass --agent or set ANOTHER_AGENT".into(),
                    ))
                }
            };
            let items = store.queue_list(QueueFilter {
                state: state.map(Into::into),
                scope,
                mine,
                self_admitted,
                limit,
            })?;
            emit(json, &items, |i| output::queue_items(i))
        }
        QueueCmd::Show { item } => {
            let item = store.queue_item(&item)?;
            emit(json, &item, output::queue_item)
        }
        QueueCmd::Admit {
            item,
            who,
            scope,
            done_when,
            priority,
            for_agent,
            reason,
        } => {
            let input = AdmitItem {
                scope,
                done_when,
                priority: priority.map(Into::into),
                for_agent,
                reason,
            };
            input.validate()?;
            let (caller, revision) = admitter(who, || {
                // What the owner approves is the item as the flags leave it.
                let (mut shown, revision) = store.queue_item_shown(&item)?;
                if input.scope.is_some() {
                    shown.scope = input.scope.clone();
                }
                if input.done_when.is_some() {
                    shown.done_condition = input.done_when.clone();
                }
                let text = describe(
                    "Admit",
                    &shown,
                    &[
                        ("priority", input.priority.map(|p| p.as_str().to_string())),
                        ("for", input.for_agent.clone()),
                        ("reason", input.reason.clone()),
                    ],
                );
                Ok((text, revision))
            })?;
            let admitted = match revision {
                Some(revision) => store.queue_admit_shown(&caller, &item, input, &revision)?,
                None => store.queue_admit(&caller, &item, input)?,
            };
            emit(json, &admitted, output::queue_item)
        }
        QueueCmd::Reject { item, who, reason } => {
            QueueItem::validate_reason(&reason)?;
            let (caller, revision) = admitter(who, || {
                let (shown, revision) = store.queue_item_shown(&item)?;
                let text = describe("Reject", &shown, &[("reason", Some(reason.clone()))]);
                Ok((text, revision))
            })?;
            let rejected = match revision {
                Some(revision) => store.queue_reject_shown(&caller, &item, &reason, &revision)?,
                None => store.queue_reject(&caller, &item, &reason)?,
            };
            emit(json, &rejected, output::queue_item)
        }
        QueueCmd::Edit {
            item,
            who,
            scope,
            done_when,
            priority,
            for_agent,
            for_anyone,
            kind,
        } => {
            let input = EditItem {
                scope,
                done_when,
                priority: priority.map(Into::into),
                for_agent,
                for_anyone,
                kind: kind.map(Into::into),
            };
            input.validate()?;
            let (caller, revision) = admitter(who, || {
                let (shown, revision) = store.queue_item_shown(&item)?;
                let text = describe(
                    "Edit",
                    &shown,
                    &[
                        ("new scope", input.scope.clone()),
                        ("new done cond.", input.done_when.clone()),
                        (
                            "new priority",
                            input.priority.map(|p| p.as_str().to_string()),
                        ),
                        ("new kind", input.kind.map(|k| k.flag().to_string())),
                        ("for", input.for_agent.clone()),
                        ("for", input.for_anyone.then(|| "anyone".to_string())),
                    ],
                );
                Ok((text, revision))
            })?;
            let edited = match revision {
                Some(revision) => store.queue_edit_shown(&caller, &item, input, &revision)?,
                None => store.queue_edit(&caller, &item, input)?,
            };
            emit(json, &edited, output::queue_item)
        }
        QueueCmd::Next { who } => {
            let agent = agent_only(who, "next")?;
            let next = store.queue_next(&agent)?;
            let refused = next.refused.is_some();
            let printed = emit(json, &next, output::queue_next);
            if refused {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                printed
            }
        }
        QueueCmd::Note { item, who, body } => {
            let agent = agent_only(who, "note")?;
            let noted = store.queue_note(&agent, &item, &body)?;
            emit(json, &noted, output::queue_item)
        }
        QueueCmd::Release {
            item,
            who,
            note,
            to,
            reason,
        } => {
            let input = ReleaseItem { note, to, reason };
            input.validate()?;
            // A claimer giving back its own item, plain or with --to: neither --owner nor
            // --reason. A delegate's key does not change that.
            let own_release = !who.owner && input.reason.is_none();
            let claimer = who.agent.clone().filter(|_| own_release);
            let (caller, revision) = admitter(who, || {
                let (shown, revision) = store.queue_item_shown(&item)?;
                let text = describe("Release", &shown, &[("reason", input.reason.clone())]);
                Ok((text, revision))
            })?;
            let released = match revision {
                Some(revision) => store.queue_release_shown(&caller, &item, input, &revision)?,
                None => store.queue_release(&caller, &item, input)?,
            };
            let printed = emit(json, &released, output::queue_item);
            if let Some(agent) = claimer {
                say_what_is_next(store, &agent, json);
            }
            printed
        }
        QueueCmd::Done {
            item,
            who,
            summary,
            pr,
            commit,
            file,
        } => {
            let agent = agent_only(who, "done")?;
            let (pointer_kind, pointer) = match (pr, commit, file) {
                (Some(pr), None, None) => (PointerKind::Pr, pr),
                (None, Some(commit), None) => (PointerKind::Commit, commit),
                (None, None, Some(file)) => (PointerKind::File, file),
                _ => return Err(usage("pass exactly one of --pr, --commit or --file".into())),
            };
            let finished = store.queue_done(
                &agent,
                &item,
                DoneItem {
                    summary,
                    pointer_kind,
                    pointer,
                },
            )?;
            let printed = emit(json, &finished, output::queue_item);
            say_what_is_next(store, &agent, json);
            printed
        }
        QueueCmd::Limit(QueueLimitCmd::Set {
            agent,
            limit,
            owner,
        }) => {
            let valid = QueueLimits::validate(limit);
            let caller = owner_only(owner, "setting a limit", valid, || {
                format!("Set the queue limit of {agent} to {limit} as the owner.")
            })?;
            let limits = store.queue_set_limit(&caller, &agent, limit)?;
            emit(json, &limits, output::queue_limits)
        }
        QueueCmd::Limit(QueueLimitCmd::Default { limit, owner }) => {
            let valid = QueueLimits::validate(limit);
            let caller = owner_only(owner, "setting the default limit", valid, || {
                format!("Set the default queue limit to {limit} as the owner.")
            })?;
            let limits = store.queue_set_default_limit(&caller, limit)?;
            emit(json, &limits, output::queue_limits)
        }
        QueueCmd::Limit(QueueLimitCmd::List) => {
            let limits = store.queue_limits()?;
            emit(json, &limits, output::queue_limits)
        }
        QueueCmd::Delegate(QueueDelegateCmd::Add { agent, owner }) => {
            let caller = owner_only(owner, "naming a delegate", Ok(()), || {
                format!(
                    "Name {agent} a queue delegate as the owner. It will admit, reject, edit and release work items with a key that is printed once."
                )
            })?;
            let made = store.queue_delegate_add(&caller, &agent)?;
            emit(json, &made, |d| {
                format!(
                    "{} is a delegate. Its key, shown this once:\n{}\nIt acts with --agent {} and the key in {ADMIT_KEY_VAR}.",
                    d.agent, d.key, d.agent
                )
            })
        }
        QueueCmd::Delegate(QueueDelegateCmd::Remove { agent, owner }) => {
            let caller = owner_only(owner, "removing a delegate", Ok(()), || {
                format!("Remove {agent} as a queue delegate, as the owner. Its key stops working.")
            })?;
            let left = store.queue_delegate_remove(&caller, &agent)?;
            emit(json, &left, |d| output::queue_delegates(d))
        }
        QueueCmd::Delegate(QueueDelegateCmd::List) => {
            let delegates = store.queue_delegates()?;
            emit(json, &delegates, |d| output::queue_delegates(d))
        }
        QueueCmd::Grace {
            duration: None,
            owner,
        } => {
            if owner {
                return Err(usage(
                    "--owner sets the grace period: pass a duration, for example 24h".into(),
                ));
            }
            let grace = store.queue_grace()?;
            emit(json, &grace, output::queue_grace)
        }
        QueueCmd::Grace {
            duration: Some(duration),
            owner,
        } => {
            let seconds = parse_secs(&duration).map_err(|e| Error::Invalid(format!("{e:#}")));
            let valid = seconds.as_ref().map_or_else(
                |e| Err(Error::Invalid(e.to_string())),
                |s| QueueGrace::validate(*s),
            );
            let caller = owner_only(owner, "changing the grace period", valid, || {
                format!("Set the queue grace period to {duration} as the owner.")
            })?;
            let seconds = seconds?;
            let grace = store.queue_set_grace(&caller, seconds)?;
            emit(json, &grace, output::queue_grace)
        }
        QueueCmd::Audit { since, owner } => {
            if owner {
                return Err(usage(
                    "--owner does not apply to queue audit: it only reads".into(),
                ));
            }
            let since = match since {
                None => None,
                Some(raw) => match Since::parse(&raw, store.now())? {
                    Since::Time(t) => Some(t),
                    Since::After(_) => {
                        return Err(usage(
                            "queue audit --since takes bare seconds like 3600, a duration like 2d or an RFC 3339 time, not an event id".into(),
                        ))
                    }
                },
            };
            let audit = store.queue_audit(since)?;
            let printed = emit(json, &audit, output::queue_audit);
            if audit.ok {
                printed
            } else {
                ExitCode::FAILURE
            }
        }
    };
    Ok(code)
}
