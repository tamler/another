//! Another core: local-first messaging and coordination for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod activity;
pub mod crypto;
mod dashboard;
mod error;
mod federation;
mod holds;
mod identity;
mod mail;
mod oauth;
mod repo;
mod requests;
mod resources;
mod stats;
mod store;
mod time;
mod tokens;
mod wake;

pub use activity::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, ActivitySummary, AgentPresence,
    DigestMessage, DigestRequest, RequestCounts, Since, WhatsNew,
};
pub use dashboard::{
    DashAccess, DashActivityItem, DashActivityPage, DashActivityQuery, DashAgent, DashClash,
    DashCounts, DashHold, DashMessage, DashProject, DashRequest, DashThread, DashboardSnapshot,
    DashboardView,
};
pub use error::{Error, ErrorBody, Result};
pub use federation::{
    reachable, IngestReport, Invite, NodeEntry, PairRequest, PairResponse, Peer, Perm,
    PullResponse, PushRequest, Rejected, WireEvent,
};
pub use holds::{
    is_repo_wide, patterns_overlap, AcquireHolds, EditGuard, HeartbeatReport, Hold, HoldConflict,
    HoldFilter, HoldReport, SessionInfo, SessionPhase, SweepReport, DEFAULT_HOLD_TTL_SECONDS,
    MAX_HOLD_TTL_SECONDS, MIN_HOLD_TTL_SECONDS,
};
pub use identity::{Agent, Program, Project, RegisterAgent};
pub use mail::{
    AckOutcome, Importance, Inbox, InboxItem, InboxQuery, Message, NewRequest, Reply, SendMessage,
    Thread, ThreadSummary, UnreadAfter, UnreadItem,
};
pub use oauth::{
    canonical_resource, error_redirect, redirect_host, redirect_is_loopback, redirect_matches,
    validate_cimd, validate_redirect_uri, Approval, AuthorizationStatus, AuthorizeInput,
    ClientProfile, ClientRegistration, CodeExchange, IssuedTokens, OAuthFail, OauthGrant,
    PendingAuthorization, PendingView, RefreshExchange, RegisteredClient, ACCESS_TTL_SECS,
};
pub use repo::{link_for as repo_link_for, normalize_remote, RepoLink};
pub use requests::{Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate};
pub use resources::{
    derive_port, Lease, LeaseMode, LeaseOutcome, RegisterResource, Resource, ResourceFilter,
    ResourceKind, ResourceRef, DEFAULT_LEASE_TTL_SECONDS, MAX_LEASE_TTL_SECONDS,
    MIN_LEASE_TTL_SECONDS,
};
pub use stats::{BlockedEdit, TrialStats, WakeRecord};
pub use store::{default_home, Clock, Event, NodeInfo, Store, DB_FILE};
pub use time::{fmt_ts, parse_ts};
pub use tokens::{AuditEntry, AuditRecord, NewToken, Token, TokenScope};
pub use wake::{WakeTarget, WakeVia};
