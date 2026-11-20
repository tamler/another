//! `another needs`: the one list of what needs the owner (docs/NEEDS.md).
//!
//! Read only. It opens the home as `another doctor` does, so it creates, migrates and
//! sweeps nothing; it calls no transport and runs no program. Queue rows come from
//! [`Store::queue_needs_owner`] and session rows from [`crate::transport::status_report`]:
//! the list defines no state of its own.
//!
//! Every value an agent or a peer wrote is printed through [`display_line`], in the text
//! form and in the JSON, because any local process can write the database.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use another_core::{
    display_line, fmt_ts, parse_ts, QueueClaimed, QueueItem, QueueKind, QuietClaim, Store,
    WakeOutcome, DB_FILE, GONE_CONFIRM_AFTER_MS,
};
use serde::Serialize;

use crate::transport::{span, StatusRow};

/// Rows printed per kind unless `--limit` says otherwise.
pub const DEFAULT_LIMIT: u32 = 10;
/// How long the command waits for a locked database.
const BUSY: Duration = Duration::from_secs(2);
/// A session whose hook ran in this long is live: the daemon should be checking it.
const LIVE_HOOK_MS: i64 = 10 * 60_000;
const TITLE_CHARS: usize = 60;
const VALUE_CHARS: usize = 200;
/// What `--brief` prints when the list cannot be read, so a prompt is never blank then.
const BRIEF_CANNOT_READ: &str = "needs: cannot read, run another doctor";
const WAIT: &str = "wait: the session may come back";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    ChecksStopped,
    TransportConfig,
    SkippedEvent,
    DecideNow,
    Blocked,
    Stuck,
    Unreachable,
    TransportSilent,
    QuietClaim,
    NoTaker,
    Proposal,
}

