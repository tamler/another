//! Another core: local-first messaging and coordination for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod activity;
mod actor_leases;
mod agent_contract;
mod board;
mod checkout;
pub mod crypto;
mod dashboard;
mod error;
mod federation;
mod holds;
mod identity;
mod lease_policy;
mod mail;
mod native_actor;
mod oauth;
mod queue;
mod repo;
mod requests;
mod resources;
mod stats;
mod store;
mod sync_skip;
/// For the workspace's tests only: forge and read back signed wire events.
#[cfg(feature = "test-support")]
pub mod test_support;
mod time;
mod tokens;
mod transport;
mod wake;

pub use activity::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, ActivitySummary, AgentPresence,
    DigestMessage, DigestRequest, RequestCounts, Since, WhatsNew,
};
pub use actor_leases::{
    ActorProof, ActorView, CheckoutRight, EditOutcome, EditReceipt, LeaseRef, OperationView,
    ParticipantProof, RegisteredActor, RightLease, SubjectKind, IDLE_REMINDER,
};
pub use agent_contract::{AgentEnvelope, AgentOperation, CompatibilityAction};
pub use board::{
    Board, BoardAgent, BoardBlocked, BoardEdit, BoardHold, BoardProject, BoardQuery, BoardRequest,
    BOARD_EVENT_SCAN_CAP, DEFAULT_BOARD_LIMIT, DEFAULT_BOARD_WINDOW_MS, EDIT_HOOK_HOLD_REASON,
    MAX_BOARD_LIMIT,
};
pub use checkout::{shell_word, ListedHold, PathRelease, ReleasePath};
pub use dashboard::{
    DashAccess, DashActivityItem, DashActivityPage, DashActivityQuery, DashAgent, DashBlocked,
    DashClash, DashCounts, DashEdit, DashHold, DashMessage, DashProject, DashRequest, DashThread,
    DashboardSnapshot, DashboardView,
};
pub use error::{Error, ErrorBody, Result};
pub use federation::{
    hub_url_is_http, reachable, validate_node_id, IngestReport, Invite, KeyConflict, NodeEntry,
    PairRefusal, PairRequest, PairResponse, Peer, Perm, PullAnchors, PullResponse, PushRequest,
    Rejected, WireEvent, MAX_EVENT_WIRE_BYTES, PULL_BODY_LIMIT_BYTES, PULL_PAGE_BUDGET_BYTES,
    PUSH_BATCH_BUDGET_BYTES, PUSH_BODY_LIMIT_BYTES, PUSH_ENVELOPE_BYTES,
};
pub use holds::{
    is_repo_wide, patterns_overlap, AcquireHolds, EditGuard, HeartbeatReport, Hold, HoldConflict,
    HoldFilter, HoldReport, NotCompared, SessionInfo, SessionPhase, SweepReport,
    DEFAULT_HOLD_TTL_SECONDS, MAX_HOLD_PATTERN_BYTES, MAX_HOLD_REASON_BYTES, MAX_HOLD_TTL_SECONDS,
    MIN_HOLD_TTL_SECONDS,
};
pub use identity::{
    Agent, Program, Project, RegisterAgent, MAX_AGENT_DESCRIPTION_BYTES, MAX_AGENT_MODEL_BYTES,
};
pub use lease_policy::{checkout_ttl, DEFAULT_TTL_SECONDS, MAX_TTL_SECONDS, MIN_TTL_SECONDS};
pub use mail::{
    display_line, is_unsafe_text_char, reply_subject, single_line, AckOutcome, Importance, Inbox,
    InboxItem, InboxQuery, Message, NewRequest, Reply, SendMessage, Thread, ThreadSummary,
    UnreadAfter, UnreadItem, MAX_SUBJECT_CHARS,
};
pub use native_actor::{
    cursor_task_child, map_native_actor, NativeBinding, NativeProfile, CURSOR_COVERAGE,
};
pub use oauth::{
    canonical_resource, error_redirect, not_plain_redirect, redirect_host, redirect_is_loopback,
    redirect_matches, shown_redirect_host, validate_cimd, validate_redirect_uri, Approval,
    AuthorizationStatus, AuthorizeInput, ClientProfile, ClientRegistration, CodeExchange,
    IssuedTokens, NotPlainClient, OAuthFail, OauthGrant, PendingAuthorization, PendingView,
    RefreshExchange, RegisteredClient, ACCESS_TTL_SECS, CLIENT_REDIRECT_NOT_PLAIN,
    NOT_A_PLAIN_HOST, REDIRECT_URI_NOT_PLAIN,
};
pub use queue::{
    queue_stamp, AdmitItem, AuditFinding, DoneItem, EditItem, FailedNotice, HeldClaim,
    NewQueueDelegate, PointerKind, ProposeItem, QueueAdmitted, QueueAudit, QueueCaller, QueueClaim,
    QueueClaimed, QueueDelegate, QueueEdited, QueueFilter, QueueGrace, QueueHistoryEntry,
    QueueItem, QueueKind, QueueLimit, QueueLimits, QueueNeedsOwner, QueueNext, QueueOffer,
    QueuePriority, QueueRefusal, QueueResult, QueueState, QueueSweepReport, QuietClaim,
    ReleaseItem, SelfAdmitted, DEFAULT_QUEUE_GRACE_SECONDS, DEFAULT_QUEUE_LIMIT,
    MAX_QUEUE_GRACE_SECONDS, MAX_QUEUE_LIMIT, MIN_QUEUE_GRACE_SECONDS, QUEUE_SESSION_CLIENTS,
};
pub use repo::{git_root, link_for as repo_link_for, normalize_remote, resolve_existing, RepoLink};
pub use requests::{
    Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate, MAX_REQUEST_NOTE_BYTES,
};
pub use resources::{
    derive_port, Lease, LeaseMode, LeaseOutcome, RegisterResource, Resource, ResourceFilter,
    ResourceKind, ResourceRef, DEFAULT_LEASE_TTL_SECONDS, MAX_LEASE_TTL_SECONDS,
    MAX_RESOURCE_ENDPOINT_BYTES, MAX_RESOURCE_METADATA_BYTES, MAX_RESOURCE_PURPOSE_BYTES,
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
pub use sync_skip::{sync_skip_lines, Skip, SkipAck, SkippedEvent, SYNC_SKIPS_SHOWN};
pub use time::{fmt_ts, now_ms, parse_duration, parse_ts};
pub use tokens::{AuditEntry, AuditRecord, NewToken, Token, TokenScope};
pub use transport::{
    attention_reason, identify, newest_at_target, resolve_dir, session_file_key, shown_state,
    ConfirmedGone, Identity, Observation, PrunedSession, Reservation, Session, TransportSession,
    TransportState, TransportWake, Tri, WakeKind, WakeOutcome, GONE_CONFIRM_AFTER_MS,
    TRANSPORT_GONE_EVENT, TRANSPORT_WAKE_EVENT, UNDELIVERED_LIMIT,
};
pub use wake::{
    refuse_option_like_target, valid_transport_name, validate_target, WakeTarget, WakeVia,
};
