//! Human decisions (docs/design/03-decisions.md): structured questions from agents to a
//! person, answered through the manager bot, the HTTP API or the CLI. A decision is a
//! request of kind `decision`; asking and answering are mail in one thread, so the answer
//! reaches the asking agent through its usual inbox and hooks.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{
    find_project_in, register_tx, resolve_agent, AgentRef, Program, RegisterAgent,
};
use crate::lanes::{
    check_scope, resolve_lane, resolve_lane_in_project, set_status, Lane, LaneStatus,
};
use crate::mail::{send_tx, touch, Importance, SendMessage};
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::{scope_allows_project, TokenScope};

/// Who is asked when a decision names nobody and no `decider` is configured
/// (`another config decider NAME`).
pub const DEFAULT_DECIDER: &str = "owner";
/// Longest note allowed next to a chosen option when free text is off.
pub const MAX_OPTION_NOTE: usize = 280;

const MAX_QUESTION: usize = 2000;
const MAX_OPTION_VALUE: usize = 64;
const MAX_OPTION_LABEL: usize = 200;
const MAX_ANSWER_TEXT: usize = 4000;
const MAX_LINKS: usize = 20;

/// The person asked by default.
pub(crate) fn decider_tx(conn: &Connection) -> Result<String> {
    crate::settings::get(conn, "decider")
}

fn check_answered_by(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.@".contains(&c))
    {
        Ok(())
    } else {
        Err(Error::Invalid("answered_by must be a name or address of 1-64 ASCII letters, digits, '-', '_', '.' or '@'".into()))
    }
}

impl Store {
    fn write_decision<T>(&self, mutate: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            tx.execute_batch("SAVEPOINT decision_mutation")?;
            match mutate(tx) {
                Ok(value) => {
                    tx.execute_batch("RELEASE decision_mutation")?;
                    Ok(Ok(value))
                }
                Err(
                    error @ (Error::Invalid(_)
                    | Error::NotFound(_)
                    | Error::Conflict(_)
                    | Error::Forbidden(_)
                    | Error::Ambiguous(_)),
                ) => {
                    tx.execute_batch("ROLLBACK TO decision_mutation; RELEASE decision_mutation")?;
                    Ok(Err(error))
                }
                Err(error) => Err(error),
            }
        })?
    }

    /// The person asked when a decision names nobody: the `decider` node setting, else
    /// [`DEFAULT_DECIDER`].
    pub fn default_decider(&self) -> Result<String> {
        self.read(decider_tx)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DecisionOption {
    /// Short token returned as the answer, e.g. `ship`.
    pub value: String,
    /// What the person sees, e.g. "Ship behind a flag".
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What happens when the deadline passes with no answer.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DecisionTimeout {
    /// The default option applies.
    ChooseDefault,
    /// The decision is cancelled; the agent proceeds without it.
    Cancel,
    /// One overdue reminder, then keep waiting.
    #[default]
    KeepOpen,
}

impl DecisionTimeout {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionTimeout::ChooseDefault => "choose_default",
            DecisionTimeout::Cancel => "cancel",
            DecisionTimeout::KeepOpen => "keep_open",
        }
    }
}

