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

impl Store {
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
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionStatus::Open => "open",
            DecisionStatus::Answered => "answered",
            DecisionStatus::Expired => "expired",
            DecisionStatus::Cancelled => "cancelled",
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

fn validate(input: &NewDecision) -> Result<()> {
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
    let existing: Option<String> = tx
        .query_row(
            "SELECT agent_id FROM agents WHERE node_id = ?1 AND name = ?2 AND project_id IS NULL",
            params![tx.node_id, name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
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
    if lane.status == LaneStatus::NeedsDecision {
        set_status(tx, actor, &lane, &project_id, LaneStatus::Working)?;
    }
    Ok(())
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
                    subject: format!("[decision] overdue: {}", short(&d.question, 80)),
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
                    format!("[decision] default applied: {label}"),
                    format!(
                        "No answer by the deadline; the default applies.\n\nValue: {v}\nLabel: {label}\n\n(automatic; decision {})",
                        d.decision_id
                    ),
                )
            }
            None => (
                format!("[decision] cancelled: {}", short(&d.question, 80)),
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
        release_lane(tx, &asker, row.lane_id.as_deref())?;
    }
    Ok(due.len())
}

impl Store {
    /// Ask a person a structured question. Mails them in a new thread and opens a request of
    /// kind `decision`.
    pub fn ask_decision(&self, input: NewDecision, scope: Option<&TokenScope>) -> Result<Decision> {
        validate(&input)?;
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let asker = resolve_agent(tx, tx.node_id, &input.agent)?;
            let human_name = match input
                .human
                .as_ref()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
            {
                Some(h) => h,
                None => decider_tx(tx)?,
            };
            if human_name == crate::identity::SYSTEM_AGENT {
                return Err(Error::Invalid(
                    "decisions are for people, not the system agent".into(),
                ));
            }
            let human = ensure_human(tx, &human_name)?;
            if human.id == asker.id {
                return Err(Error::Invalid(
                    "an agent cannot ask itself for a decision".into(),
                ));
            }
            // The project comes first (explicit, else the asker's) and is scope-checked before
            // any lane lookup; the lane is then resolved inside that project only, so a lane
            // elsewhere, out of scope or missing is the same "unknown lane".
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
                    subject: format!("[decision] {}", short(&input.question, 120)),
                    body: render_question(&input, &decision_id, &human.address),
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
                    "human": human.address,
                    "question": input.question,
                    "options": input.options,
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
            Ok(load(tx, tx.node_id, &decision_id)?.d)
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
            Ok(out)
        })
    }

    /// One decision, by decision id or its request id.
    pub fn decision(&self, key: &str, scope: Option<&TokenScope>) -> Result<Decision> {
        self.sweep()?;
        self.read(|conn| {
            let r = load(conn, self.node_id(), key)?;
            check_scope(conn, scope, &r.project_id)?;
            Ok(r.d)
        })
    }

    /// Record a person's answer and mail it to the asking agent in the decision's thread.
    pub fn answer_decision(
        &self,
        input: AnswerDecision,
        scope: Option<&TokenScope>,
    ) -> Result<Decision> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let row = load(tx, tx.node_id, &input.decision)?;
            check_scope(tx, scope, &row.project_id)?;
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
                        .find(|o| o.value == *v || o.value.eq_ignore_ascii_case(v))
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
            check_text("answered_by", &answered_by, 64)?;
            let relayed = match &input.relayed_by {
                Some(r) => Some(resolve_agent(tx, tx.node_id, r)?),
                None => None,
            };
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
                Some(o) => format!("[decision] {}: {}", o.label, short(&d.question, 60)),
                None => format!("[decision] answered: {}", short(&d.question, 80)),
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
            release_lane(
                tx,
                relayed.as_ref().unwrap_or(&human),
                row.lane_id.as_deref(),
            )?;
            Ok(load(tx, tx.node_id, &d.decision_id)?.d)
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
            let row = load(tx, tx.node_id, key)?;
            check_scope(tx, scope, &row.project_id)?;
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
            tx.emit(
                &me.address,
                "decision.cancelled",
                json!({ "decision_id": row.d.decision_id, "project_id": row.project_id, "asker_id": row.asker_id, "lane_id": row.lane_id, "reason": reason }),
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
                    subject: format!("[decision] withdrawn: {}", short(&row.d.question, 80)),
                    body: format!(
                        "No answer needed any more.{}\n(decision {})",
                        reason.as_deref().map(|r| format!(" Reason: {r}")).unwrap_or_default(),
                        row.d.decision_id
                    ),
                    reply_to: Some(row.message_id.clone()),
                    ..Default::default()
                },
            )?;
            close_request(tx, &row.d.request_id, "cancelled", None, reason.as_deref().unwrap_or("withdrawn"))?;
            release_lane(tx, &me, row.lane_id.as_deref())?;
            Ok(load(tx, tx.node_id, &row.d.decision_id)?.d)
        })
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
mod tests {
    use super::*;

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
