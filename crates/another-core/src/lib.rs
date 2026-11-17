//! Another core: local-first messaging and coordination for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod activity;
mod board;
pub mod crypto;
mod dashboard;
mod error;
mod federation;
mod holds;
mod identity;
mod mail;
mod oauth;
mod queue;
mod repo;
mod requests;
mod resources;
mod stats;
mod store;
mod time;
mod tokens;
mod transport;
mod wake;

pub use activity::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, ActivitySummary, AgentPresence,
    DigestMessage, DigestRequest, RequestCounts, Since, WhatsNew,
};
pub use board::{
    Board, BoardAgent, BoardBlocked, BoardEdit, BoardHold, BoardProject, BoardQuery, BoardRequest,
    BOARD_EVENT_SCAN_CAP, DEFAULT_BOARD_LIMIT, DEFAULT_BOARD_WINDOW_MS, EDIT_HOOK_HOLD_REASON,
    MAX_BOARD_LIMIT,
};
pub use dashboard::{
    DashAccess, DashActivityItem, DashActivityPage, DashActivityQuery, DashAgent, DashBlocked,
    DashClash, DashCounts, DashEdit, DashHold, DashMessage, DashProject, DashRequest, DashThread,
    DashboardSnapshot, DashboardView,
};
pub use error::{Error, ErrorBody, Result};
pub use federation::{
    reachable, IngestReport, Invite, NodeEntry, PairRequest, PairResponse, Peer, Perm, PullAnchors,
    PullResponse, PushRequest, Rejected, WireEvent,
};
pub use holds::{
    is_repo_wide, patterns_overlap, AcquireHolds, EditGuard, HeartbeatReport, Hold, HoldConflict,
    HoldFilter, HoldReport, SessionInfo, SessionPhase, SweepReport, DEFAULT_HOLD_TTL_SECONDS,
    MAX_HOLD_TTL_SECONDS, MIN_HOLD_TTL_SECONDS,
};
pub use identity::{Agent, Program, Project, RegisterAgent};
pub use mail::{
    is_unsafe_text_char, reply_subject, single_line, AckOutcome, Importance, Inbox, InboxItem,
    InboxQuery, Message, NewRequest, Reply, SendMessage, Thread, ThreadSummary, UnreadAfter,
    UnreadItem, MAX_SUBJECT_CHARS,
};
pub use oauth::{
    canonical_resource, error_redirect, redirect_host, redirect_is_loopback, redirect_matches,
    validate_cimd, validate_redirect_uri, Approval, AuthorizationStatus, AuthorizeInput,
    ClientProfile, ClientRegistration, CodeExchange, IssuedTokens, OAuthFail, OauthGrant,
    PendingAuthorization, PendingView, RefreshExchange, RegisteredClient, ACCESS_TTL_SECS,
};
pub use queue::{
    AdmitItem, AuditFinding, DoneItem, EditItem, FailedNotice, NewQueueDelegate, PointerKind,
    ProposeItem, QueueAdmitted, QueueAudit, QueueCaller, QueueClaim, QueueDelegate, QueueEdited,
    QueueFilter, QueueGrace, QueueHistoryEntry, QueueItem, QueueKind, QueueLimit, QueueLimits,
    QueueNeedsOwner, QueueNext, QueuePriority, QueueRefusal, QueueResult, QueueState, QuietClaim,
    ReleaseItem, SelfAdmitted, DEFAULT_QUEUE_GRACE_SECONDS, DEFAULT_QUEUE_LIMIT,
    MAX_QUEUE_GRACE_SECONDS, MAX_QUEUE_LIMIT, MIN_QUEUE_GRACE_SECONDS,
};
pub use repo::{link_for as repo_link_for, normalize_remote, RepoLink};
pub use requests::{Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate};
pub use resources::{
    derive_port, Lease, LeaseMode, LeaseOutcome, RegisterResource, Resource, ResourceFilter,
    ResourceKind, ResourceRef, DEFAULT_LEASE_TTL_SECONDS, MAX_LEASE_TTL_SECONDS,
    MIN_LEASE_TTL_SECONDS,
};
pub use stats::{BlockedEdit, TrialStats, WakeRecord};
pub use store::{
    allow_dev_migrate, default_home, BuildStamp, Clock, DbInspection, Event, HomeOrigin, NodeInfo,
    Store, Verified, BACKUP_DIR, DB_FILE, ORIGIN_FILE,
};
/// For the workspace's tests only: build a home at an older schema, and open as another build.
#[cfg(feature = "test-support")]
pub use store::{apply_schema, Build};
pub use time::{fmt_ts, parse_ts};
pub use tokens::{AuditEntry, AuditRecord, NewToken, Token, TokenScope};
pub use transport::{
    attention_reason, identify, newest_at_target, resolve_dir, session_file_key, shown_state,
    ConfirmedGone, Identity, Observation, PrunedSession, Reservation, Session, TransportSession,
    TransportState, TransportWake, Tri, WakeKind, WakeOutcome, GONE_CONFIRM_AFTER_MS,
    TRANSPORT_GONE_EVENT, TRANSPORT_WAKE_EVENT, UNDELIVERED_LIMIT,
};
pub use wake::{refuse_option_like_target, WakeTarget, WakeVia};
