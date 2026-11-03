//! MCP tools for human decisions (docs/design/03-decisions.md). Remote callers only reach
//! decisions in their token's projects; a relayed answer records the token's agent.

use another_core::{
    parse_deadline, AnswerDecision, Decision, DecisionFilter, DecisionOption, DecisionStatus,
    DecisionTimeout, NewDecision,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::{AnotherMcp, ToolResult};
use crate::remote;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionAskArgs {
    /// The asking agent.
    pub agent: String,
    /// The question, in a sentence or two.
    pub question: String,
    /// 2 to 6 options: `value` is a short token (`ship`), `label` what the person sees.
    pub options: Vec<DecisionOption>,
    /// Accept a free-text answer as well as the options.
    #[serde(default)]
    pub allow_free_text: bool,
    /// URLs or ids that help decide (PR, CI run, lane).
    #[serde(default)]
    pub context_links: Vec<String>,
    /// Duration like `2h` or an RFC 3339 time.
    pub deadline: Option<String>,
    /// At the deadline: choose_default (needs `default`), cancel, or keep_open (default).
    pub on_timeout: Option<DecisionTimeout>,
    /// The option value applied by choose_default.
    pub default: Option<String>,
    /// Who to ask (default: the node's decider, `jacob` unless configured).
    pub human: Option<String>,
    /// Project path or slug (default: the lane's, else yours).
    pub project: Option<String>,
    /// Lane id or name: it shows needs_decision until this is answered.
    pub lane: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionListArgs {
    /// open (default), answered, expired, cancelled.
    pub status: Option<DecisionStatus>,
    /// Every status.
    #[serde(default)]
    pub all: bool,
    pub project: Option<String>,
    pub lane: Option<String>,
    pub human: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DecisionsOut {
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionShowArgs {
    /// Decision id (or its request id).
    pub decision: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DecisionShown {
    pub decision: Decision,
    /// Short, phone-readable markdown.
    pub markdown: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionAnswerArgs {
    /// Decision id (or its request id).
    pub decision: String,
    /// The chosen option value (or its 1-based number).
    pub value: Option<String>,
    /// Free text (only if the decision allows it).
    pub text: Option<String>,
    /// The person who decided (default: the person asked).
    pub answered_by: Option<String>,
    /// The agent relaying the answer; remote callers: always the token's agent.
    pub relayed_by: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionCancelArgs {
    /// The asking agent.
    pub agent: String,
    pub decision: String,
    pub reason: Option<String>,
}

#[tool_router(router = decision_router, vis = "pub(crate)")]
impl AnotherMcp {
    #[tool(
        description = "Ask a person a structured question (2-6 options, optional free text, \
        context links, deadline and what happens if nobody answers). The answer arrives in your \
        inbox as a reply in the same thread. With a lane, the lane shows needs_decision until \
        it is answered."
    )]
    async fn decision_ask(
        &self,
        Parameters(a): Parameters<DecisionAskArgs>,
    ) -> ToolResult<Decision> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let deadline_at = a
                .deadline
                .as_deref()
                .map(|d| parse_deadline(d, s.now()))
                .transpose()?;
            s.ask_decision(
                NewDecision {
                    agent: a.agent,
                    question: a.question,
                    options: a.options,
                    allow_free_text: a.allow_free_text,
                    context_links: a.context_links,
                    deadline_at,
                    on_timeout: a.on_timeout.unwrap_or_default(),
                    default_value: a.default,
                    human: a.human,
                    project: a.project,
                    lane: a.lane,
                },
                scope.as_ref(),
            )
        })
        .await
    }

    #[tool(description = "List decisions (open ones by default), newest first.")]
    async fn decision_list(
        &self,
        Parameters(a): Parameters<DecisionListArgs>,
    ) -> ToolResult<DecisionsOut> {
        let scope = remote::current_scope();
        self.run(move |s| {
            Ok(DecisionsOut {
                decisions: s.decisions(
                    DecisionFilter {
                        status: a.status,
                        all: a.all,
                        project: a.project,
                        lane: a.lane,
                        human: a.human,
                        limit: a.limit.unwrap_or(50),
                    },
                    scope.as_ref(),
                )?,
            })
        })
        .await
    }

    #[tool(description = "Show one decision, with a short phone-readable markdown rendering.")]
    async fn decision_show(
        &self,
        Parameters(a): Parameters<DecisionShowArgs>,
    ) -> ToolResult<DecisionShown> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let decision = s.decision(&a.decision, scope.as_ref())?;
            Ok(DecisionShown {
                markdown: another_core::decision_markdown(&decision),
                decision,
            })
        })
        .await
    }

    #[tool(
        description = "Answer a decision on a person's behalf (the manager relays what they \
        chose). Records who answered and who relayed it, mails the answer to the asking agent \
        and wakes it if it has a wake target."
    )]
    async fn decision_answer(
        &self,
        Parameters(a): Parameters<DecisionAnswerArgs>,
    ) -> ToolResult<Decision> {
        let scope = remote::current_scope();
        self.run(move |s| {
            let d = s.answer_decision(
                AnswerDecision {
                    decision: a.decision,
                    value: a.value,
                    text: a.text,
                    answered_by: a.answered_by,
                    relayed_by: Some(a.relayed_by),
                },
                scope.as_ref(),
            )?;
            crate::wake::wake_if_targeted(s, &d.asker);
            Ok(d)
        })
        .await
    }

    #[tool(description = "Withdraw a decision you asked (it no longer needs an answer).")]
    async fn decision_cancel(
        &self,
        Parameters(a): Parameters<DecisionCancelArgs>,
    ) -> ToolResult<Decision> {
        let scope = remote::current_scope();
        self.run(move |s| s.cancel_decision(&a.agent, &a.decision, a.reason, scope.as_ref()))
            .await
    }
}