impl std::str::FromStr for DecisionTimeout {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "choose_default" | "default" => Ok(DecisionTimeout::ChooseDefault),
            "cancel" => Ok(DecisionTimeout::Cancel),
            "keep_open" | "wait" => Ok(DecisionTimeout::KeepOpen),
            other => Err(Error::Invalid(format!(
                "unknown on_timeout {other:?}; expected choose_default, cancel or keep_open"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Answered,
    Expired,
    Cancelled,
    /// Another node's decision: an answer was relayed to its node, which has not confirmed yet.
    AnswerSent,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionStatus::Open => "open",
            DecisionStatus::Answered => "answered",
            DecisionStatus::Expired => "expired",
            DecisionStatus::Cancelled => "cancelled",
            DecisionStatus::AnswerSent => "answer_sent",
        }
    }
}

impl std::str::FromStr for DecisionStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "open" => Ok(DecisionStatus::Open),
            "answered" => Ok(DecisionStatus::Answered),
            "expired" => Ok(DecisionStatus::Expired),
            "cancelled" => Ok(DecisionStatus::Cancelled),
            "answer_sent" => Ok(DecisionStatus::AnswerSent),
            other => Err(Error::Invalid(format!(
                "unknown decision status {other:?}; expected open, answered, expired or cancelled"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DecisionAnswer {
    /// The chosen option's value (none for a free-text-only answer or a cancelled timeout).
    pub value: Option<String>,
    pub label: Option<String>,
    pub text: Option<String>,
    /// The person who decided, or `timeout`.
    pub answered_by: String,
    /// The agent that relayed the answer through the API (e.g. the manager bot).
    pub relayed_by: Option<String>,
    pub answered_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Decision {
    pub decision_id: String,
    pub status: DecisionStatus,
    pub question: String,
    pub options: Vec<DecisionOption>,
    pub allow_free_text: bool,
    pub context_links: Vec<String>,
    pub deadline_at: Option<String>,
    pub on_timeout: DecisionTimeout,
    pub default_value: Option<String>,
    /// Who is asked.
    pub human: String,
    /// The agent that asked.
    pub asker: String,
    pub project: String,
    pub lane: Option<String>,
    pub answer: Option<DecisionAnswer>,
    pub request_id: String,
    pub thread_id: String,
    pub created_at: String,
    pub updated_at: String,
    /// The node the decision was asked on, when it is a mirror of another node's decision
    /// (answers are relayed there). `None` for this node's decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Input for [`Store::ask_decision`].
#[derive(Debug, Clone, Default)]
pub struct NewDecision {
    /// The asking agent.
    pub agent: String,
    pub question: String,
    pub options: Vec<DecisionOption>,
    pub allow_free_text: bool,
    pub context_links: Vec<String>,
    /// Deadline (ms since epoch); see [`parse_deadline`].
    pub deadline_at: Option<i64>,
    pub on_timeout: DecisionTimeout,
    /// Required with `choose_default`: one of the option values.
    pub default_value: Option<String>,
    /// Who to ask (default: the `decider` node setting, else `owner`).
    pub human: Option<String>,
    /// Project path or slug (default: the lane's, else the asking agent's).
    pub project: Option<String>,
    /// Lane id or name; it shows `needs_decision` while the decision is open.
    pub lane: Option<String>,
}

/// Input for [`Store::answer_decision`].
#[derive(Debug, Clone, Default)]
pub struct AnswerDecision {
    pub decision: String,
    pub value: Option<String>,
    pub text: Option<String>,
    /// The person who decided (default: the decision's human).
    pub answered_by: Option<String>,
    /// The agent relaying the answer (remote callers: the token's agent).
    pub relayed_by: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DecisionFilter {
    /// Only this status (default: open).
    pub status: Option<DecisionStatus>,
    /// Every status.
    pub all: bool,
    /// Project path or slug.
    pub project: Option<String>,
    pub lane: Option<String>,
    pub human: Option<String>,
    pub limit: u32,
}

/// Parse a deadline: a duration (`30m`, `2h`, `1d`) from `now`, or an RFC 3339 time.
pub fn parse_deadline(raw: &str, now: i64) -> Result<i64> {
    let raw = raw.trim();
    let at = if let Some(ms) = crate::activity::parse_duration_ms(raw) {
        now + ms
    } else if let Ok(t) = chrono::DateTime::parse_from_rfc3339(raw) {
        t.timestamp_millis()
    } else {
        return Err(Error::Invalid(format!(
            "invalid deadline {raw:?}: use a duration like 2h or an RFC 3339 time"
        )));
    };
    if at <= now {
        return Err(Error::Invalid(format!("deadline {raw:?} is in the past")));
    }
    Ok(at)
}

fn check_text(what: &str, s: &str, max: usize) -> Result<()> {
    if s.trim().is_empty() {
        return Err(Error::Invalid(format!("{what} must not be empty")));
    }
    if s.chars().count() > max {
        return Err(Error::Invalid(format!(
            "{what} is longer than {max} characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate(input: &NewDecision) -> Result<()> {
    check_text("question", &input.question, MAX_QUESTION)?;
    if !(2..=6).contains(&input.options.len()) {
        return Err(Error::Invalid(format!(
            "a decision needs 2 to 6 options (got {})",
            input.options.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::new();
    for o in &input.options {
        check_text("option value", &o.value, MAX_OPTION_VALUE)?;
        check_text("option label", &o.label, MAX_OPTION_LABEL)?;
        if o.value.chars().any(char::is_whitespace) {
            return Err(Error::Invalid(format!(
                "option value {:?} must be a single word (use the label for text)",
                o.value
            )));
        }
        if seen.contains(&o.value.as_str()) {
            return Err(Error::Invalid(format!(
                "duplicate option value {:?}",
                o.value
            )));
        }
        seen.push(&o.value);
    }
    if input.context_links.len() > MAX_LINKS {
        return Err(Error::Invalid(format!("at most {MAX_LINKS} context links")));
    }
    match (input.on_timeout, &input.default_value) {
        (DecisionTimeout::ChooseDefault, None) => {
            return Err(Error::Invalid(
                "on_timeout choose_default needs a default (one of the option values)".into(),
            ))
        }
        (_, Some(d)) if !seen.contains(&d.as_str()) => {
            return Err(Error::Invalid(format!(
                "default {d:?} is not one of the option values"
            )))
        }
        _ => {}
    }
    if input.on_timeout != DecisionTimeout::KeepOpen && input.deadline_at.is_none() {
        return Err(Error::Invalid(format!(
            "on_timeout {} needs a deadline",
            input.on_timeout.as_str()
        )));
    }
    Ok(())
}

fn short(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or_default();
    if line.chars().count() <= n {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(n - 1).collect::<String>())
    }
}

/// An agent by id (decisions store ids).
fn agent_by_id(conn: &Connection, id: &str) -> Result<AgentRef> {
    Ok(conn.query_row(
        "SELECT agent_id, address, node_id FROM agents WHERE agent_id = ?1",
        [id],
        |r| {
            Ok(AgentRef {
                id: r.get(0)?,
                address: r.get(1)?,
                node_id: r.get(2)?,
            })
        },
    )?)
}

/// The person as an agent: found by name among project-less agents, else registered.
fn ensure_human(tx: &Tx<'_>, name: &str) -> Result<AgentRef> {
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT agent_id, program FROM agents WHERE node_id = ?1 AND name = ?2 AND project_id IS NULL",
            params![tx.node_id, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, program)) = existing {
        // Decisions go to people. An agent with that name is not one, and making it one is
        // the person's call on this machine.
        if program != Program::Human.as_str() {
            return Err(Error::Invalid(format!(
                "{name} is registered as a {program} agent, not a person (program human): name another person, or re-register it with `another agent register {name} --program human`"
            )));
        }
        return agent_by_id(tx, &id);
    }
    let agent = register_tx(
        tx,
        &RegisterAgent {
            name: name.to_string(),
            program: Program::Human,
            project: None,
            model: None,
            description: Some("a person who answers decisions".into()),
        },
    )?;
    agent_by_id(tx, &agent.agent_id)
}

const DECISION_SELECT: &str = "SELECT d.decision_id, d.status, d.question, d.options,
       d.allow_free_text, d.context_links, d.deadline_at, d.on_timeout, d.default_value,
       h.address, a.address, p.slug, l.name, d.answer_value, d.answer_text, d.answered_by,
       d.relayed_by, d.answered_at, d.request_id, d.thread_id, d.created_at, d.updated_at,
       d.project_id, d.lane_id, d.asker_id, d.human_id, d.message_id
     FROM decisions d
     JOIN agents h ON h.agent_id = d.human_id
     JOIN agents a ON a.agent_id = d.asker_id
     JOIN projects p ON p.project_id = d.project_id
     LEFT JOIN lanes l ON l.lane_id = d.lane_id";

/// A decision plus the ids the store needs internally.
struct Row0 {
    d: Decision,
    project_id: String,
    lane_id: Option<String>,
    asker_id: String,
    human_id: String,
    message_id: String,
}

fn decision_from_row(r: &Row<'_>) -> rusqlite::Result<Row0> {
    let options: Vec<DecisionOption> =
        serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
    let links: Vec<String> = serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default();
    let status: DecisionStatus = r
        .get::<_, String>(1)?
        .parse()
        .unwrap_or(DecisionStatus::Open);
    let on_timeout: DecisionTimeout = r.get::<_, String>(7)?.parse().unwrap_or_default();
    let value: Option<String> = r.get(13)?;
    let answered_at: Option<i64> = r.get(17)?;
    let answer = answered_at.map(|at| DecisionAnswer {
        label: value
            .as_ref()
            .and_then(|v| options.iter().find(|o| &o.value == v))
            .map(|o| o.label.clone()),
        value: value.clone(),
        text: r.get(14).ok().flatten(),
        answered_by: r
            .get::<_, Option<String>>(15)
            .ok()
            .flatten()
            .unwrap_or_default(),
        relayed_by: r.get(16).ok().flatten(),
        answered_at: fmt_ts(at),
    });
    Ok(Row0 {
        d: Decision {
            decision_id: r.get(0)?,
            status,
            question: r.get(2)?,
            options,
            allow_free_text: r.get(4)?,
            context_links: links,
            deadline_at: r.get::<_, Option<i64>>(6)?.map(fmt_ts),
            on_timeout,
            default_value: r.get(8)?,
            human: r.get(9)?,
            asker: r.get(10)?,
            project: r.get(11)?,
            lane: r.get(12)?,
            answer,
            request_id: r.get(18)?,
            thread_id: r.get(19)?,
            created_at: fmt_ts(r.get(20)?),
            updated_at: fmt_ts(r.get(21)?),
            node: None,
        },
        project_id: r.get(22)?,
        lane_id: r.get(23)?,
        asker_id: r.get(24)?,
        human_id: r.get(25)?,
        message_id: r.get(26)?,
    })
}

fn load(conn: &Connection, node_id: &str, key: &str) -> Result<Row0> {
    let found = conn
        .query_row(
            &format!("{DECISION_SELECT} WHERE d.node_id = ?1 AND (d.decision_id = ?2 OR d.request_id = ?2)"),
            params![node_id, key.trim().to_ascii_uppercase()],
            decision_from_row,
        )
        .optional()?;
    found.ok_or_else(|| Error::NotFound(format!("no decision {key:?}")))
}

/// [`load`] as `scope` sees it: a decision in a project outside the token's globs is the
/// same `not_found` (same message) as one that doesn't exist.
fn load_in(
    conn: &Connection,
    node_id: &str,
    key: &str,
    scope: Option<&TokenScope>,
) -> Result<Row0> {
    let row = load(conn, node_id, key)?;
    if let Some(s) = scope {
        if !scope_allows_project(conn, s, &row.project_id)? {
            return Err(Error::NotFound(format!("no decision {key:?}")));
        }
    }
    Ok(row)
}

/// Only a manager bot (`grok_bot`) or a person (`human`) may relay an answer, and never
/// the agent that asked: an agent can't approve its own request (decisions, spend).
pub(crate) fn check_relayer(conn: &Connection, relayer: &AgentRef, asker_id: &str) -> Result<()> {
    if relayer.id == asker_id {
        return Err(Error::Forbidden(
            "the asking agent cannot answer its own decision".into(),
        ));
    }
    let program: String = conn.query_row(
        "SELECT program FROM agents WHERE agent_id = ?1",
        [&relayer.id],
        |r| r.get(0),
    )?;
    if !matches!(program.as_str(), "grok_bot" | "human") {
        return Err(Error::Forbidden(format!(
            "{} ({program}) cannot relay answers: only a manager (grok_bot) or a person (human) can",
            relayer.address
        )));
    }
    // The role must have been confirmed on this machine: a person created on demand as a
    // decision's addressee (or any role an agent gave itself) can't relay.
    if !crate::identity::role_confirmed(conn, &relayer.id, &program)? {
        return Err(Error::Forbidden(format!(
            "{} is not a confirmed {program}: confirm it on this machine with `another agent register {} --program {program}`",
            relayer.address,
            relayer.address.split('@').next().unwrap_or(&relayer.address)
        )));
    }
    Ok(())
}

fn render_question(input: &NewDecision, decision_id: &str, human: &str) -> String {
    let mut out = format!("{}\n", input.question.trim());
    out.push_str("\nOptions:\n");
    for (i, o) in input.options.iter().enumerate() {
        out.push_str(&format!("{}. {} [{}]", i + 1, o.label, o.value));
        if let Some(d) = &o.description {
            out.push_str(&format!(" — {d}"));
        }
        out.push('\n');
    }
    if input.allow_free_text {
        out.push_str("(A free-text answer is also accepted.)\n");
    }
    for l in &input.context_links {
        out.push_str(&format!("Link: {l}\n"));
    }
    if let Some(at) = input.deadline_at {
        out.push_str(&format!(
            "Deadline: {} (then: {})\n",
            fmt_ts(at),
            timeout_text(input.on_timeout, input.default_value.as_deref())
        ));
    }
    out.push_str(&format!(
        "\nAnswer: another decision answer {decision_id} <value>   (for {human})\n(decision {decision_id})"
    ));
    out
}

fn timeout_text(t: DecisionTimeout, default: Option<&str>) -> String {
    match t {
        DecisionTimeout::ChooseDefault => format!("default {}", default.unwrap_or("?")),
        DecisionTimeout::Cancel => "cancel".into(),
        DecisionTimeout::KeepOpen => "keep waiting".into(),
    }
}

/// After a decision closes: a lane left in `needs_decision` with no other open decision goes
/// back to `working`.
fn release_lane(tx: &Tx<'_>, actor: &AgentRef, lane_id: Option<&str>) -> Result<()> {
    let Some(lane_id) = lane_id else {
        return Ok(());
    };
    let others: i64 = tx.query_row(
        "SELECT COUNT(*) FROM decisions WHERE lane_id = ?1 AND status = 'open'",
        [lane_id],
        |r| r.get(0),
    )?;
    if others > 0 {
        return Ok(());
    }
    let (lane, project_id): (Lane, String) = resolve_lane(tx, tx.node_id, lane_id)?;
    if lane.node.is_some() {
        // Never write to another node's lane mirror.
        return Ok(());
    }
    if lane.status == LaneStatus::NeedsDecision {
        set_status(tx, actor, &lane, &project_id, LaneStatus::Working)?;
    }
    Ok(())
}

/// Withdraw an open decision: event, status, mail to the person, request, waiting spend lease,
/// lane status.
fn cancel_row_tx(tx: &Tx<'_>, me: &AgentRef, row: &Row0, reason: Option<&str>) -> Result<()> {
    tx.emit(
        &me.address,
        "decision.cancelled",
        json!({ "decision_id": row.d.decision_id, "project_id": row.project_id, "asker_id": row.asker_id, "lane_id": row.lane_id, "has_reason": reason.is_some() }),
    )?;
    tx.execute(
        "UPDATE decisions SET status = 'cancelled', updated_at = ?2 WHERE decision_id = ?1",
        params![row.d.decision_id, tx.now],
    )?;
    let human = agent_by_id(tx, &row.human_id)?;
    send_tx(
        tx,
        &SendMessage {
            from: me.address.clone(),
            to: vec![human.address.clone()],
            subject: format!("[decision] withdrawn ({})", row.d.decision_id),
            body: format!(
                "No answer needed any more.{}\n(decision {})",
                reason.map(|r| format!(" Reason: {r}")).unwrap_or_default(),
                row.d.decision_id
            ),
            reply_to: Some(row.message_id.clone()),
            ..Default::default()
        },
    )?;
    close_request(
        tx,
        &row.d.request_id,
        "cancelled",
        None,
        reason.unwrap_or("withdrawn"),
    )?;
    crate::spend::decision_closed_tx(tx, &row.d.decision_id, false)?;
    release_lane(tx, me, row.lane_id.as_deref())?;
    Ok(())
}

/// Withdraw a decision if it is still open (used when its spend lease is withdrawn).
pub(crate) fn withdraw_tx(
    tx: &Tx<'_>,
    me: &AgentRef,
    decision_id: &str,
    reason: &str,
) -> Result<bool> {
    let row = load(tx, tx.node_id, decision_id)?;
    if row.d.status != DecisionStatus::Open {
        return Ok(false);
    }
    cancel_row_tx(tx, me, &row, Some(reason))?;
    Ok(true)
}

fn close_request(
    tx: &Tx<'_>,
    request_id: &str,
    status: &str,
    owner: Option<&str>,
    note: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE requests SET status = ?2, owner_id = COALESCE(?3, owner_id), note = ?4,
           updated_at = ?5, closed_at = ?5 WHERE request_id = ?1",
        params![request_id, status, owner, note, tx.now],
    )?;
    Ok(())
}

/// Close decisions whose deadline passed (called by the sweeper).
pub(crate) fn expire_decisions_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "{DECISION_SELECT} WHERE d.node_id = ?1 AND d.status = 'open'
           AND d.deadline_at IS NOT NULL AND d.deadline_at <= ?2
           AND NOT (d.on_timeout = 'keep_open' AND d.overdue_at IS NOT NULL)"
    ))?;
    let due = stmt
        .query_map(params![tx.node_id, tx.now], decision_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &due {
        let d = &row.d;
        let human = agent_by_id(tx, &row.human_id)?;
        let asker = agent_by_id(tx, &row.asker_id)?;
        if d.on_timeout == DecisionTimeout::KeepOpen {
            tx.emit(
                "system",
                "decision.overdue",
                json!({ "decision_id": d.decision_id, "project_id": row.project_id, "asker_id": row.asker_id, "lane_id": row.lane_id }),
            )?;
            tx.execute(
                "UPDATE decisions SET overdue_at = ?2, updated_at = ?2 WHERE decision_id = ?1",
                params![d.decision_id, tx.now],
            )?;
            let system = crate::identity::system_agent_tx(tx)?;
            send_tx(
                tx,
                &SendMessage {
                    from: system.address.clone(),
                    to: vec![human.address.clone()],
                    subject: format!("[decision] overdue ({})", d.decision_id),
                    body: format!(
                        "Still waiting for an answer (deadline was {}).\n\nAnswer: another decision answer {} <value>",
                        d.deadline_at.clone().unwrap_or_default(),
                        d.decision_id
                    ),
                    importance: Importance::High,
                    thread_id: Some(d.thread_id.clone()),
                    ..Default::default()
                },
            )?;
            continue;
        }
        let applied = match d.on_timeout {
            DecisionTimeout::ChooseDefault => d.default_value.clone(),
            _ => None,
        };
        tx.emit(
            "system",
            "decision.expired",
            json!({
                "decision_id": d.decision_id,
                "project_id": row.project_id,
                "asker_id": row.asker_id,
                "lane_id": row.lane_id,
                "on_timeout": d.on_timeout.as_str(),
                "value": applied,
            }),
        )?;
        tx.execute(
            "UPDATE decisions SET status = 'expired', answer_value = ?2,
               answered_by = CASE WHEN ?2 IS NULL THEN NULL ELSE 'timeout' END,
               answered_at = CASE WHEN ?2 IS NULL THEN NULL ELSE ?3 END, updated_at = ?3
             WHERE decision_id = ?1",
            params![d.decision_id, applied, tx.now],
        )?;
        let (subject, body) = match &applied {
            Some(v) => {
                let label = d
                    .options
                    .iter()
                    .find(|o| &o.value == v)
                    .map_or(v.as_str(), |o| o.label.as_str());
                (
                    format!("[decision] default applied: {v} ({})", d.decision_id),
                    format!(
                        "No answer by the deadline; the default applies.\n\nValue: {v}\nLabel: {label}\n\n(automatic; decision {})",
                        d.decision_id
                    ),
                )
            }
            None => (
                format!("[decision] timed out, cancelled ({})", d.decision_id),
                format!(
                    "No answer by the deadline; the decision is cancelled. Proceed without it.\n\n(automatic; decision {})",
                    d.decision_id
                ),
            ),
        };
        // Automatic mail comes from the system agent, never in the person's name.
        let system = crate::identity::system_agent_tx(tx)?;
        let msg = send_tx(
            tx,
            &SendMessage {
                from: system.address.clone(),
                to: vec![asker.address.clone()],
                subject,
                body,
                importance: Importance::High,
                thread_id: Some(d.thread_id.clone()),
                ..Default::default()
            },
        )?;
        tx.execute(
            "UPDATE decisions SET answer_message_id = ?2 WHERE decision_id = ?1",
            params![d.decision_id, msg.message_id],
        )?;
        close_request(
            tx,
            &d.request_id,
            if applied.is_some() {
                "done"
            } else {
                "cancelled"
            },
            None,
            "deadline passed",
        )?;
        crate::spend::decision_closed_tx(
            tx,
            &d.decision_id,
            applied.as_deref() == Some("approve"),
        )?;
        release_lane(tx, &asker, row.lane_id.as_deref())?;
    }
    Ok(due.len())
}

/// Ask a decision inside a write transaction (validated input). Returns the decision id.
pub(crate) fn ask_tx(
    tx: &Tx<'_>,
    input: &NewDecision,
    human_name: &str,
    scope: Option<&TokenScope>,
) -> Result<String> {
    let asker = resolve_agent(tx, tx.node_id, &input.agent)?;
    if human_name == crate::identity::SYSTEM_AGENT {
        return Err(Error::Invalid(
            "decisions are for people, not the system agent".into(),
        ));
    }
    let human = ensure_human(tx, human_name)?;
    if human.id == asker.id {
        return Err(Error::Invalid(
            "an agent cannot ask itself for a decision".into(),
        ));
    }
    // The project comes first (explicit, else the asker's) and is scope-checked before any
    // lane lookup; the lane is then resolved inside that project only, so a lane elsewhere,
    // out of scope or missing is the same "unknown lane".
    let home: Option<String> = match &input.project {
        Some(p) => Some(find_project_in(tx, tx.node_id, p, scope)?.project_id),
        None => tx.query_row(
            "SELECT project_id FROM agents WHERE agent_id = ?1",
            [&asker.id],
            |r| r.get::<_, Option<String>>(0),
        )?,
    };
    if let Some(p) = &home {
        check_scope(tx, scope, p)?;
    }
    let lane = match &input.lane {
        Some(l) => Some(resolve_lane_in_project(
            tx,
            tx.node_id,
            l,
            home.as_deref(),
            scope,
        )?),
        None => None,
    };
    // A mirrored lane belongs to another node: asking here would flip only the mirror to
    // `needs_decision`. The owner node is checked before anything else about the lane.
    if let Some((l, _)) = &lane {
        if let Some(node) = &l.node {
            return Err(Error::Conflict(format!(
                "lane {} is owned by node {node}: ask the decision there (its MCP / HTTP API)",
                l.name
            )));
        }
    }
    let project_id = match (home, &lane) {
        (Some(p), _) => p,
        (None, Some((_, lane_project))) => lane_project.clone(),
        (None, None) => {
            return Err(Error::Invalid(
                "name a project (or a lane): decisions belong to a project".into(),
            ))
        }
    };
    check_scope(tx, scope, &project_id)?;
    if let Some((l, _)) = &lane {
        if l.status.is_closed() {
            return Err(Error::Conflict(format!("lane {} is closed", l.name)));
        }
    }
    let decision_id = new_id();
    let msg = send_tx(
        tx,
        &SendMessage {
            from: asker.address.clone(),
            to: vec![human.address.clone()],
            // Subjects show in the activity feed (and federate): ids and names only; the
            // question is in the body.
            subject: format!(
                "[decision] {} asks {} ({decision_id})",
                asker.address, human.address
            ),
            body: render_question(input, &decision_id, &human.address),
            importance: Importance::High,
            ..Default::default()
        },
    )?;
    let request_id = new_id();
    tx.execute(
        "INSERT INTO requests (request_id, node_id, message_id, thread_id, title,
           created_by, status, created_at, updated_at, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?7, 'decision')",
        params![
            request_id,
            tx.node_id,
            msg.message_id,
            msg.thread_id,
            short(&input.question, 200),
            asker.id,
            tx.now
        ],
    )?;
    let lane_id = lane.as_ref().map(|(l, _)| l.lane_id.clone());
    let event_id = tx.emit(
        &asker.address,
        "decision.created",
        json!({
            "decision_id": decision_id,
            "request_id": request_id,
            "project_id": project_id,
            "lane_id": lane_id,
            "asker_id": asker.id,
            "human_id": human.id,
            // Ids and status only: the question and option labels stay in the decisions
            // table and the question mail (D39).
            "option_count": input.options.len(),
            // Option values are single-word identifiers (like an answer's `value`), so
            // other nodes can validate answers to their mirror; labels stay here.
            "option_values": input.options.iter().map(|o| o.value.clone()).collect::<Vec<_>>(),
            "message_id": msg.message_id,
            "allow_free_text": input.allow_free_text,
            "deadline_at": input.deadline_at,
            "on_timeout": input.on_timeout.as_str(),
        }),
    )?;
    tx.execute(
        "INSERT INTO decisions (decision_id, node_id, project_id, lane_id, asker_id,
           human_id, question, options, allow_free_text, context_links, deadline_at,
           on_timeout, default_value, status, request_id, message_id, thread_id,
           created_at, updated_at, event_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'open', ?14, ?15,
           ?16, ?17, ?17, ?18)",
        params![
            decision_id,
            tx.node_id,
            project_id,
            lane_id,
            asker.id,
            human.id,
            input.question,
            serde_json::to_string(&input.options)?,
            input.allow_free_text,
            serde_json::to_string(&input.context_links)?,
            input.deadline_at,
            input.on_timeout.as_str(),
            input.default_value,
            request_id,
            msg.message_id,
            msg.thread_id,
            tx.now,
            event_id
        ],
    )?;
    if let Some((l, lp)) = &lane {
        if l.status != LaneStatus::NeedsDecision {
            set_status(tx, &asker, l, lp, LaneStatus::NeedsDecision)?;
        }
    }
    touch(tx, &asker.id)?;
    Ok(decision_id)
}

/// Answer a local decision inside a write transaction. `relayed` is the agent that relayed
/// the answer (local API caller, or another node's agent for a relayed answer).
pub(crate) fn answer_tx(
    tx: &Tx<'_>,
    input: &AnswerDecision,
    relayed: Option<AgentRef>,
    scope: Option<&TokenScope>,
) -> Result<Decision> {
    let row = load_in(tx, tx.node_id, &input.decision, scope)?;
    let d = &row.d;
    if d.status != DecisionStatus::Open {
        return Err(Error::Conflict(format!(
            "decision {} is already {}",
            d.decision_id,
            d.status.as_str()
        )));
    }
    let value = input
        .value
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let text = input
        .text
        .as_ref()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let option = match &value {
        Some(v) => Some(
            d.options
                .iter()
                // An exact match wins over a case-insensitive one ("a" vs "A").
                .find(|o| o.value == *v)
                .or_else(|| d.options.iter().find(|o| o.value.eq_ignore_ascii_case(v)))
                .or_else(|| {
                    // Accept the option number shown on the phone (1-based).
                    v.parse::<usize>()
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| d.options.get(i))
                })
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "{v:?} is not an option; choose one of {}",
                        d.options
                            .iter()
                            .map(|o| o.value.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?
                .clone(),
        ),
        None => None,
    };
    if let Some(t) = &text {
        if d.allow_free_text {
            check_text("answer text", t, MAX_ANSWER_TEXT)?;
        } else if option.is_some() {
            // A short note next to the chosen option is always allowed.
            check_text("note", t, MAX_OPTION_NOTE)?;
        } else {
            return Err(Error::Invalid(
                "this decision does not accept free text; choose an option (a short note may go with it)".into(),
            ));
        }
    }
    if option.is_none() && text.is_none() {
        return Err(Error::Invalid(if d.allow_free_text {
            "give an option value or free text".into()
        } else {
            "give an option value".into()
        }));
    }
    let answered_by = input
        .answered_by
        .as_ref()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| d.human.clone());
    check_answered_by(&answered_by)?;
    if let Some(r) = &relayed {
        check_relayer(tx, r, &row.asker_id)?;
    }
    let human = agent_by_id(tx, &row.human_id)?;
    let asker = agent_by_id(tx, &row.asker_id)?;
    tx.emit(
        relayed
            .as_ref()
            .map_or(answered_by.as_str(), |r| r.address.as_str()),
        "decision.answered",
        json!({
            "decision_id": d.decision_id,
            "project_id": row.project_id,
            "lane_id": row.lane_id,
            "asker_id": row.asker_id,
            "value": option.as_ref().map(|o| o.value.clone()),
            "has_text": text.is_some(),
            "answered_by": answered_by,
            "relayed_by": relayed.as_ref().map(|r| r.address.clone()),
        }),
    )?;
    let mut body = String::new();
    if let Some(o) = &option {
        body.push_str(&format!("Value: {}\nLabel: {}\n", o.value, o.label));
    }
    if let Some(t) = &text {
        body.push_str(&format!("\n{t}\n"));
    }
    body.push_str(&format!("\nAnswered by {answered_by}"));
    if let Some(r) = &relayed {
        body.push_str(&format!(" (relayed by {})", r.address));
    }
    body.push_str(&format!(
        " at {}.\n(decision {})",
        fmt_ts(tx.now),
        d.decision_id
    ));
    let subject = match &option {
        Some(o) => format!("[decision] answered: {} ({})", o.value, d.decision_id),
        None => format!("[decision] answered ({})", d.decision_id),
    };
    let msg = send_tx(
        tx,
        &SendMessage {
            from: human.address.clone(),
            to: vec![asker.address.clone()],
            subject,
            body,
            importance: Importance::High,
            reply_to: Some(row.message_id.clone()),
            ..Default::default()
        },
    )?;
    tx.execute(
        "UPDATE decisions SET status = 'answered', answer_value = ?2, answer_text = ?3,
           answered_by = ?4, relayed_by = ?5, answered_at = ?6, updated_at = ?6,
           answer_message_id = ?7
         WHERE decision_id = ?1",
        params![
            d.decision_id,
            option.as_ref().map(|o| o.value.clone()),
            text,
            answered_by,
            relayed.as_ref().map(|r| r.address.clone()),
            tx.now,
            msg.message_id
        ],
    )?;
    close_request(
        tx,
        &d.request_id,
        "done",
        Some(&human.id),
        &format!(
            "answered by {answered_by}: {}",
            option.as_ref().map_or("free text", |o| o.value.as_str())
        ),
    )?;
    crate::spend::decision_closed_tx(
        tx,
        &d.decision_id,
        option.as_ref().is_some_and(|o| o.value == "approve"),
    )?;
    release_lane(
        tx,
        relayed.as_ref().unwrap_or(&human),
        row.lane_id.as_deref(),
    )?;
    Ok(load(tx, tx.node_id, &d.decision_id)?.d)
}

// ---------------------------------------------------------------------------
// Federation: mirrors of other nodes' decisions, relayed answers (07-remote-agents.md §5)

const MIRROR_SELECT: &str =
    "SELECT m.decision_id, m.status, m.question, m.options, m.allow_free_text,
       m.deadline_at, m.human, a.address, p.slug, l.name, m.answer_value, m.answered_by,
       m.relayed_by, m.answered_at, m.created_at, m.updated_at, n.display_name, m.project_id,
       m.node_id
     FROM remote_decisions m
     LEFT JOIN agents a ON a.agent_id = m.asker_id
     LEFT JOIN projects p ON p.project_id = m.project_id
     LEFT JOIN lanes l ON l.lane_id = m.lane_id
     LEFT JOIN nodes n ON n.node_id = m.node_id";

/// A mirrored decision, with its project id and origin node id.
fn mirror_from_row(r: &Row<'_>) -> rusqlite::Result<(Decision, Option<String>, String)> {
    let options: Vec<DecisionOption> =
        serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
    let value: Option<String> = r.get(10)?;
    let answered_at: Option<i64> = r.get(13)?;
    let answer = answered_at.map(|at| DecisionAnswer {
        label: value
            .as_ref()
            .and_then(|v| options.iter().find(|o| &o.value == v))
            .map(|o| o.label.clone()),
        value: value.clone(),
        text: None,
        answered_by: r
            .get::<_, Option<String>>(11)
            .ok()
            .flatten()
            .unwrap_or_default(),
        relayed_by: r.get(12).ok().flatten(),
        answered_at: fmt_ts(at),
    });
    Ok((
        Decision {
            decision_id: r.get(0)?,
            status: r
                .get::<_, String>(1)?
                .parse()
                .unwrap_or(DecisionStatus::Open),
            question: r.get(2)?,
            options,
            allow_free_text: r.get(4)?,
            context_links: vec![],
            deadline_at: r.get::<_, Option<i64>>(5)?.map(fmt_ts),
            on_timeout: DecisionTimeout::KeepOpen,
            default_value: None,
            human: r.get(6)?,
            asker: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            project: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            lane: r.get(9)?,
            answer,
            request_id: String::new(),
            thread_id: String::new(),
            created_at: fmt_ts(r.get(14)?),
            updated_at: fmt_ts(r.get(15)?),
            node: Some(
                r.get::<_, Option<String>>(16)?
                    .unwrap_or_else(|| r.get(18).unwrap_or_default()),
            ),
        },
        r.get(17)?,
        r.get(18)?,
    ))
}

fn load_mirror(conn: &Connection, key: &str) -> Result<Option<(Decision, Option<String>, String)>> {
    Ok(conn
        .query_row(
            &format!("{MIRROR_SELECT} WHERE m.decision_id = ?1"),
            [key.trim().to_ascii_uppercase()],
            mirror_from_row,
        )
        .optional()?)
}

/// Store another node's `decision.created` as a mirror.
pub(crate) fn mirror_created_tx(
    tx: &Tx<'_>,
    ev: &crate::federation::WireEvent,
    p: &serde_json::Value,
) -> Result<()> {
    let s = |k: &str| {
        p.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let Some(id) = s("decision_id") else {
        return Ok(());
    };
    // Events carry no question text: use the question mail if it was delivered here (the
    // person lives on this node), else point at the decision's node.
    let question: String = match s("message_id") {
        Some(mid) => tx
            .query_row(
                "SELECT body FROM messages WHERE message_id = ?1",
                [&mid],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    }
    .or_else(|| s("question"))
    .unwrap_or_else(|| {
        format!(
            "(question asked on node {}; see the decision mail)",
            ev.node_id
        )
    });
    let options: Vec<DecisionOption> = match p.get("option_values").and_then(|v| v.as_array()) {
        Some(values) => values
            .iter()
            .filter_map(|v| v.as_str())
            .map(|v| DecisionOption {
                value: v.to_string(),
                label: v.to_string(),
                description: None,
            })
            .collect(),
        None => p
            .get("options")
            .and_then(|o| serde_json::from_value(o.clone()).ok())
            .unwrap_or_default(),
    };
    let human: String = match s("human_id") {
        Some(hid) => tx
            .query_row(
                "SELECT address FROM agents WHERE agent_id = ?1",
                [&hid],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    }
    .or_else(|| s("human"))
    .unwrap_or_default();
    let known = |table: &str, col: &str, v: &Option<String>| -> Result<Option<String>> {
        match v {
            Some(v) => Ok(tx
                .query_row(
                    &format!("SELECT 1 FROM {table} WHERE {col} = ?1"),
                    [v],
                    |_| Ok(()),
                )
                .optional()?
                .map(|_| v.clone())),
            None => Ok(None),
        }
    };
    let project = known("projects", "project_id", &s("project_id"))?;
    let lane = known("lanes", "lane_id", &s("lane_id"))?;
    let asker = known("agents", "agent_id", &s("asker_id"))?;
    tx.execute(
        "INSERT OR IGNORE INTO remote_decisions (decision_id, node_id, project_id, lane_id,
           asker_id, human, question, options, allow_free_text, deadline_at, status, created_at,
           updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'open', ?11, ?11)",
        params![
            id,
            ev.node_id,
            project,
            lane,
            asker,
            human,
            question,
            serde_json::to_string(&options)?,
            p.get("allow_free_text")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            p.get("deadline_at").and_then(serde_json::Value::as_i64),
            ev.ts
        ],
    )?;
    Ok(())
}

/// The origin closed a mirrored decision (answered, expired, cancelled).
pub(crate) fn mirror_closed_tx(
    tx: &Tx<'_>,
    ev: &crate::federation::WireEvent,
    p: &serde_json::Value,
) -> Result<()> {
    let s = |k: &str| {
        p.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let status = ev.kind.trim_start_matches("decision.");
    tx.execute(
        "UPDATE remote_decisions SET status = ?2, answer_value = ?3, answered_by = ?4,
           relayed_by = ?5, answered_at = CASE WHEN ?3 IS NULL THEN answered_at ELSE ?6 END,
           updated_at = ?6
         WHERE decision_id = ?1 AND node_id = ?7",
        params![
            s("decision_id"),
            status,
            s("value"),
            s("answered_by"),
            s("relayed_by"),
            ev.ts,
            ev.node_id
        ],
    )?;
    Ok(())
}

/// Answering another node's decision: validate against the mirror, mark it, and relay the
/// answer to the decision's node only. Returns `None` for local decisions.
fn relay_answer_tx(
    tx: &Tx<'_>,
    input: &AnswerDecision,
    scope: Option<&TokenScope>,
) -> Result<Option<Decision>> {
    if load(tx, tx.node_id, &input.decision).is_ok() {
        return Ok(None);
    }
    let Some((d, project_id, origin)) = load_mirror(tx, &input.decision)? else {
        return Ok(None);
    };
    // A hidden mirror is a missing decision, as for local ones.
    let hidden = match (&project_id, scope) {
        (Some(p), Some(s)) => !scope_allows_project(tx, s, p)?,
        (None, Some(_)) => true,
        _ => false,
    };
    if hidden {
        return Err(Error::NotFound(format!("no decision {:?}", input.decision)));
    }
    if d.status != DecisionStatus::Open {
        return Err(Error::Conflict(format!(
            "decision {} is already {}",
            d.decision_id,
            d.status.as_str()
        )));
    }
    let value = input
        .value
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let text = input
        .text
        .as_ref()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let option = match &value {
        Some(v) => Some(
            d.options
                .iter()
                .find(|o| o.value == *v)
                .or_else(|| d.options.iter().find(|o| o.value.eq_ignore_ascii_case(v)))
                .or_else(|| {
                    v.parse::<usize>()
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| d.options.get(i))
                })
                .ok_or_else(|| Error::Invalid(format!("{v:?} is not an option")))?
                .clone(),
        ),
        None => None,
    };
    // Same rules as a local answer, checked before anything is marked or sent.
    if let Some(t) = &text {
        if d.allow_free_text {
            check_text("answer text", t, MAX_ANSWER_TEXT)?;
        } else if option.is_some() {
            check_text("note", t, MAX_OPTION_NOTE)?;
        } else {
            return Err(Error::Invalid(
                "this decision does not accept free text; choose an option (a short note may go with it)".into(),
            ));
        }
    }
    if option.is_none() && text.is_none() {
        return Err(Error::Invalid("give an option value".into()));
    }
    // Another node's decision is answered by someone here: the relaying agent (or the person
    // answering, when they are registered here). The origin only accepts a relayer of this
    // node, so one is required.
    let relayed_ref = match (&input.relayed_by, &input.answered_by) {
        (Some(r), _) => Some(resolve_agent(tx, tx.node_id, r)?),
        (None, Some(a)) => resolve_agent(tx, tx.node_id, a).ok(),
        (None, None) => None,
    };
    if relayed_ref.is_none() {
        return Err(Error::Invalid(
            "answering another node's decision needs a relaying agent on this node: --relayed-by (a confirmed manager or person), or --as a person registered here".into(),
        ));
    }
    // The same relayer rule the origin applies, checked before anything is marked or sent,
    // so a refused relayer can't leave the mirror stuck in `answer_sent`.
    if let Some(r) = &relayed_ref {
        let asker: Option<String> = tx
            .query_row(
                "SELECT asker_id FROM remote_decisions WHERE decision_id = ?1",
                [&d.decision_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        check_relayer(tx, r, asker.as_deref().unwrap_or(""))?;
    }
    if let Some(answered_by) = &input.answered_by {
        check_answered_by(answered_by.trim())?;
    }
    // Free text never goes in an event: it travels as mail to the asking agent on the
    // decision's node (mail is routed only to its recipients' nodes), and the relay event
    // names that message.
    let text_message_id = match &text {
        Some(t) => {
            let (name, slug): (String, Option<String>) = tx
                .query_row(
                    "SELECT a.name, p.slug FROM remote_decisions r
                     JOIN agents a ON a.agent_id = r.asker_id
                     LEFT JOIN projects p ON p.project_id = a.project_id
                     WHERE r.decision_id = ?1",
                    [&d.decision_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    Error::Conflict("the asking agent isn't known here yet; sync first".into())
                })?;
            let from = match &relayed_ref {
                Some(r) => r.address.clone(),
                None => crate::identity::system_agent_tx(tx)?.address,
            };
            let msg = send_tx(
                tx,
                &SendMessage {
                    from,
                    to: vec![format!("{name}@{}@{origin}", slug.unwrap_or_default())],
                    subject: format!("[decision] answer note ({})", d.decision_id),
                    body: t.clone(),
                    ..Default::default()
                },
            )?;
            Some(msg.message_id)
        }
        None => None,
    };
    let relayed = relayed_ref.as_ref().map(|r| r.address.clone());
    let answered_by = input
        .answered_by
        .clone()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| d.human.clone());
    tx.emit(
        relayed.as_deref().unwrap_or(answered_by.as_str()),
        "decision.answer_relayed",
        json!({
            "decision_id": d.decision_id,
            "decision_node": origin,
            "project_id": project_id,
            "value": option.as_ref().map(|o| o.value.clone()),
            "has_text": text.is_some(),
            "text_message_id": text_message_id,
            "answered_by": answered_by,
            "relayed_by": relayed,
            // Agents sync as directory events, so the id resolves on the decision's node.
            "relayed_by_id": relayed_ref.as_ref().map(|r| r.id.clone()),
        }),
    )?;
    tx.execute(
        "UPDATE remote_decisions SET status = 'answer_sent', answer_value = ?2, answered_by = ?3,
           relayed_by = ?4, answered_at = ?5, updated_at = ?5 WHERE decision_id = ?1",
        params![
            d.decision_id,
            option.as_ref().map(|o| o.value.clone()),
            answered_by,
            relayed,
            tx.now
        ],
    )?;
    Ok(load_mirror(tx, &d.decision_id)?.map(|(d, _, _)| d))
}

/// At the decision's node: apply the first relayed answer through the normal path (mail,
/// wake, lane, spend approvals); later ones are ignored and recorded.
pub(crate) fn apply_relayed_tx(
    tx: &Tx<'_>,
    ev: &crate::federation::WireEvent,
    p: &serde_json::Value,
) -> Result<()> {
    let s = |k: &str| {
        p.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let Some(id) = s("decision_id") else {
        return Ok(());
    };
    let Ok(row) = load(tx, tx.node_id, &id) else {
        return Ok(());
    };
    // The relayer must be an agent of the node that signed the event. A missing, unknown or
    // foreign identity is refused, never treated as a trusted local answer.
    let relayed = s("relayed_by_id").and_then(|id| agent_by_id(tx, &id).ok());
    let refuse = |why: &str| -> Result<()> {
        tx.emit(
            "system",
            "decision.answer_ignored",
            json!({ "decision_id": id, "project_id": row.project_id, "relayed_from": ev.node_id, "status": why }),
        )?;
        Ok(())
    };
    let Some(relayed) = relayed else {
        return refuse("relayer_unknown");
    };
    if relayed.node_id != ev.node_id {
        return refuse("relayer_not_of_sender");
    }
    let relayed = Some(relayed);
    if row.d.status != DecisionStatus::Open {
        tx.emit(
            "system",
            "decision.answer_ignored",
            json!({ "decision_id": id, "project_id": row.project_id, "relayed_from": ev.node_id, "status": row.d.status.as_str() }),
        )?;
        return Ok(());
    }
    // The answer's text, if any, came as mail from the relaying node (never in the event).
    let text = match s("text_message_id") {
        Some(mid) => {
            let body: Option<String> = tx
                .query_row(
                    "SELECT body FROM messages WHERE message_id = ?1 AND node_id = ?2",
                    params![mid, ev.node_id],
                    |r| r.get(0),
                )
                .optional()?;
            // It only carried the text: the asker gets the answer mail itself.
            tx.execute(
                "UPDATE deliveries SET read_at = COALESCE(read_at, ?2) WHERE message_id = ?1",
                params![mid, tx.now],
            )?;
            body
        }
        None => None,
    };
    let applied = answer_tx(
        tx,
        &AnswerDecision {
            decision: id.clone(),
            value: s("value"),
            text,
            answered_by: s("answered_by"),
            relayed_by: None,
        },
        relayed,
        None,
    );
    match applied {
        Ok(_) => Ok(()),
        // A relayed answer that breaks the rules here (self-approval, bad option) is
        // recorded and dropped, not retried forever.
        Err(Error::Forbidden(_) | Error::Invalid(_) | Error::Conflict(_)) => {
            tx.emit(
                "system",
                "decision.answer_ignored",
                json!({ "decision_id": id, "project_id": row.project_id, "relayed_from": ev.node_id, "status": "refused" }),
            )?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

impl Store {
    /// Ask a person a structured question. Mails them in a new thread and opens a request of
    /// kind `decision`.
    pub fn ask_decision(&self, input: NewDecision, scope: Option<&TokenScope>) -> Result<Decision> {
        validate(&input)?;
        self.write_decision(|tx| {
            let human_name = match input
                .human
                .as_ref()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
            {
                Some(h) => h,
                None => decider_tx(tx)?,
            };
            let id = ask_tx(tx, &input, &human_name, scope)?;
            Ok(load(tx, tx.node_id, &id)?.d)
        })
    }

    /// Decisions (open ones by default), newest first, within `scope`.
    pub fn decisions(
        &self,
        filter: DecisionFilter,
        scope: Option<&TokenScope>,
    ) -> Result<Vec<Decision>> {
        self.sweep()?;
        self.read(|conn| {
            // Filters resolve within the caller's scope: hidden and missing projects or lanes
            // give the same "unknown project" / "unknown lane" error.
            let project_id = match &filter.project {
                Some(p) => Some(find_project_in(conn, self.node_id(), p, scope)?.project_id),
                None => None,
            };
            let lane_id = match &filter.lane {
                Some(l) => Some(
                    resolve_lane_in_project(conn, self.node_id(), l, project_id.as_deref(), scope)?
                        .0
                        .lane_id,
                ),
                None => None,
            };
            let status = if filter.all {
                None
            } else {
                Some(filter.status.unwrap_or(DecisionStatus::Open).as_str())
            };
            let limit = match filter.limit {
                0 => 50,
                n => n.min(500),
            };
            let mut stmt = conn.prepare(&format!(
                "{DECISION_SELECT} WHERE d.node_id = ?1 AND (?2 IS NULL OR d.status = ?2)
                   AND (?3 IS NULL OR d.project_id = ?3) AND (?4 IS NULL OR d.lane_id = ?4)
                   AND (?5 IS NULL OR h.name = ?5 OR h.address = ?5)
                 ORDER BY d.created_at DESC, d.decision_id DESC"
            ))?;
            let rows = stmt
                .query_map(
                    params![self.node_id(), status, project_id, lane_id, filter.human],
                    decision_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for r in rows {
                if let Some(s) = scope {
                    if !scope_allows_project(conn, s, &r.project_id)? {
                        continue;
                    }
                }
                out.push(r.d);
                if out.len() as u32 >= limit {
                    break;
                }
            }
            // Mirrors of other nodes' decisions (answers are relayed to their node), under the
            // same filters, merged with the local ones before the limit so the newest win.
            let mut stmt = conn.prepare(&format!(
                "{MIRROR_SELECT} WHERE (?1 IS NULL OR m.status = ?1)
                   AND (?2 IS NULL OR m.project_id = ?2) AND (?3 IS NULL OR m.lane_id = ?3)
                   AND (?4 IS NULL OR m.human = ?4 OR m.human LIKE ?4 || '@%')
                 ORDER BY m.created_at DESC"
            ))?;
            let mut rows = stmt.query(params![status, project_id, lane_id, filter.human])?;
            // Visibility before the limit: hidden mirrors must not use up the slots of
            // visible ones (they would hide permitted decisions, and their count).
            let mut mirrors = 0u32;
            while mirrors < limit {
                let Some(r) = rows.next()? else { break };
                let (d, project, _) = mirror_from_row(r)?;
                let visible = match (scope, &project) {
                    (None, _) => true,
                    (Some(s), Some(p)) => scope_allows_project(conn, s, p)?,
                    (Some(_), None) => false,
                };
                if visible {
                    out.push(d);
                    mirrors += 1;
                }
            }
            out.sort_by(|x, y| {
                (&y.created_at, &y.decision_id).cmp(&(&x.created_at, &x.decision_id))
            });
            out.truncate(limit as usize);
            Ok(out)
        })
    }

    /// Every open decision `scope` may see, oldest first, with no page limit (the daily
    /// summary must never drop the longest-waiting ones).
    pub(crate) fn open_decisions_oldest_first(
        &self,
        scope: Option<&TokenScope>,
    ) -> Result<Vec<Decision>> {
        self.sweep()?;
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{DECISION_SELECT} WHERE d.node_id = ?1 AND d.status = 'open'
                 ORDER BY d.created_at ASC, d.decision_id ASC"
            ))?;
            let rows = stmt
                .query_map([self.node_id()], decision_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for r in rows {
                if let Some(s) = scope {
                    if !scope_allows_project(conn, s, &r.project_id)? {
                        continue;
                    }
                }
                out.push(r.d);
            }
            Ok(out)
        })
    }

    /// One decision, by decision id or its request id.
    pub fn decision(&self, key: &str, scope: Option<&TokenScope>) -> Result<Decision> {
        self.sweep()?;
        self.read(|conn| match load_in(conn, self.node_id(), key, scope) {
            Ok(r) => Ok(r.d),
            // Another node's decision: same rule, a hidden mirror is a missing decision.
            Err(e) => match load_mirror(conn, key)? {
                Some((d, Some(project), _)) => match scope {
                    Some(s) if !scope_allows_project(conn, s, &project)? => Err(e),
                    _ => Ok(d),
                },
                Some((d, None, _)) if scope.is_none() => Ok(d),
                _ => Err(e),
            },
        })
    }

    /// Record a person's answer and mail it to the asking agent in the decision's thread.
    pub fn answer_decision(
        &self,
        input: AnswerDecision,
        scope: Option<&TokenScope>,
    ) -> Result<Decision> {
        self.write_decision(|tx| {
            if let Some(d) = relay_answer_tx(tx, &input, scope)? {
                return Ok(d);
            }
            let relayed = match &input.relayed_by {
                Some(r) => Some(resolve_agent(tx, tx.node_id, r)?),
                None => None,
            };
            answer_tx(tx, &input, relayed, scope)
        })
    }

    /// Withdraw an open decision (the asking agent only).
    pub fn cancel_decision(
        &self,
        agent: &str,
        key: &str,
        reason: Option<String>,
        scope: Option<&TokenScope>,
    ) -> Result<Decision> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let checked = (|| {
                let row = load_in(tx, tx.node_id, key, scope)?;
                let me = resolve_agent(tx, tx.node_id, agent)?;
                if me.id != row.asker_id {
                    return Err(Error::Forbidden(format!(
                        "only {} (who asked) can cancel this decision",
                        row.d.asker
                    )));
                }
                if row.d.status != DecisionStatus::Open {
                    return Err(Error::Conflict(format!(
                        "decision {} is already {}",
                        row.d.decision_id,
                        row.d.status.as_str()
                    )));
                }
                Ok((row, me))
            })();
            let (row, me) = match checked {
                Ok(actors) => actors,
                // Refusing cancellation must commit the sweep, while cancellation
                // mutations below still roll back together if any of them fail.
                Err(error) => return Ok(Err(error)),
            };
            cancel_row_tx(tx, &me, &row, reason.as_deref())?;
            Ok(Ok(load(tx, tx.node_id, &row.d.decision_id)?.d))
        })?
    }
}

/// A short, phone-readable rendering of a decision.
pub fn decision_markdown(d: &Decision) -> String {
    let mut out = format!(
        "**Decision for {}** ({}) from {} · {}{}\n\n{}\n\n",
        d.human,
        d.status.as_str(),
        d.asker,
        d.project,
        d.lane
            .as_ref()
            .map(|l| format!(" · lane {l}"))
            .unwrap_or_default(),
        d.question.trim()
    );
    for (i, o) in d.options.iter().enumerate() {
        out.push_str(&format!("{}. {} (`{}`)", i + 1, o.label, o.value));
        if let Some(desc) = &o.description {
            out.push_str(&format!(" — {desc}"));
        }
        out.push('\n');
    }
    if d.allow_free_text {
        out.push_str("Free text is OK too.\n");
    }
    for l in &d.context_links {
        out.push_str(&format!("- {l}\n"));
    }
    if let Some(at) = &d.deadline_at {
        out.push_str(&format!(
            "\nDeadline {at}, then {}.\n",
            timeout_text(d.on_timeout, d.default_value.as_deref())
        ));
    }
    match &d.answer {
        Some(a) => {
            out.push_str(&format!(
                "\n**Answer:** {}{} — {}{}\n",
                a.label
                    .as_deref()
                    .or(a.value.as_deref())
                    .unwrap_or("(text)"),
                a.text
                    .as_ref()
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default(),
                a.answered_by,
                a.relayed_by
                    .as_ref()
                    .map(|r| format!(" via {r}"))
                    .unwrap_or_default()
            ));
        }
        None if d.status == DecisionStatus::Open => {
            out.push_str(&format!(
                "\nReply with a number or value. CLI: `another decision answer {} <value>`\n",
                d.decision_id
            ));
        }
        None => {}
    }
    out
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::Clock;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    #[test]
    fn cancellation_cannot_discard_a_default_due_at_its_transaction_time() {
        let deadline = 1_700_000_060_000;
        let clock = Arc::new(AtomicI64::new(deadline - 60_000));
        let s = Store::open_in_memory(Clock::Ticking(clock.clone())).unwrap();
        s.ensure_project("t", "/work/shop").unwrap();
        s.register_agent_trusted(RegisterAgent {
            name: "coder".into(),
            program: Program::Codex,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
        let d = s
            .ask_decision(
                NewDecision {
                    agent: "coder@shop".into(),
                    question: "Proceed?".into(),
                    options: ["go", "wait"]
                        .into_iter()
                        .map(|value| DecisionOption {
                            value: value.into(),
                            label: value.into(),
                            description: None,
                        })
                        .collect(),
                    deadline_at: Some(deadline),
                    on_timeout: DecisionTimeout::ChooseDefault,
                    default_value: Some("wait".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        // A separate sweep would run just before the deadline, and its following
        // cancellation transaction at the deadline. The outcome must use its own time.
        clock.store(deadline - 1, Ordering::SeqCst);
        match s.cancel_decision("coder@shop", &d.decision_id, None, None) {
            Ok(cancelled) => {
                let at = chrono::DateTime::parse_from_rfc3339(&cancelled.updated_at)
                    .unwrap()
                    .timestamp_millis();
                assert!(
                    at < deadline,
                    "cancellation at {at} discarded a due default"
                );
            }
            Err(e) => {
                assert_eq!(e.code(), "conflict", "{e}");
                assert!(s.events(100).unwrap().iter().any(|event| {
                    event.kind == "decision.expired"
                        && event.payload["decision_id"] == d.decision_id
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, Program};

    /// A newer mirror in a hidden project must not take the only slot of a visible one.
    #[test]
    fn hidden_mirrors_do_not_use_up_the_limit() {
        let s = Store::open_in_memory(Clock::System).unwrap();
        let shop = s.ensure_project("t", "/work/shop").unwrap().project_id;
        let vault = s.ensure_project("t", "/work/vault").unwrap().project_id;
        s.register_agent_trusted(RegisterAgent {
            name: "manager".into(),
            program: Program::GrokBot,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
        s.write(|tx| {
            for (id, project, at) in [
                ("MIRROR-SHOP", &shop, 1_000),
                ("MIRROR-VAULT", &vault, 2_000),
            ] {
                tx.execute(
                    "INSERT INTO remote_decisions (decision_id, node_id, project_id, human,
                       question, options, status, created_at, updated_at)
                     VALUES (?1, 'OTHER-NODE', ?2, 'owner', 'q', '[]', 'open', ?3, ?3)",
                    params![id, project, at],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let t = s
            .create_token("manager", &["/work/shop".into()], None, None)
            .unwrap();
        let scope = s.verify_token(&t.secret).unwrap();
        let got = s
            .decisions(
                DecisionFilter {
                    limit: 1,
                    ..Default::default()
                },
                Some(&scope),
            )
            .unwrap();
        assert_eq!(
            got.iter()
                .map(|d| d.decision_id.as_str())
                .collect::<Vec<_>>(),
            ["MIRROR-SHOP"]
        );
    }

    /// A signed relay names a relayer the sender doesn't own (this node's manager), or none:
    /// both are refused at ingestion, and the decision stays open.
    #[test]
    fn relayed_answers_must_name_a_relayer_of_the_signing_node() {
        let s = Store::open_in_memory(Clock::System).unwrap();
        s.ensure_project("t", "/work/shop").unwrap();
        for (name, program) in [("claude", Program::Claude), ("manager", Program::GrokBot)] {
            s.register_agent_trusted(RegisterAgent {
                name: name.into(),
                program,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
        }
        let d = s
            .ask_decision(
                NewDecision {
                    agent: "claude@shop".into(),
                    question: "Ship?".into(),
                    options: vec![
                        DecisionOption {
                            value: "yes".into(),
                            label: "Yes".into(),
                            description: None,
                        },
                        DecisionOption {
                            value: "no".into(),
                            label: "No".into(),
                            description: None,
                        },
                    ],
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let manager = s.agent("manager@shop").unwrap().agent_id;
        let relay = |relayer: Option<&str>| {
            let payload = serde_json::json!({
                "decision_id": d.decision_id,
                "decision_node": s.node_id(),
                "value": "yes",
                "relayed_by_id": relayer,
            });
            let ev = crate::federation::WireEvent {
                seq: 1,
                event_id: crate::store::new_id(),
                node_id: "SPOKE-NODE".into(),
                ts: s.now(),
                actor: "manager".into(),
                kind: "decision.answer_relayed".into(),
                payload: payload.to_string(),
                prev_id: None,
                sig: String::new(),
            };
            s.write(|tx| apply_relayed_tx(tx, &ev, &payload)).unwrap();
        };
        relay(Some(manager.as_str()));
        relay(None);
        relay(Some("NO-SUCH-AGENT"));
        assert_eq!(
            s.decision(&d.decision_id, None).unwrap().status,
            DecisionStatus::Open
        );
        let why: Vec<String> = s
            .events(100)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "decision.answer_ignored")
            .map(|e| e.payload["status"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            why,
            [
                "relayer_not_of_sender",
                "relayer_unknown",
                "relayer_unknown"
            ]
        );
    }

    fn review_store() -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>) {
        let (clock, handle) = crate::Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        store.ensure_project("test", "/work/shop").unwrap();
        store
            .register_agent_trusted(RegisterAgent {
                name: "worker".into(),
                program: Program::Codex,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
        (store, handle)
    }

    fn review_question(store: &Store, deadline: bool) -> NewDecision {
        NewDecision {
            agent: "worker@shop".into(),
            question: "Choose an option".into(),
            options: vec![
                DecisionOption {
                    value: "wait".into(),
                    label: "Wait".into(),
                    description: None,
                },
                DecisionOption {
                    value: "ship".into(),
                    label: "Ship".into(),
                    description: None,
                },
            ],
            deadline_at: deadline.then(|| store.now() + 60_000),
            on_timeout: if deadline {
                DecisionTimeout::ChooseDefault
            } else {
                DecisionTimeout::KeepOpen
            },
            default_value: deadline.then(|| "wait".into()),
            ..Default::default()
        }
    }

    #[test]
    fn refused_decision_mutations_keep_due_defaults_and_roll_back_their_own_changes() {
        for refusal in ["late_answer", "invalid_answer", "unknown_lane", "self_ask"] {
            let (store, clock) = review_store();
            let due = store
                .ask_decision(review_question(&store, true), None)
                .unwrap();
            let active = store
                .ask_decision(review_question(&store, false), None)
                .unwrap();
            clock.fetch_add(60_001, std::sync::atomic::Ordering::SeqCst);
            let error = if refusal.ends_with("answer") {
                store
                    .answer_decision(
                        AnswerDecision {
                            decision: if refusal == "late_answer" {
                                due.decision_id.clone()
                            } else {
                                active.decision_id.clone()
                            },
                            value: Some(
                                if refusal == "late_answer" {
                                    "wait"
                                } else {
                                    "missing-option"
                                }
                                .into(),
                            ),
                            text: None,
                            answered_by: None,
                            relayed_by: None,
                        },
                        None,
                    )
                    .unwrap_err()
            } else {
                let mut question = review_question(&store, false);
                if refusal == "unknown_lane" {
                    question.human = Some("new-person".into());
                    question.lane = Some("missing-lane".into());
                } else {
                    question.agent = "owner".into();
                    question.project = Some("shop".into());
                }
                store.ask_decision(question, None).unwrap_err()
            };
            assert!(
                matches!(
                    error,
                    Error::Invalid(_) | Error::Conflict(_) | Error::NotFound(_)
                ),
                "{refusal}: {error}"
            );
            // Query directly: decision(), inbox() and request reads must not repair the result.
            store.read(|conn| {
                let status: String = conn.query_row("SELECT status FROM decisions WHERE decision_id = ?1", [&due.decision_id], |r| r.get(0))?;
                assert_eq!(status, "expired", "{refusal}");
                let request: String = conn.query_row("SELECT status FROM requests WHERE request_id = ?1", [&due.request_id], |r| r.get(0))?;
                assert_eq!(request, "done", "{refusal}");
                let automatic: i64 = conn.query_row("SELECT count(*) FROM messages WHERE thread_id = ?1 AND subject LIKE '[decision] default applied:%'", [&due.thread_id], |r| r.get(0))?;
                assert_eq!(automatic, 1, "{refusal}");
                let humans: i64 = conn.query_row("SELECT count(*) FROM agents WHERE name = 'new-person'", [], |r| r.get(0))?;
                assert_eq!(humans, 0, "failed ask must roll back its new human");
                Ok(())
            }).unwrap();
            let previous: String = store
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT event_id FROM events WHERE node_id = ?1 ORDER BY seq DESC LIMIT 1",
                        [store.node_id()],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            store.set_node_name("after-refusal").unwrap();
            let next_previous: String = store
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT prev_id FROM events WHERE node_id = ?1 ORDER BY seq DESC LIMIT 1",
                        [store.node_id()],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(
                next_previous, previous,
                "rolled-back events cannot remain in the chain"
            );
        }
    }

    #[test]
    fn answer_labels_reject_prose_before_it_reaches_events() {
        let (store, _) = review_store();
        let decision = store
            .ask_decision(review_question(&store, false), None)
            .unwrap();
        let error = store
            .answer_decision(
                AnswerDecision {
                    decision: decision.decision_id.clone(),
                    value: Some("wait".into()),
                    text: None,
                    answered_by: Some("a sentence with private details".into()),
                    relayed_by: None,
                },
                None,
            )
            .unwrap_err();
        assert_eq!(error.code(), "invalid");
        store
            .read(|conn| {
                let count: i64 = conn.query_row(
                    "SELECT count(*) FROM events WHERE kind = 'decision.answered'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
        for name in ["alex", "alex@shop", "alex@shop@node", "alex@@node"] {
            let decision = store
                .ask_decision(review_question(&store, false), None)
                .unwrap();
            store
                .answer_decision(
                    AnswerDecision {
                        decision: decision.decision_id,
                        value: Some("wait".into()),
                        text: None,
                        answered_by: Some(name.into()),
                        relayed_by: None,
                    },
                    None,
                )
                .unwrap();
        }
    }

    #[test]
    fn a_database_failure_rolls_back_the_sweep_and_attempted_question() {
        let (store, clock) = review_store();
        let due = store
            .ask_decision(review_question(&store, true), None)
            .unwrap();
        clock.fetch_add(60_001, std::sync::atomic::Ordering::SeqCst);
        let before: i64 = store
            .read(|conn| Ok(conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?))
            .unwrap();
        store.read(|conn| {
            conn.execute_batch("CREATE TEMP TRIGGER refuse_decision BEFORE INSERT ON decisions BEGIN SELECT RAISE(ABORT, 'injected write failure'); END")?;
            Ok(())
        }).unwrap();
        let mut question = review_question(&store, false);
        question.human = Some("new-person".into());
        assert!(matches!(
            store.ask_decision(question, None),
            Err(Error::Db(_))
        ));
        store
            .read(|conn| {
                let status: String = conn.query_row(
                    "SELECT status FROM decisions WHERE decision_id = ?1",
                    [&due.decision_id],
                    |r| r.get(0),
                )?;
                assert_eq!(
                    status, "open",
                    "database failures roll back the whole transaction"
                );
                let events: i64 =
                    conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
                assert_eq!(events, before);
                let people: i64 = conn.query_row(
                    "SELECT count(*) FROM agents WHERE name = 'new-person'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(people, 0);
                conn.execute_batch("DROP TRIGGER refuse_decision")?;
                Ok(())
            })
            .unwrap();
        store
            .ask_decision(review_question(&store, false), None)
            .unwrap();
        let status: String = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT status FROM decisions WHERE decision_id = ?1",
                    [&due.decision_id],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            status, "expired",
            "a successful retry can commit the timeout"
        );
    }

    fn foreign_role_relay(history: &[Program], projected: Program, refresh: Option<Program>) {
        let (hub, _) = review_store();
        hub.set_node_name("hub").unwrap();
        let decision = hub
            .ask_decision(review_question(&hub, false), None)
            .unwrap();
        let human = hub.agent("owner").unwrap();
        for &program in history {
            hub.register_agent_trusted(RegisterAgent {
                name: "owner".into(),
                program,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
        }
        let peer = Store::open_in_memory(crate::Clock::manual(hub.now()).0).unwrap();
        peer.set_node_name("peer").unwrap();
        // Use one clock for invite validity on both stores.
        let code = hub
            .create_invite("http://127.0.0.1:1", &crate::Perm::ALL, 60)
            .unwrap();
        let (invite, request) = peer.prepare_join(&code).unwrap();
        let response = hub.accept_pairing(&request).unwrap();
        peer.complete_join(&invite, &response).unwrap();
        let event_id = peer
            .write(|tx| {
                tx.emit(
                    "peer",
                    "agent.registered",
                    json!({
                        "agent_id": human.agent_id,
                        "name": "owner",
                        "program": projected.as_str(),
                        "role_confirmed": true
                    }),
                )
            })
            .unwrap();
        let (events, _) = peer.outbound_events(0, 100).unwrap();
        let report = hub.ingest(peer.node_id(), &events, &[]).unwrap();
        assert!(report.accepted > 0);
        assert!(hub
            .events(100)
            .unwrap()
            .iter()
            .any(|e| e.event_id == event_id));
        if let Some(program) = refresh {
            let refreshed = hub
                .register_agent(RegisterAgent {
                    name: "owner".into(),
                    program,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
            assert_eq!(refreshed.program, projected.as_str());
            let latest = hub
                .events(100)
                .unwrap()
                .into_iter()
                .rev()
                .find(|e| {
                    e.node_id == hub.node_id()
                        && e.kind == "agent.registered"
                        && e.payload["agent_id"] == human.agent_id
                })
                .unwrap();
            assert_eq!(latest.payload["program"], projected.as_str());
            assert_eq!(latest.payload["role_confirmed"], false);
        }
        let asker = hub.agent("worker@shop").unwrap();
        let denied = hub
            .read(|conn| {
                let relayer = resolve_agent(conn, hub.node_id(), "owner")?;
                check_relayer(conn, &relayer, &asker.agent_id)
            })
            .unwrap_err();
        assert_eq!(denied.code(), "forbidden");
        let error = hub
            .answer_decision(
                AnswerDecision {
                    decision: decision.decision_id.clone(),
                    value: Some("wait".into()),
                    text: None,
                    answered_by: None,
                    relayed_by: Some("owner".into()),
                },
                None,
            )
            .unwrap_err();
        assert_eq!(error.code(), "forbidden");
        hub.register_agent_trusted(RegisterAgent {
            name: "owner".into(),
            program: projected,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
        hub.answer_decision(
            AnswerDecision {
                decision: decision.decision_id,
                value: Some("wait".into()),
                text: None,
                answered_by: None,
                relayed_by: Some("owner".into()),
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn a_signed_foreign_registration_cannot_confirm_a_local_human_role() {
        foreign_role_relay(&[], Program::Human, None);
    }

    #[test]
    fn a_peer_cannot_restore_a_locally_removed_privileged_role() {
        foreign_role_relay(&[Program::Human, Program::Codex], Program::Human, None);
    }

    #[test]
    fn an_untrusted_refresh_cannot_reconfirm_a_human_role_restored_by_a_peer() {
        for refresh in [Program::Codex, Program::Human] {
            foreign_role_relay(
                &[Program::Human, Program::Codex],
                Program::Human,
                Some(refresh),
            );
        }
    }

    #[test]
    fn an_untrusted_refresh_cannot_reconfirm_a_previous_privileged_role() {
        for refresh in [Program::Codex, Program::GrokBot] {
            foreign_role_relay(
                &[Program::GrokBot, Program::Human],
                Program::GrokBot,
                Some(refresh),
            );
        }
    }

    #[test]
    fn an_untrusted_same_program_refresh_keeps_genuine_role_confirmation() {
        for program in [Program::Human, Program::GrokBot] {
            let (hub, _) = review_store();
            let decision = hub
                .ask_decision(review_question(&hub, false), None)
                .unwrap();
            hub.register_agent_trusted(RegisterAgent {
                name: "owner".into(),
                program,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
            hub.register_agent(RegisterAgent {
                name: "owner".into(),
                program,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
            hub.answer_decision(
                AnswerDecision {
                    decision: decision.decision_id,
                    value: Some("wait".into()),
                    text: None,
                    answered_by: None,
                    relayed_by: Some("owner".into()),
                },
                None,
            )
            .unwrap();
        }
    }

    #[test]
    fn deadlines() {
        let now = 1_000_000_000;
        assert_eq!(parse_deadline("2h", now).unwrap(), now + 7_200_000);
        assert_eq!(parse_deadline("30m", now).unwrap(), now + 1_800_000);
        assert!(parse_deadline("1970-01-01T00:00:01Z", now).is_err(), "past");
        assert_eq!(
            parse_deadline("2100-01-01T00:00:00Z", now).unwrap(),
            4_102_444_800_000
        );
        assert!(parse_deadline("soon", now).is_err());
    }
}