/// The sections of the list and the kinds of each, in the order they are shown.
const SECTIONS: [(&str, &[Kind]); 5] = [
    (
        "Another",
        &[
            Kind::ChecksStopped,
            Kind::TransportConfig,
            Kind::SkippedEvent,
        ],
    ),
    ("Decide now", &[Kind::DecideNow]),
    (
        "Agents",
        &[
            Kind::Blocked,
            Kind::Stuck,
            Kind::Unreachable,
            Kind::TransportSilent,
        ],
    ),
    ("Claims", &[Kind::QuietClaim, Kind::NoTaker]),
    ("Proposals", &[Kind::Proposal]),
];

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::ChecksStopped => "checks_stopped",
            Kind::TransportConfig => "transport_config",
            Kind::SkippedEvent => "skipped_event",
            Kind::DecideNow => "decide_now",
            Kind::Blocked => "blocked",
            Kind::Stuck => "stuck",
            Kind::Unreachable => "unreachable",
            Kind::TransportSilent => "transport_silent",
            Kind::QuietClaim => "quiet_claim",
            Kind::NoTaker => "no_taker",
            Kind::Proposal => "proposal",
        }
    }

    /// The command that lists the rows `--limit` cut.
    fn more(self) -> &'static str {
        match self {
            Kind::DecideNow | Kind::Proposal => "another queue list --state proposed",
            Kind::QuietClaim | Kind::NoTaker => "another queue list --needs-owner",
            Kind::SkippedEvent => "another activity --kinds sync",
            _ => "another transport status --attention",
        }
    }

    /// The kind in the one-line form: a count and fixed words.
    fn brief(self, n: u32) -> String {
        let plural = if n == 1 { "" } else { "s" };
        match self {
            Kind::ChecksStopped => "checks stopped".to_string(),
            Kind::TransportConfig => "transports.json refused".to_string(),
            Kind::SkippedEvent => format!("{n} skipped event{plural}"),
            Kind::DecideNow => format!("{n} decide now"),
            Kind::Blocked => format!("{n} blocked"),
            Kind::Stuck => format!("{n} stuck"),
            Kind::Unreachable => format!("{n} unreachable"),
            Kind::TransportSilent => format!("{n} transport silent"),
            Kind::QuietClaim => format!("{n} quiet claim{plural}"),
            Kind::NoTaker => format!("{n} no taker"),
            Kind::Proposal => format!("{n} proposal{plural}"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct Counts {
    checks_stopped: u32,
    transport_config: u32,
    skipped_event: u32,
    decide_now: u32,
    blocked: u32,
    stuck: u32,
    unreachable: u32,
    transport_silent: u32,
    quiet_claim: u32,
    no_taker: u32,
    proposal: u32,
}

impl Counts {
    fn of(&self, kind: Kind) -> u32 {
        match kind {
            Kind::ChecksStopped => self.checks_stopped,
            Kind::TransportConfig => self.transport_config,
            Kind::SkippedEvent => self.skipped_event,
            Kind::DecideNow => self.decide_now,
            Kind::Blocked => self.blocked,
            Kind::Stuck => self.stuck,
            Kind::Unreachable => self.unreachable,
            Kind::TransportSilent => self.transport_silent,
            Kind::QuietClaim => self.quiet_claim,
            Kind::NoTaker => self.no_taker,
            Kind::Proposal => self.proposal,
        }
    }

    fn add(&mut self, kind: Kind) {
        let n = match kind {
            Kind::ChecksStopped => &mut self.checks_stopped,
            Kind::TransportConfig => &mut self.transport_config,
            Kind::SkippedEvent => &mut self.skipped_event,
            Kind::DecideNow => &mut self.decide_now,
            Kind::Blocked => &mut self.blocked,
            Kind::Stuck => &mut self.stuck,
            Kind::Unreachable => &mut self.unreachable,
            Kind::TransportSilent => &mut self.transport_silent,
            Kind::QuietClaim => &mut self.quiet_claim,
            Kind::NoTaker => &mut self.no_taker,
            Kind::Proposal => &mut self.proposal,
        };
        *n += 1;
    }
}

#[derive(Debug, Clone, Serialize)]
struct FailedNotice {
    item: String,
    to: String,
    at: String,
}

#[derive(Debug, Clone, Serialize)]
struct SelfAdmitted {
    /// The items self-admitted in the last seven days, as `Q` numbers, newest first.
    recent: Vec<String>,
    total: u32,
}

/// What is shown under the list and never counted: nothing the owner does clears it.
#[derive(Debug, Clone, Serialize)]
struct Closing {
    /// Sessions the transport no longer lists that hold no claim and have no mail waiting.
    gone_idle: u32,
    failed_notices: Vec<FailedNotice>,
    self_admitted: SelfAdmitted,
}

/// One thing that needs the owner. Every key is always present in the JSON.
#[derive(Debug, Clone, Serialize)]
struct Row {
    kind: Kind,
    /// Stable while the condition lasts.
    key: String,
    /// `owner`, or `admitter` where a delegate's key covers the step that clears the row.
    who: &'static str,
    /// When the condition began.
    since: Option<String>,
    age_seconds: Option<i64>,
    /// The proposer, the claimer, the reserved agent or the session's agent.
    agent: Option<String>,
    item: Option<String>,
    item_kind: Option<String>,
    scope: Option<String>,
    title: Option<String>,
    /// The first 8 characters of the session id; `""` when the session has none.
    session: Option<String>,
    transport: Option<String>,
    target: Option<String>,
    reason: Option<String>,
    /// Claimed items bound to this row's session.
    claims: Vec<String>,
    /// Claimed items of the agent's name that are bound to no session.
    name_claims: Vec<String>,
    /// When the session row was last checked.
    as_of: Option<String>,
    next: Vec<String>,
    /// The same steps as a delegate runs them; empty when `who` is `owner`.
    next_delegate: Vec<String>,
    /// The row as the text form prints it.
    #[serde(skip)]
    line: String,
    #[serde(skip)]
    order: (i64, i64, String),
}

impl Row {
    fn new(kind: Kind, key: String, line: String) -> Row {
        Row {
            kind,
            key,
            who: "owner",
            since: None,
            age_seconds: None,
            agent: None,
            item: None,
            item_kind: None,
            scope: None,
            title: None,
            session: None,
            transport: None,
            target: None,
            reason: None,
            claims: Vec::new(),
            name_claims: Vec::new(),
            as_of: None,
            next: Vec::new(),
            next_delegate: Vec::new(),
            line,
            order: (i64::MAX, 0, String::new()),
        }
    }

    /// The condition began at `at` (milliseconds); rows of a kind are shown oldest first.
    fn since(mut self, at: Option<i64>, now: i64) -> Row {
        self.since = at.map(fmt_ts);
        self.age_seconds = at.map(|at| (now - at).max(0) / 1000);
        self.order.0 = at.unwrap_or(i64::MAX);
        self
    }

    /// The steps that clear the row. A delegate's key covers them when `admitter` is set.
    fn steps(mut self, next: Vec<String>, admitter: bool) -> Row {
        if admitter {
            self.who = "admitter";
            self.next_delegate = next.iter().map(|step| as_delegate(step)).collect();
        }
        self.next = next;
        self
    }
}

/// `another needs --json`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    now: String,
    node: String,
    /// The sum of `counts`. Nothing under `closing` is in it.
    total: u32,
    limit: u32,
    counts: Counts,
    closing: Closing,
    /// At most `limit` rows per kind, in the order of the sections.
    rows: Vec<Row>,
}

fn clean(text: &str) -> String {
    display_line(text, VALUE_CHARS)
}

/// A value as one word of a printed command: as it is when it holds nothing a shell
/// reads, in single quotes otherwise.
fn word(text: &str) -> String {
    let text = clean(text);
    let plain = |c: char| c.is_ascii_alphanumeric() || "@._-:/+%".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Free text as one argument of a printed command, quoted once as a whole: in double
/// quotes when it holds nothing a shell reads inside them, in single quotes otherwise.
/// A value is never quoted by [`word`] and then put inside another pair of quotes.
fn phrase(text: &str) -> String {
    let text = clean(text);
    if text.chars().any(|c| "\"$`\\!".contains(c)) {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        format!("\"{text}\"")
    }
}

/// An owner's step as a delegate runs it, with its key in `ANOTHER_ADMIT_KEY`.
fn as_delegate(step: &str) -> String {
    match step.strip_suffix(" --owner") {
        Some(head) => format!("{head} --agent <delegate>"),
        None => step.to_string(),
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The first `n` characters of a session id, cleaned.
fn session_head(id: &str, n: usize) -> String {
    clean(&id.chars().take(n).collect::<String>())
}

fn item_number(item: &QueueItem) -> String {
    clean(&item.label)
}

/// A row about one queue item: `label scope  title`, then `words`.
fn item_row(kind: Kind, item: &QueueItem, lead: &str, words: &str) -> Row {
    let label = item_number(item);
    let title = display_line(&item.title, TITLE_CHARS);
    let mut row = Row::new(
        kind,
        format!("{}:{label}", kind.as_str()),
        format!("{label} {lead}  {title}  {words}"),
    );
    row.item = Some(label);
    row.item_kind = Some(item.kind.as_str().to_string());
    row.scope = item.scope.as_deref().map(clean);
    row.title = Some(title);
    row.order.1 = item.seq;
    row
}

fn proposal_row(kind: Kind, item: &QueueItem, now: i64) -> Row {
    let at = parse_ts(&item.proposed_at);
    let proposer = clean(&item.proposed_by);
    let words = match at {
        Some(at) => format!("proposed by {proposer} {} ago", span(now - at)),
        None => format!("proposed by {proposer}"),
    };
    let label = word(&item.label);
    let mut row = item_row(kind, item, item.kind.as_str(), &words)
        .since(at, now)
        .steps(
            vec![
                format!("another queue show {label}"),
                format!("another queue admit {label} --owner"),
                format!("another queue reject {label} --reason \"<why>\" --owner"),
            ],
            kind == Kind::Proposal,
        );
    row.agent = Some(proposer);
    row
}

/// The second step of every quiet claim: ask the claimer by mail. The list prints no
/// wake: whether a pane still holds the agent is for `another wake` to decide when it is
/// run, and nothing the list reads can say it.
fn ask_step(agent: &str, label: &str) -> String {
    format!(
        "ask {} whether a session is working on {}: another send --from <your agent> --to {} --subject {}",
        clean(agent),
        clean(label),
        word(agent),
        phrase(&format!("Who holds {label}?"))
    )
}

/// `may_release`: what is stored licenses a release for a quiet name. It does when the
/// name has no session row a hook made, or when every such row was checked in the last
/// 5 minutes and is idle or gone.
fn quiet_claim_row(q: &QuietClaim, last_sign: Option<i64>, may_release: bool, now: i64) -> Row {
    let item = &q.item;
    let claimer = item.claim.as_ref().map_or("", |c| c.agent.as_str());
    let claimed_at = item.claim.as_ref().and_then(|c| parse_ts(&c.at));
    let since = match q.reason.as_str() {
        "session_quiet" => q.session_last_seen_at.as_deref().and_then(parse_ts),
        "old_unbound" => claimed_at,
        _ => last_sign.max(claimed_at),
    };
    let label = word(&item.label);
    let mut next = vec![
        format!("another queue show {label}"),
        ask_step(claimer, &item.label),
    ];
    if q.reason == "name_quiet" && may_release {
        next.push(format!(
            "another queue release {label} --reason \"<why>\" --owner"
        ));
    }
    let words = format!(
        "claimed by {}, {}",
        clean(claimer),
        clean(&crate::output::quiet_claim_words(q))
    );
    let lead = item.scope.as_deref().map_or("-".to_string(), clean);
    let mut row = item_row(Kind::QuietClaim, item, &lead, &words)
        .since(since, now)
        .steps(next, item.kind == QueueKind::Work);
    row.agent = Some(clean(claimer));
    row.reason = Some(clean(&q.reason));
    row
}

/// A ready item nobody can take: which case of the three holds, and its command.
fn no_taker_row(item: &QueueItem, any_seat: bool, default_limit: i64) -> Row {
    let label = word(&item.label);
    let (reason, agent, words, next) = match (&item.for_agent, any_seat) {
        (Some(reserved), _) => (
            "reserved",
            Some(clean(reserved)),
            format!(
                "ready, reserved for {}, which cannot take it now",
                clean(reserved)
            ),
            format!("another queue edit {label} --for-anyone --owner"),
        ),
        (None, false) => (
            "no_seat",
            None,
            "ready, no agent has a seat".to_string(),
            if default_limit == 0 {
                "another queue limit default 1 --owner".to_string()
            } else {
                "another queue limit list".to_string()
            },
        ),
        (None, true) => (
            "skipped",
            item.skip_agent.as_deref().map(clean),
            match &item.skip_agent {
                Some(gave_up) => format!(
                    "ready, {} gave it up less than an hour ago and nobody else has a seat",
                    clean(gave_up)
                ),
                None => "ready, no agent with a seat can take it now".to_string(),
            },
            "another queue limit list".to_string(),
        ),
    };
    let lead = item.scope.as_deref().map_or("-".to_string(), clean);
    let mut row = item_row(Kind::NoTaker, item, &lead, &words).steps(
        vec![next],
        reason == "reserved" && item.kind == QueueKind::Work,
    );
    row.agent = agent;
    row.reason = Some(reason.to_string());
    row
}

/// A row about one session: `kind  agent sess..  transport target  [reason ]age`.
fn session_row(kind: Kind, reason: &str, r: &StatusRow, claims: &[QueueClaimed], now: i64) -> Row {
    // The reason is a stored string (the attention column, or the wake stop), and no
    // constraint limits what it holds: it is cleaned like every other stored value.
    let reason = clean(reason);
    let agent = clean(&r.agent);
    let transport = clean(&r.transport);
    let target = clean(&r.target);
    let since = r.attention_since.as_deref().and_then(parse_ts);
    let session = if r.session_id.is_empty() {
        "-".to_string()
    } else {
        format!("{}..", session_head(&r.session_id, 4))
    };
    let mut line = format!(
        "{:<11}  {agent} {session}  {transport} {target}  ",
        kind.as_str()
    );
    if kind == Kind::Unreachable {
        let _ = write!(line, "{reason} ");
    }
    line.push_str(&since.map_or("-".to_string(), |at| span(now - at)));
    if kind == Kind::Stuck {
        if let Some(outcome) = &r.last_wake_outcome {
            let meaning = WakeOutcome::parse(outcome).map_or("not known", |o| o.meaning());
            let _ = write!(line, "  last wake: {meaning}");
        }
    }
    let mine = |c: &&QueueClaimed| c.agent == r.agent;
    let bound: Vec<String> = claims
        .iter()
        .filter(mine)
        .filter(|c| !r.session_id.is_empty() && c.session.as_deref() == Some(&r.session_id))
        .map(|c| clean(&c.item))
        .collect();
    let unbound: Vec<String> = claims
        .iter()
        .filter(mine)
        .filter(|c| c.session.is_none())
        .map(|c| clean(&c.item))
        .collect();
    if !bound.is_empty() {
        let _ = write!(line, "  claims {}", bound.join(", "));
    }
    if !unbound.is_empty() {
        // Named only: they may belong to a working sibling, so no step is printed for them.
        let _ = write!(line, "  name also holds {} (not bound)", unbound.join(", "));
    }
    if r.stale {
        match &r.observed_at {
            Some(at) => {
                let _ = write!(line, "  as of {at}");
            }
            None => line.push_str("  not checked yet"),
        }
    }
    let head = session_head(&r.session_id, 8);
    let mut row =
        Row::new(kind, format!("{}:{agent}:{head}", kind.as_str()), line).since(since, now);
    row.order.2 = format!("{agent} {head}");
    row.agent = Some(agent);
    row.session = Some(head);
    row.transport = Some(transport);
    row.target = Some(target);
    row.reason = Some(reason);
    row.claims = bound;
    row.name_claims = unbound;
    row.as_of = r.observed_at.clone();
    row
}

/// Read everything the list shows. `limit` is the most rows kept per kind.
pub fn report(store: &Store, limit: u32) -> anyhow::Result<Report> {
    let now = store.now();
    let status = crate::transport::status_report(store, false)?;
    let queue = store.queue_needs_owner()?;
    let claims = store.queue_claims()?;
    let limits = store.queue_limits()?;
    let agents = store.agents()?;
    let mut rows: Vec<Row> = Vec::new();
    let mut gone_idle = 0u32;

    // Another: whether the session rows below can be trusted at all.
    let live: Vec<&StatusRow> = status
        .sessions
        .iter()
        .filter(|r| {
            r.hook_made
                && r.transport == "herdr"
                && r.hook_seen_ms.is_some_and(|at| now - at <= LIVE_HOOK_MS)
        })
        .collect();
    if !live.is_empty() && live.iter().all(|r| r.stale) {
        let newest = live.iter().filter_map(|r| r.observed_ms).max();
        let mut row = Row::new(
            Kind::ChecksStopped,
            Kind::ChecksStopped.as_str().to_string(),
            format!(
                "checks stopped  {}, none checked since {}; agent rows may be out of date",
                plural(
                    live.len() as u32,
                    "live Herdr session",
                    "live Herdr sessions"
                ),
                newest.map_or("ever".to_string(), fmt_ts)
            ),
        )
        .steps(vec!["another daemon status".to_string()], false);
        row.as_of = newest.map(fmt_ts);
        rows.push(row);
    }
    if let Some(refused) = &status.config_error {
        let refused = display_line(refused, 2 * VALUE_CHARS);
        let mut row = Row::new(
            Kind::TransportConfig,
            Kind::TransportConfig.as_str().to_string(),
            format!("transports.json refused  {refused}"),
        )
        .steps(vec!["another doctor".to_string()], false);
        row.reason = Some(refused);
        rows.push(row);
    }
    // Events too large to send, in this node's log or its hub's, until acknowledged. The
    // values a marker carried are already bounded, safe lines.
    for (n, skip) in store.unacknowledged_skips()?.iter().enumerate() {
        let mut row = Row::new(
            Kind::SkippedEvent,
            format!(
                "{}:{}:{}",
                Kind::SkippedEvent.as_str(),
                skip.holder,
                skip.event_id
            ),
            skip.needs_line(),
        )
        .since(Some(skip.since_ms), now)
        .steps(skip.needs_steps(), false);
        row.order.1 = n as i64;
        row.agent = Some(skip.actor.clone());
        row.reason = Some(skip.direction.clone());
        rows.push(row);
    }

    // Agents: one row per session, by the stored reason. Nothing here acts on a name.
    let mut has_mail: BTreeMap<String, bool> = BTreeMap::new();
    let mut silent: BTreeMap<String, (Option<i64>, u32, Option<String>)> = BTreeMap::new();
    for r in &status.sessions {
        let Some(reason) = r.attention.as_deref() else {
            continue;
        };
        let place = format!("{} pane {}", clean(&r.transport), clean(&r.target));
        match reason {
            "blocked" => rows.push(
                session_row(Kind::Blocked, reason, r, &claims, now)
                    .steps(vec![format!("answer the dialog in {place}")], false),
            ),
            // A hook of the session ran after the stop: the agent has taken a turn.
            "stuck" if !r.stop_holds => {}
            "stuck" => rows.push(
                session_row(
                    Kind::Stuck,
                    r.wake_stop.as_deref().unwrap_or(reason),
                    r,
                    &claims,
                    now,
                )
                .steps(
                    vec![format!(
                        "in {place}, clear the input box, then send the agent any prompt"
                    )],
                    false,
                ),
            ),
            "gone" => {
                let row = session_row(Kind::Unreachable, reason, r, &claims, now);
                if row.claims.is_empty() {
                    // Mail waits on the agent, not on one session: the first gone session
                    // of the agent carries the inbox step, and the others are closing-line
                    // sessions, so one mailbox is one row.
                    let mail = match has_mail.get(&r.agent) {
                        Some(_) => false,
                        None => {
                            let mail = store.unannounced(&r.agent, "", 1)?.count > 0;
                            has_mail.insert(r.agent.clone(), mail);
                            mail
                        }
                    };
                    if !mail {
                        gone_idle += 1;
                        continue;
                    }
                    let inbox = format!("another inbox --agent {}", word(&r.agent));
                    rows.push(row.steps(vec![inbox], false));
                    continue;
                }
                // One missed listing makes a session gone, so a release is offered only
                // when no hook of it has run since, it has been gone for the confirmation
                // time, a trusted listing had seen it live, and the row was just checked.
                let since_gone = r.state_since_ms;
                let confirmed = since_gone.is_some_and(|at| now - at >= GONE_CONFIRM_AFTER_MS)
                    && r.hook_seen_ms <= since_gone
                    && r.seen_live
                    && !r.stale;
                let mut next = Vec::new();
                for item in &row.claims {
                    let item = word(item);
                    next.push(format!("another queue show {item}"));
                    if confirmed {
                        next.push(format!(
                            "another queue release {item} --reason \"session gone\" --owner"
                        ));
                    }
                }
                if !confirmed {
                    next.push(WAIT.to_string());
                }
                rows.push(row.steps(next, false));
            }
            "unknown" => rows.push(
                session_row(Kind::Unreachable, reason, r, &claims, now).steps(
                    vec![format!("look at {place}: it may hold another session")],
                    false,
                ),
            ),
            "no_answer" => {
                let since = r.attention_since.as_deref().and_then(parse_ts);
                let entry = silent.entry(r.transport.clone()).or_default();
                entry.0 = match (entry.0, since) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                entry.1 += 1;
                entry.2 = entry.2.take().max(r.observed_at.clone());
            }
            // `undelivered`, and any reason a later build stores.
            _ => rows.push(
                session_row(Kind::Unreachable, reason, r, &claims, now).steps(
                    vec![
                        "another doctor".to_string(),
                        "another transport status".to_string(),
                    ],
                    false,
                ),
            ),
        }
    }
    for (transport, (oldest, sessions, as_of)) in silent {
        let name = clean(&transport);
        let mut next = vec!["another doctor".to_string()];
        if transport == "herdr" {
            next.push("herdr agent list".to_string());
        }
        let mut row = Row::new(
            Kind::TransportSilent,
            format!("{}:{name}", Kind::TransportSilent.as_str()),
            format!(
                "transport silent  {name} has not answered for {}, {}",
                oldest.map_or("-".to_string(), |at| span(now - at)),
                plural(sessions, "session", "sessions")
            ),
        )
        .since(oldest, now)
        .steps(next, false);
        row.order.2 = name.clone();
        row.transport = Some(name);
        row.reason = Some("no_answer".to_string());
        row.as_of = as_of;
        rows.push(row);
    }

    // The queue's part.
    for item in &queue.decide_now {
        rows.push(proposal_row(Kind::DecideNow, item, now));
    }
    for item in &queue.awaiting {
        rows.push(proposal_row(Kind::Proposal, item, now));
    }
    for q in &queue.quiet_claims {
        let claimer = q.item.claim.as_ref().map(|c| c.agent.as_str());
        let last_sign = agents
            .iter()
            .find(|a| Some(a.address.as_str()) == claimer)
            .and_then(|a| a.last_seen_at.as_deref())
            .and_then(parse_ts);
        // An observation older than 5 minutes licenses nothing: the session may be inside
        // one long tool call. So every session row of the name must have been checked
        // just now and be idle or gone; any other state (working, blocked, unknown,
        // present, no answer) may be a session in a turn. A row that only holds the
        // outcome of a wake by hand is no session and is never checked, so it is not
        // counted. A name with no session row at all is only silent.
        let may_release = status
            .sessions
            .iter()
            .filter(|r| r.hook_made && Some(r.agent.as_str()) == claimer)
            .all(|r| {
                r.observed_ms
                    .is_some_and(|at| now - at <= crate::transport::STALE_AFTER_MS)
                    && matches!(r.observed_state.as_deref(), Some("idle" | "gone"))
            });
        rows.push(quiet_claim_row(q, last_sign, may_release, now));
    }
    // A seat, as the queue counts it: an agent of this node whose limit is above 0.
    let any_seat = agents.iter().any(|a| {
        limits
            .agents
            .iter()
            .find(|l| l.agent == a.address)
            .map_or(limits.default_limit, |l| l.limit)
            > 0
    });
    for item in &queue.nobody_can_take {
        rows.push(no_taker_row(item, any_seat, limits.default_limit));
    }

    let mut counts = Counts::default();
    for row in &rows {
        counts.add(row.kind);
    }
    // The order of the sections; inside a kind the oldest first, then by item or agent.
    let place = |kind: Kind| {
        SECTIONS
            .iter()
            .flat_map(|(_, kinds)| kinds.iter())
            .position(|k| *k == kind)
    };
    rows.sort_by(|a, b| (place(a.kind), &a.order).cmp(&(place(b.kind), &b.order)));
    let mut kept: BTreeMap<&str, u32> = BTreeMap::new();
    rows.retain(|row| {
        let n = kept.entry(row.kind.as_str()).or_default();
        *n += 1;
        *n <= limit
    });

    Ok(Report {
        now: fmt_ts(now),
        node: clean(&store.node()?.display_name),
        total: SECTIONS
            .iter()
            .flat_map(|(_, kinds)| kinds.iter())
            .map(|k| counts.of(*k))
            .sum(),
        limit,
        counts,
        closing: Closing {
            gone_idle,
            failed_notices: queue
                .failed_notices
                .iter()
                .map(|f| FailedNotice {
                    item: clean(&f.item),
                    to: clean(&f.to),
                    at: f.at.clone(),
                })
                .collect(),
            self_admitted: SelfAdmitted {
                recent: queue.self_admitted.recent.iter().map(item_number).collect(),
                total: queue.self_admitted.total,
            },
        },
        rows,
    })
}

/// `named` items, then how many more there are.
fn named(items: Vec<String>, most: usize) -> String {
    let more = items.len().saturating_sub(most);
    let mut out = items.into_iter().take(most).collect::<Vec<_>>().join(", ");
    if more > 0 {
        let _ = write!(out, " and {more} more");
    }
    out
}

/// The lines under the list. Never counted, and never in the one-line form.
fn closing_lines(c: &Closing) -> Vec<String> {
    let mut lines = Vec::new();
    if c.gone_idle > 0 {
        lines.push(format!(
            "{} gone with nothing waiting: another wake prune --dry-run",
            plural(c.gone_idle, "session", "sessions")
        ));
    }
    if let Some(first) = c.failed_notices.first() {
        let notices = c
            .failed_notices
            .iter()
            .map(|f| format!("{} to {}", f.item, f.to))
            .collect();
        lines.push(format!(
            "{} could not be sent in the last 24 hours ({}): another queue show {}",
            plural(c.failed_notices.len() as u32, "notice", "notices"),
            named(notices, 5),
            word(&first.item)
        ));
    }
    if !c.self_admitted.recent.is_empty() {
        lines.push(format!(
            "{} self-admitted in the last 7 days ({}), {} in all: another queue list --self-admitted",
            c.self_admitted.recent.len(),
            named(c.self_admitted.recent.clone(), 10),
            c.self_admitted.total
        ));
    }
    lines
}

/// The text form of the list.
pub fn render(r: &Report) -> String {
    let mut out = match r.total {
        0 => "nothing needs the owner\n".to_string(),
        n => format!(
            "{n} {} the owner ({}, {})\n",
            if n == 1 { "needs" } else { "need" },
            r.node,
            r.now
        ),
    };
    for (title, kinds) in SECTIONS {
        let in_section: u32 = kinds.iter().map(|k| r.counts.of(*k)).sum();
        if in_section == 0 {
            continue;
        }
        let _ = writeln!(out, "\n{title} ({in_section})");
        for kind in kinds {
            let mut shown = 0;
            for row in r.rows.iter().filter(|row| row.kind == *kind) {
                shown += 1;
                let _ = writeln!(out, "  {}", row.line);
                for (n, step) in row.next.iter().enumerate() {
                    let lead = if n == 0 { "    next: " } else { "          " };
                    let _ = writeln!(out, "{lead}{step}");
                }
            }
            let cut = r.counts.of(*kind).saturating_sub(shown);
            if cut > 0 {
                let _ = writeln!(out, "  and {cut} more: {}", kind.more());
            }
        }
    }
    let closing = closing_lines(&r.closing);
    if !closing.is_empty() {
        out.push('\n');
    }
    for line in closing {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// The one-line form: counts and fixed words only, so nothing an agent wrote reaches a
/// prompt or a status line. Empty when nothing is counted.
pub fn brief(r: &Report) -> String {
    if r.total == 0 {
        return String::new();
    }
    let parts: Vec<String> = SECTIONS
        .iter()
        .flat_map(|(_, kinds)| kinds.iter())
        .filter(|k| r.counts.of(**k) > 0)
        .map(|k| k.brief(r.counts.of(*k)))
        .collect();
    format!("needs you: {} ({})", r.total, parts.join(", "))
}

fn read(home: &Path, limit: u32) -> anyhow::Result<Report> {
    if !home.join(DB_FILE).exists() {
        anyhow::bail!(
            "no Another database in {}; run `another init` first",
            home.display()
        );
    }
    // Read only, as doctor opens it: nothing is created, migrated or swept.
    let store = Store::open_read_only(home, BUSY)?;
    report(&store, limit)
}

/// `another needs`. Rows never change the exit code: 0 when the list was read, 1 when it
/// could not be. A list that cannot be read is never an empty output: the reason goes to
/// standard error, and with `--brief` a fixed line goes to standard output.
pub fn run(home: &Path, json: bool, brief_only: bool, limit: u32) -> ExitCode {
    let mut report = match read(home, limit) {
        Ok(report) => report,
        Err(e) => {
            if brief_only {
                println!("{BRIEF_CANNOT_READ}");
            }
            eprintln!("another: {}", display_line(&format!("{e:#}"), 500));
            eprintln!("hint: run another doctor");
            return ExitCode::FAILURE;
        }
    };
    if brief_only {
        report.rows.clear();
    }
    crate::cli::emit(
        json,
        &report,
        |r| {
            if brief_only {
                brief(r)
            } else {
                render(r)
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_in_a_command_is_one_shell_word() {
        assert_eq!(word("claude@shop"), "claude@shop");
        assert_eq!(word("Q12"), "Q12");
        assert_eq!(word("a b"), "'a b'");
        assert_eq!(word("x;rm -rf ~"), "'x;rm -rf ~'");
        assert_eq!(word("it's"), "'it'\\''s'");
        assert_eq!(word("$(id)"), "'$(id)'");
        assert_eq!(word(""), "''");
        // What cannot be seen is gone before the quoting.
        assert_eq!(word("a\u{202e}b\u{1b}"), "'a b '");
    }

    #[test]
    fn free_text_in_a_command_is_quoted_once_as_a_whole() {
        assert_eq!(phrase("Who holds Q12?"), "\"Who holds Q12?\"");
        // What a shell reads inside double quotes moves the whole argument into single
        // quotes; nothing is ever quoted inside another pair of quotes.
        assert_eq!(phrase("Who holds $(touch x)?"), "'Who holds $(touch x)?'");
        assert_eq!(phrase("a `id` b"), "'a `id` b'");
        assert_eq!(phrase("say \"hi\""), "'say \"hi\"'");
        assert_eq!(phrase("it's $HOME"), "'it'\\''s $HOME'");
        assert_eq!(phrase("back\\slash"), "'back\\slash'");
        assert_eq!(phrase("a\u{1b}b"), "\"a b\"");
        // An item id that needs quoting, in both steps that put it inside free text.
        let ask = ask_step("cy", "Q1$(touch x)");
        assert!(
            ask.ends_with("--to cy --subject 'Who holds Q1$(touch x)?'"),
            "{ask}"
        );
        assert!(!ask.contains('"'), "{ask}");
    }

    #[test]
    fn a_delegate_step_swaps_only_the_owner_flag() {
        assert_eq!(
            as_delegate("another queue admit Q4 --owner"),
            "another queue admit Q4 --agent <delegate>"
        );
        assert_eq!(
            as_delegate("another queue show Q4"),
            "another queue show Q4"
        );
    }

    #[test]
    fn long_lists_are_named_up_to_a_number() {
        let items = |n: usize| (1..=n).map(|i| format!("Q{i}")).collect::<Vec<_>>();
        assert_eq!(named(items(2), 5), "Q1, Q2");
        assert_eq!(named(items(7), 5), "Q1, Q2, Q3, Q4, Q5 and 2 more");
    }
}
