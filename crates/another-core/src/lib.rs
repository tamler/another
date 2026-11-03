//! Another core: a local-first, vendor-neutral inbox for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod activity;
pub mod crypto;
mod decisions;
mod error;
mod federation;
mod holds;
mod identity;
mod lanes;
mod mail;
mod requests;
mod resources;
mod settings;
mod spend;
mod store;
mod summary;
mod time;
mod tokens;
mod visibility;
mod wake;
mod webhooks;

pub use activity::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, ActivitySummary, AgentPresence,
    RequestCounts, Since,
};
pub use decisions::{
    decision_markdown, parse_deadline, AnswerDecision, Decision, DecisionAnswer, DecisionFilter,
    DecisionOption, DecisionStatus, DecisionTimeout, NewDecision, DEFAULT_DECIDER, MAX_OPTION_NOTE,
};
pub use error::{Error, ErrorBody, Result};
pub use federation::{
    reachable, IngestReport, Invite, NodeEntry, PairRequest, PairResponse, Peer, Perm,
    PullResponse, PushRequest, Rejected, WireEvent,
};
pub use holds::{
    patterns_overlap, AcquireHolds, HeartbeatReport, Hold, HoldConflict, HoldFilter, HoldReport,
    LaneHolds, SessionInfo, SessionPhase, SweepReport, DEFAULT_HOLD_TTL_SECONDS,
    MAX_HOLD_TTL_SECONDS, MIN_HOLD_TTL_SECONDS,
};
pub use identity::{Agent, Program, Project, RegisterAgent, SYSTEM_AGENT};
pub use lanes::{
    lane_markdown, Lane, LaneDetail, LaneFilter, LaneLink, LaneLinkKind, LaneReport,
    LaneReportInput, LaneStatus, LaneUpdate, NewLane, ReportKind,
};
pub use mail::{
    AckOutcome, Importance, Inbox, InboxItem, InboxQuery, Message, NewRequest, Reply, SendMessage,
    Thread, ThreadSummary,
};
pub use requests::{Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate};
pub use resources::{
    derive_port, Lease, LeaseMode, LeaseOutcome, RegisterResource, Resource, ResourceFilter,
    ResourceKind, ResourceRef, DEFAULT_LEASE_TTL_SECONDS, LEASE_EXPIRING_WINDOW_MS,
    MAX_LEASE_TTL_SECONDS, MIN_LEASE_TTL_SECONDS,
};
pub use settings::{Setting, SETTINGS};
pub use spend::{
    period_bounds, spend_markdown, AccountSpend, BorrowSpend, BudgetStatus, NewSpendAccount,
    PeriodSel, RecordSpend, SpendAccount, SpendAccountUpdate, SpendEntry, SpendGroup, SpendKind,
    SpendLease, SpendLeaseOutcome, SpendLeaseStatus, SpendPeriod, SpendQuery, SpendReport,
    DEFAULT_SPEND_LEASE_TTL_SECONDS,
};
pub use store::{default_home, Clock, Event, NodeInfo, Store, DB_FILE};
pub use summary::{
    summary_markdown, ClosedLane, DailySummary, DecisionSummary, HoldSummary, LaneSummary,
    OpenDecision, RequestSummary, SpendLine,
};
pub use time::fmt_ts;
pub use tokens::{AuditEntry, AuditRecord, NewToken, Token, TokenScope};
pub use wake::{WakeTarget, WakeVia};
pub use webhooks::{
    signature as webhook_signature, validate_webhook_url, DeliveryStatus, NewWebhook,
    OutboundDelivery, Webhook, WebhookCreated, WebhookDelivery, WebhookFilter,
    MAX_DELIVERY_ATTEMPTS,
};
