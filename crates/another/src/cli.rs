//! The `another` command line. Every command accepts `--json` for scripts and hooks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use another_core::{
    AcquireHolds, ActivityKind, ActivityQuery, HoldFilter, Importance, InboxQuery, LeaseMode,
    NewRequest, Program, RegisterAgent, RegisterResource, Reply, RequestAction, RequestFilter,
    RequestStatus, RequestUpdate, ResourceFilter, ResourceKind, ResourceRef, SendMessage,
    SessionInfo, SessionPhase, Since, Store,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::hooks::{self, Client, HookEnv, HookEvent};
use crate::output;

#[derive(Debug, Parser)]
#[command(
    name = "another",
    version,
    about = "Local-first inbox, requests and path holds for coding agents"
)]
pub struct Cli {
    /// Data directory (default: $ANOTHER_HOME, else ~/.another).
    #[arg(long, global = true, env = "ANOTHER_HOME", hide_env_values = true)]
    pub home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct AgentArg {
    /// Acting agent: name or name@project (default: $ANOTHER_AGENT).
    #[arg(long, short = 'a', env = "ANOTHER_AGENT")]
    pub agent: String,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the data directory and database if needed; show the local node.
    Init,
    /// Show the local node, database location and registered agents.
    Status,
    /// Check the setup: store, background service, server, tokens, client hooks (and Codex hook
    /// trust), ports and resources. Prints PASS / WARN / FAIL with a fix for each problem; exits
    /// 1 on FAIL.
    Doctor {
        /// The `another serve` HTTP address to check. Default: the installed background service
        /// when it uses this home, otherwise http://127.0.0.1:7466.
        #[arg(long, env = "ANOTHER_SERVER")]
        server: Option<String>,
    },
    /// Run the daemon: MCP over loopback streamable HTTP (default) or stdio.
    Serve {
        /// Serve MCP on stdin/stdout instead of HTTP (for MCP client configs).
        #[arg(long)]
        stdio: bool,
        /// HTTP bind address. Loopback unless --auth and --allow-remote-bind are both given.
        #[arg(long, default_value = crate::serve::DEFAULT_BIND)]
        bind: String,
        /// Require bearer tokens (`another token create`) on /mcp and /activity; audit every call.
        #[arg(long)]
        auth: bool,
        /// Allow a non-loopback --bind (requires --auth). Prefer a tunnel instead.
        #[arg(long, requires = "auth")]
        allow_remote_bind: bool,
        /// Extra Host header to accept, e.g. a tunnel hostname (repeatable; requires --auth).
        #[arg(long = "allowed-host", requires = "auth")]
        allowed_hosts: Vec<String>,
        /// Header your local tunnel sets with the client address, for the /join limit and the
        /// audit log (requires --auth). Without it, forwarding headers are ignored.
        #[arg(long, value_enum, requires = "auth")]
        trusted_proxy_header: Option<crate::remote::ForwardedHeader>,
        /// Act as a federation hub: serve /fed/* for paired spokes (node-signed requests).
        #[arg(long)]
        federation: bool,
        /// Seconds between sweeps that expire holds and stale requests.
        #[arg(long, default_value_t = 30)]
        sweep_secs: u64,
    },
    /// Opt-in background service running `another serve` (launchd on macOS, systemd on
    /// Linux). Nothing is installed unless you run `another service install`.
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Register projects or list them.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Register agents, list them, or show one.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Send a message.
    Send {
        /// Sender: name or name@project (default: $ANOTHER_AGENT).
        #[arg(long, short = 'f', env = "ANOTHER_AGENT")]
        from: String,
        /// Recipients (repeat or comma-separate).
        #[arg(long, short = 't', value_delimiter = ',', required = true)]
        to: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        cc: Vec<String>,
        #[arg(long, short = 's')]
        subject: String,
        /// Message body; `-` reads stdin.
        #[arg(long, short = 'b', default_value = "")]
        body: String,
        #[arg(long, short = 'i', default_value = "normal")]
        importance: String,
        /// Ask recipients to acknowledge.
        #[arg(long)]
        ack: bool,
        /// Also open a claimable request.
        #[arg(long)]
        request: bool,
        /// Request due time in seconds (implies --request).
        #[arg(long)]
        due: Option<i64>,
        /// Post into an existing thread.
        #[arg(long)]
        thread: Option<String>,
    },
    /// Reply to a message within its thread.
    Reply {
        message_id: String,
        #[arg(long, short = 'f', env = "ANOTHER_AGENT")]
        from: String,
        /// Reply body; `-` reads stdin.
        #[arg(long, short = 'b')]
        body: String,
        /// Reply to all original recipients.
        #[arg(long)]
        all: bool,
        #[arg(long, short = 'i')]
        importance: Option<String>,
        #[arg(long)]
        ack: bool,
    },
    /// Show an agent's inbox and open requests.
    Inbox {
        #[command(flatten)]
        agent: AgentArg,
        /// Only unread messages.
        #[arg(long)]
        unread: bool,
        /// Only high/urgent messages.
        #[arg(long)]
        urgent: bool,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Mark messages read and acknowledged.
    Ack {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(required = true)]
        message_ids: Vec<String>,
        /// Only mark read; do not acknowledge.
        #[arg(long)]
        read_only: bool,
    },
    /// List threads an agent is part of.
    Threads {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show every message in a thread.
    Thread { thread_id: String },
    /// List requests.
    Requests {
        /// open | claimed | done | stale | cancelled
        #[arg(long)]
        status: Option<String>,
        /// Owner name or name@project.
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Claim, release, complete or cancel a request.
    Request {
        /// claim | release | complete | cancel
        action: String,
        request_id: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        note: Option<String>,
        /// On claim: due in this many seconds.
        #[arg(long)]
        due: Option<i64>,
    },
    /// Acquire, check, renew or release path holds.
    #[command(subcommand)]
    Hold(HoldCmd),
    /// List live path holds.
    Holds {
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// Group by lane.
        #[arg(long)]
        by_lane: bool,
    },
    /// Session heartbeat: renew holds; with --end release them and go offline.
    Heartbeat {
        #[command(flatten)]
        agent: AgentArg,
        /// Session is starting: record a `session.started` event.
        #[arg(long, conflicts_with = "end")]
        start: bool,
        #[arg(long)]
        end: bool,
        /// With --end: keep resource leases (servers meant to outlive the session).
        #[arg(long, requires = "end")]
        keep: bool,
    },
    /// Find registered resources (model servers, dev servers, ports, GPUs, containers, jobs).
    Resources {
        /// model_server | dev_server | port | gpu | container | long_job
        #[arg(long)]
        kind: Option<String>,
        /// Substring of name, endpoint or purpose.
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Include released resources.
        #[arg(long)]
        all: bool,
        /// Only resources owned by this node (default includes other nodes').
        #[arg(long)]
        local: bool,
    },
    /// Register a resource or renew resource leases.
    #[command(subcommand)]
    Resource(ResourceCmd),
    /// Borrow a shared resource (id or kind:name).
    Borrow {
        target: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Release your lease on a resource (id or kind:name).
    Release {
        target: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Owner: the resource was shut down; end all leases and mark it released.
        #[arg(long)]
        stopped: bool,
    },
    /// List orphan candidates: lapsed resources and unregistered listeners.
    Orphans {
        /// Skip the listening-process scan (lapsed registered resources only).
        #[arg(long)]
        no_scan: bool,
    },
    /// Reap an orphan (resource id, kind:name, or pid:N). Without --confirm, only shows the plan.
    Reap {
        target: String,
        /// Seconds the target must have been orphaned (or alive, for pid targets).
        #[arg(long, default_value_t = crate::reap::DEFAULT_GRACE_SECONDS)]
        grace: i64,
        /// Human confirmation: actually send SIGTERM. Agents never pass this.
        #[arg(long)]
        confirm: bool,
        /// With --confirm: SIGKILL if the process survives SIGTERM.
        #[arg(long, requires = "confirm")]
        force: bool,
    },
    /// Create, list or revoke bearer tokens for remote peers.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Show the audit log of remote (token-authenticated) calls.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show or rename this node.
    #[command(subcommand)]
    Node(NodeCmd),
    /// Pair nodes for federation: invite (hub), join (spoke), revoke.
    #[command(subcommand)]
    Pair(PairCmd),
    /// List paired nodes.
    Peers,
    /// Spoke: push this node's events to the hub and pull everyone else's.
    Sync {
        /// Keep syncing every --every seconds.
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 30)]
        every: u64,
    },
    /// Nudge agents with unread mail through tmux, cmux or Herdr (optional adapters).
    #[command(args_conflicts_with_subcommands = true)]
    Wake {
        #[command(subcommand)]
        cmd: Option<WakeCmd>,
        #[command(flatten)]
        run: WakeRun,
    },
    /// Lanes: named units of work with a charter, lead, status and structured reports.
    #[command(subcommand)]
    Lane(LaneCmd),
    /// Node settings: `another config` lists them, `another config KEY` shows one,
    /// `another config KEY VALUE` sets it (e.g. `another config decider owner`).
    Config {
        key: Option<String>,
        value: Option<String>,
    },
    /// Decisions: structured questions from agents to a person, answered here or by the
    /// manager bot.
    #[command(subcommand)]
    Decision(DecisionCmd),
    /// Invites for remote agents: single-use codes that admit an agent with a scoped token.
    #[command(subcommand)]
    Invite(InviteCmd),
    /// Join a hub as a remote agent with an invite code (saves a profile, writes MCP config).
    Join {
        /// The hub's URL, e.g. https://my-laptop.my-tailnet.ts.net
        hub: String,
        /// The invite code (anj_…).
        code: String,
        /// Write MCP client config: auto, claude, codex, both or none.
        #[arg(long, value_enum, default_value = "none")]
        write_mcp: crate::join::WriteMcp,
        /// Send a session-start heartbeat and a "joined" report to the invite's lane.
        #[arg(long)]
        hello: bool,
    },
    /// Act on the hub you joined (uses the profile `another join` saved).
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Daily summary: what needs you, lanes, decisions, requests, holds, spend, agents.
    #[command(args_conflicts_with_subcommands = true)]
    Summary {
        #[command(subcommand)]
        cmd: Option<SummaryCmd>,
        /// Duration (24h), RFC 3339 time, or event id.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Spend ledger: budgets, spend entries, hourly leases with approval, burn-down report.
    #[command(args_conflicts_with_subcommands = true)]
    Spend {
        #[command(subcommand)]
        cmd: Option<SpendCmd>,
        #[command(flatten)]
        report: SpendReportArgs,
    },
    /// Webhooks: signed HTTP POSTs for lane, request, hold and lease events.
    #[command(subcommand)]
    Webhook(WebhookCmd),
    /// Show the activity feed (who is doing what), or a summary of it.
    Activity {
        /// Start point: a duration (30m, 2h, 1d), an RFC 3339 time, or an event id cursor.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Only these categories: mail, request, hold, resource, session, system, federation.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Print a summary (counts, active agents, highlights) instead of items.
        #[arg(long)]
        summary: bool,
    },
    /// Install, remove or run client hook packs.
    #[command(subcommand)]
    Hook(HookCmd),
    /// Expire lapsed holds and mark overdue requests stale now.
    Sweep,
    /// Show the most recent event-log records.
    Events {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Args)]
pub struct WakeRun {
    /// Agent to wake (default: $ANOTHER_AGENT); or use --all.
    #[arg(long, short = 'a', env = "ANOTHER_AGENT", conflicts_with = "all")]
    agent: Option<String>,
    /// Wake every agent with a stored target that has unread mail or open requests.
    #[arg(long)]
    all: bool,
    /// Adapter: tmux | cmux | herdr (default: the agent's stored target).
    #[arg(long)]
    via: Option<String>,
    /// tmux pane, cmux surface id (or `notify`), or Herdr agent name.
    #[arg(long)]
    target: Option<String>,
    /// Text to send (default: a one-line inbox summary).
    #[arg(long)]
    message: Option<String>,
    /// Wake even when nothing is unread.
    #[arg(long)]
    force: bool,
    /// Print the adapter commands without running them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum WakeCmd {
    /// Store where to wake an agent (hooks capture tmux/cmux targets automatically).
    Set {
        #[command(flatten)]
        agent: AgentArg,
        /// tmux | cmux | herdr
        #[arg(long)]
        via: String,
        #[arg(long)]
        target: String,
    },
    /// Forget an agent's wake target.
    Clear {
        #[command(flatten)]
        agent: AgentArg,
    },
    /// List stored wake targets.
    List,
}

#[derive(Debug, Subcommand)]
pub enum InviteCmd {
    /// Mint an invite. Prints the code once (and, with --print-snippet, a join script).
    Create {
        /// Name the joining agent gets.
        #[arg(long)]
        agent: String,
        /// claude | codex | grok | grok_bot | cursor | other
        #[arg(long, default_value = "other")]
        program: String,
        /// Project path or slug glob the agent may touch (repeatable, required).
        #[arg(long = "project", short = 'p', required = true)]
        projects: Vec<String>,
        /// How long the code is valid (default 24h, at most 30d).
        #[arg(long, default_value = "24h")]
        expires: String,
        /// How many times it can be redeemed (each use gets its own token).
        #[arg(long, default_value_t = 1)]
        uses: u32,
        /// Lane the agent joins (id, name or name@project).
        #[arg(long)]
        lane: Option<String>,
        /// Ephemeral agents leave after this many hours unseen (default: the node's
        /// `remote_idle_hours` setting, 4 unless changed with `another config`).
        #[arg(long)]
        idle_hours: Option<u32>,
        /// Keep the agent after its session ends or it goes idle.
        #[arg(long)]
        persistent: bool,
        /// Expiry of issued tokens (default: none).
        #[arg(long)]
        token_ttl: Option<String>,
        /// Also print a shell snippet for a cloud session (needs --hub-url).
        #[arg(long, requires = "hub_url")]
        print_snippet: bool,
        /// The hub URL the remote agent will use.
        #[arg(long)]
        hub_url: Option<String>,
    },
    /// List invites and their status.
    List,
    /// Stop an invite from being redeemed.
    Revoke { invite_id: String },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCmd {
    /// Show the saved profile and send a heartbeat.
    Status,
    /// Heartbeat (renews holds and leases); --end ends the session.
    Heartbeat {
        #[arg(long)]
        end: bool,
    },
    /// Lanes in your scope on the hub.
    Lanes,
    /// Post a status report to a lane (default: the invite's lane).
    Report {
        #[arg(long)]
        lane: Option<String>,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        progress: Option<u8>,
    },
    /// Open decisions in your scope.
    Decisions,
    /// Answer a decision on the hub (as the relaying agent).
    Answer {
        decision: String,
        value: Option<String>,
        #[arg(long)]
        text: Option<String>,
        /// The person who decided.
        #[arg(long = "as")]
        answered_by: Option<String>,
    },
    /// Leave the hub (ephemeral agents: token revoked, holds released) and forget the profile.
    Leave,
}

#[derive(Debug, Subcommand)]
pub enum DecisionCmd {
    /// Ask a person (default: `another config decider`, else owner) a question with 2-6 options.
    Ask {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long, short = 'q')]
        question: String,
        /// `value=Label` (repeat 2-6 times), e.g. `--option ship="Ship behind a flag"`.
        #[arg(long = "option", short = 'o', required = true)]
        options: Vec<String>,
        /// Accept a free-text answer too.
        #[arg(long)]
        free_text: bool,
        /// Context link (repeatable).
        #[arg(long = "link")]
        links: Vec<String>,
        /// Duration (2h) or RFC 3339 time.
        #[arg(long)]
        deadline: Option<String>,
        /// choose_default, cancel or keep_open (default).
        #[arg(long)]
        on_timeout: Option<String>,
        /// Option value applied by choose_default.
        #[arg(long)]
        default: Option<String>,
        /// Who to ask.
        #[arg(long)]
        to: Option<String>,
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        lane: Option<String>,
    },
    /// List decisions (open ones unless --status or --all).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        lane: Option<String>,
        /// Only decisions for this person.
        #[arg(long = "for")]
        human: Option<String>,
    },
    /// Show a decision; --markdown prints the short phone-readable form.
    Show {
        decision: String,
        #[arg(long)]
        markdown: bool,
    },
    /// Answer a decision: an option value (or its number), and/or --text if allowed.
    Answer {
        decision: String,
        value: Option<String>,
        #[arg(long)]
        text: Option<String>,
        /// Who is answering (default: the person asked).
        #[arg(long = "as")]
        answered_by: Option<String>,
        /// The manager or person on this node relaying the answer (needed for another node's
        /// decision, unless --as names a person registered here).
        #[arg(long)]
        relayed_by: Option<String>,
    },
    /// Withdraw a decision you asked.
    Cancel {
        decision: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Debug, clap::Args)]
pub struct SpendReportArgs {
    /// current (default), previous, or YYYY-MM.
    #[arg(long, default_value = "current")]
    period: String,
    #[arg(long)]
    account: Option<String>,
    #[arg(long)]
    lane: Option<String>,
    /// Only this agent's spend.
    #[arg(long = "agent")]
    agent_filter: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum SpendCmd {
    /// Manage accounts (budgets).
    #[command(subcommand)]
    Account(SpendAccountCmd),
    /// Record an amount spent.
    Record {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        account: String,
        /// In the account's unit.
        #[arg(long)]
        amount: f64,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Start an hourly lease (asks for approval if the account requires it).
    Borrow {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        account: String,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long)]
        cost_per_hour: Option<f64>,
        /// Lease seconds, renewed by heartbeat (default 3600).
        #[arg(long)]
        ttl: Option<i64>,
        /// Expected hours (shown in the approval question).
        #[arg(long)]
        hours: Option<f64>,
        #[arg(long)]
        note: Option<String>,
    },
    /// End your lease (its cost is recorded) or withdraw a pending one.
    Release {
        lease_id: String,
        #[command(flatten)]
        agent: AgentArg,
    },
    /// List leases (open ones unless --all).
    Leases {
        #[arg(long)]
        all: bool,
    },
    /// Charge borrowing a registered resource to an account, e.g.
    /// `another spend link runpod gpu_pod:a100`. Borrowers then get a spend lease (behind
    /// the account's approval gate) that accrues while their resource lease lives.
    Link {
        account: String,
        /// Resource id or kind:name.
        resource: String,
    },
    /// Stop charging a resource to its account (open spend leases run to their end).
    Unlink {
        /// Resource id or kind:name.
        resource: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SummaryCmd {
    /// Opt-in daily rollup sent by `another serve`, e.g.
    /// `another summary schedule --daily 08:30 --to mail:owner`. Without flags, shows it.
    Schedule {
        /// Local time, HH:MM (24-hour).
        #[arg(long, conflicts_with = "off")]
        daily: Option<String>,
        /// `webhook` (local webhooks with the daily_summary filter) or `mail:NAME`.
        #[arg(long, conflicts_with = "off")]
        to: Option<String>,
        /// Turn the rollup off.
        #[arg(long)]
        off: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SpendAccountCmd {
    /// Add an account, e.g. `another spend account add claude-api --kind api_credits --unit USD --budget 200`.
    Add {
        name: String,
        /// gpu_pod, api_credits, plan_reset, server, domain.
        #[arg(long)]
        kind: String,
        /// USD, credits, resets, hours, ...
        #[arg(long)]
        unit: String,
        /// Budget per period, in the unit.
        #[arg(long)]
        budget: Option<f64>,
        /// monthly (default) or none.
        #[arg(long, default_value = "monthly")]
        period: String,
        /// Day of the month the period resets (1-28).
        #[arg(long, default_value_t = 1)]
        reset_day: u32,
        #[arg(long)]
        cost_per_hour: Option<f64>,
        /// Borrowing asks a person first (a decision).
        #[arg(long)]
        requires_approval: bool,
        /// Who approves (default: the node's decider, `another config decider NAME`).
        #[arg(long)]
        approver: Option<String>,
        /// Owning project; omit for a shared account.
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// List accounts.
    List,
    /// Change an account.
    Update {
        name: String,
        #[arg(long, conflicts_with = "clear_budget")]
        budget: Option<f64>,
        #[arg(long)]
        clear_budget: bool,
        #[arg(long)]
        reset_day: Option<u32>,
        #[arg(long)]
        cost_per_hour: Option<f64>,
        #[arg(long)]
        requires_approval: Option<bool>,
        #[arg(long)]
        approver: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Archive an account (hidden from reports; history kept).
    Archive { name: String },
}

#[derive(Debug, Subcommand)]
pub enum WebhookCmd {
    /// Subscribe a URL. Prints the signing secret once.
    Add {
        /// `https://` URL (plain http only to 127.0.0.1, localhost or [::1]).
        #[arg(long)]
        url: String,
        /// Comma-separated filters: lane_status, lane_report, needs_decision,
        /// request_created, request_answered, hold_conflict, lease_expiring, budget,
        /// daily_summary (with --all-projects only), all.
        #[arg(long, value_delimiter = ',', required = true)]
        events: Vec<String>,
        /// Project path or slug glob (repeatable). Required unless --all-projects.
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        /// Every project on this node.
        #[arg(long, conflicts_with = "projects")]
        all_projects: bool,
        #[arg(long)]
        description: Option<String>,
    },
    /// List subscriptions with delivery counts (secrets are never shown).
    List,
    /// Remove a subscription; pending deliveries are dropped.
    Remove { webhook_id: String },
    /// Send a signed test delivery now and show the result.
    Test { webhook_id: String },
    /// Show dead deliveries (gave up after 7 attempts), newest first.
    Dead {
        #[arg(long)]
        webhook: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show recent deliveries of any status.
    Deliveries {
        #[arg(long)]
        webhook: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Re-queue a dead delivery.
    Retry { delivery_id: String },
}

#[derive(Debug, Subcommand)]
pub enum LaneCmd {
    /// Create a lane (status starts at planned).
    Create {
        name: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Agent that leads the work.
        #[arg(long)]
        lead: String,
        /// The brief; `-` reads stdin.
        #[arg(long, default_value = "")]
        charter: String,
        #[arg(long)]
        status: Option<String>,
    },
    /// List lanes (open ones unless --all or --status).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        lead: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Show a lane with links and recent reports.
    Show {
        lane: String,
        /// Print as markdown (like the old STATUS files).
        #[arg(long)]
        markdown: bool,
    },
    /// Change status, charter or lead, or link threads / requests / holds / messages.
    Update {
        lane: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        charter: Option<String>,
        #[arg(long)]
        lead: Option<String>,
        #[arg(long = "link-thread")]
        link_threads: Vec<String>,
        #[arg(long = "link-request")]
        link_requests: Vec<String>,
        #[arg(long = "link-hold")]
        link_holds: Vec<String>,
        #[arg(long = "link-message")]
        link_messages: Vec<String>,
    },
    /// Post a status report (also mailed to the lane's counterpart).
    Report {
        lane: String,
        #[command(flatten)]
        agent: AgentArg,
        /// `-` reads stdin.
        #[arg(long, short = 's')]
        summary: String,
        #[arg(long)]
        progress: Option<u8>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long = "link")]
        links: Vec<String>,
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
    },
    /// Post the DONE report: outcome and optional PR link; closes the lane.
    Done {
        lane: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        pr: Option<String>,
        #[arg(long, short = 's')]
        summary: Option<String>,
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
    },
    /// Close a lane without a report (done, or abandoned with --abandon).
    Close {
        lane: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        abandon: bool,
        #[arg(long)]
        outcome: Option<String>,
    },
    /// Write a lane as markdown: to stdout, --out FILE, or --dir DIR (<name>-STATUS.md, plus
    /// DONE-<name>.md when done).
    Export {
        lane: String,
        #[arg(long, conflicts_with = "dir")]
        out: Option<PathBuf>,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCmd {
    /// Show this node's id, name and key fingerprint.
    Show,
    /// Rename this node (do this before pairing; names must be unique across nodes).
    /// Without a name, prints the current one.
    Name { name: Option<String> },
}

#[derive(Debug, Subcommand)]
pub enum PairCmd {
    /// Hub: print a single-use invite code for a spoke.
    Invite {
        /// URL spokes use to reach this hub (e.g. its tunnel URL).
        #[arg(long)]
        url: String,
        /// Permissions for the spoke: read_feed, send_mail, borrow_resource (comma-separated).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "read_feed,send_mail,borrow_resource"
        )]
        perms: Vec<String>,
        /// Invite lifetime: seconds or a duration like 30m, 24h.
        #[arg(long, default_value = "1h")]
        expires: String,
    },
    /// Spoke: redeem an invite code and record the hub.
    Join { code: String },
    /// Stop accepting (or using) a paired node, by name or id.
    Revoke { node: String },
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Create a token that acts as one agent within project globs. Printed once.
    Create {
        /// The agent the token acts as (register it first, e.g. with --program grok_bot).
        #[arg(long, short = 'a')]
        agent: String,
        /// Project path or slug glob the token may touch (repeatable), e.g. '/work/*' or shop.
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        /// A note to recognise the token by.
        #[arg(long)]
        label: Option<String>,
        /// Expiry: seconds or a duration like 12h, 30d.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List tokens: id, agent, project globs, status and last use (never the secret).
    List,
    /// Revoke a token by id (takes effect on the next request).
    Revoke { token_id: String },
}

fn parse_secs(raw: &str) -> anyhow::Result<i64> {
    let raw = raw.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {raw:?}"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => anyhow::bail!("invalid duration {raw:?}: use s, m, h, d or w"),
    };
    Ok(n * mult)
}

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install (or update in place) and start the service; it restarts at login and reboot.
    /// Takes `serve`'s HTTP flags; `--home` selects the data directory it uses. An update
    /// replaces every flag, so repeat the ones you want to keep.
    Install {
        /// HTTP bind address. Loopback unless --auth and --allow-remote-bind are both given.
        #[arg(long, default_value = crate::serve::DEFAULT_BIND)]
        bind: String,
        /// Require bearer tokens (needed for remote access).
        #[arg(long)]
        auth: bool,
        /// Allow a non-loopback --bind (requires --auth). Prefer a tunnel instead.
        #[arg(long, requires = "auth")]
        allow_remote_bind: bool,
        /// Extra Host header to accept, e.g. a tunnel hostname (repeatable; requires --auth).
        #[arg(long = "allowed-host", requires = "auth")]
        allowed_hosts: Vec<String>,
        /// Header your local tunnel sets with the client address, for the /join limit and the
        /// audit log (requires --auth). Without it, forwarding headers are ignored.
        #[arg(long, value_enum, requires = "auth")]
        trusted_proxy_header: Option<crate::remote::ForwardedHeader>,
        /// Also act as a federation hub.
        #[arg(long)]
        federation: bool,
    },
    /// Stop the service and remove it (safe when it is not installed).
    Uninstall,
    /// Show whether it is installed and loaded, its pid, bind address and recent log lines.
    Status {
        #[arg(long, default_value_t = 10)]
        lines: usize,
    },
    /// Start the installed service.
    Start,
    /// Stop the installed service; it stays installed (launchd: loads again at next login;
    /// systemd: starts again at next boot or user-manager start). Uninstall to remove it.
    Stop,
    /// Print the service's recent log lines.
    Logs {
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum ResourceCmd {
    /// Register a running resource and take the owner lease.
    Register {
        /// model_server | dev_server | port | gpu | container | long_job
        kind: String,
        /// Identity, e.g. ollama@11434, vite@5173, tcp:127.0.0.1:5432, metal:0.
        name: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        pid: Option<u32>,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        purpose: Option<String>,
        /// Refuse borrowers.
        #[arg(long)]
        exclusive: bool,
        /// Lease seconds (default 1800, max 86400).
        #[arg(long)]
        ttl: Option<i64>,
        /// Metadata as key=value (repeatable), e.g. --meta model=llama3 --meta backend=metal.
        #[arg(long = "meta", value_name = "KEY=VALUE")]
        meta: Vec<String>,
    },
    /// Renew your resource leases (all, or the given ids / kind:name).
    Heartbeat {
        #[command(flatten)]
        agent: AgentArg,
        targets: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ClientSel {
    Claude,
    Codex,
    Grok,
    Cursor,
    All,
}

fn clients(sel: &[ClientSel]) -> Vec<Client> {
    let mut out = Vec::new();
    for s in sel {
        let add: &[Client] = match s {
            ClientSel::Claude => &[Client::Claude],
            ClientSel::Codex => &[Client::Codex],
            ClientSel::Grok => &[Client::Grok],
            ClientSel::Cursor => &[Client::Cursor],
            ClientSel::All => &Client::ALL,
        };
        for c in add {
            if !out.contains(c) {
                out.push(*c);
            }
        }
    }
    out
}

#[derive(Debug, Subcommand)]
pub enum HookCmd {
    /// Add Another hooks to client configs (idempotent; backs up files it changes).
    Install {
        /// Clients to configure (repeat or comma-separate).
        #[arg(long, value_delimiter = ',', required = true)]
        client: Vec<ClientSel>,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also install a Stop hook that posts an automatic report into the agent's lane
        /// (debounced to one every 15 minutes). Installing without it removes that hook.
        #[arg(long)]
        lane_reports: bool,
        /// Also install a pre-edit hook (Claude Code, Codex) that holds files before they are
        /// edited and denies the edit if another lane or agent holds them. Installing without
        /// it removes that hook.
        #[arg(long)]
        pre_edit: bool,
    },
    /// Remove Another hooks from client configs, leaving other hooks untouched.
    Uninstall {
        #[arg(long, value_delimiter = ',', required = true)]
        client: Vec<ClientSel>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show which clients have Another hooks installed.
    Status,
    /// Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0.
    Run {
        event: HookEvent,
        #[arg(long)]
        client: Client,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// Register a project by path (default: current directory).
    Ensure { path: Option<PathBuf> },
    /// List registered projects.
    List,
}

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Register or update an agent.
    Register {
        name: String,
        /// claude | grok | codex | cursor | grok_bot | other
        #[arg(long)]
        program: String,
        /// Project path to scope the name to (enables name@project).
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// List registered agents.
    List,
    /// Show one agent: its program, status and when it was last seen.
    Show { name: String },
    /// Remove an agent now: revoke all its tokens, release its holds and leases, tell its lanes.
    Kick {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Agents that joined through invites, with presence (online, idle, gone).
    Remotes,
}

#[derive(Debug, Args)]
pub struct ProjectArg {
    /// Project path or slug (default: current directory).
    #[arg(long, short = 'p')]
    pub project: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum HoldCmd {
    /// Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted).
    Acquire {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
        /// Shared instead of exclusive.
        #[arg(long)]
        shared: bool,
        #[arg(long)]
        reason: Option<String>,
        /// Lane the holds are for (default: $ANOTHER_LANE, else the one lane you lead here).
        #[arg(long, env = "ANOTHER_LANE")]
        lane: Option<String>,
    },
    /// Report conflicts for paths without acquiring (exit code 3 on conflict).
    Check {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        shared: bool,
    },
    /// Renew your holds (all, or the given ids).
    Renew {
        #[command(flatten)]
        agent: AgentArg,
        hold_ids: Vec<String>,
    },
    /// Release your holds (all, or the given ids).
    Release {
        #[command(flatten)]
        agent: AgentArg,
        hold_ids: Vec<String>,
    },
}

/// Exit status used when holds conflict, so hooks can branch on it.
pub const EXIT_CONFLICT: u8 = 3;

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(matches!(cli.command, Command::Serve { .. }));
    let json = cli.json;
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            let body = match err.downcast_ref::<another_core::Error>() {
                Some(e) => e.body(),
                None => another_core::ErrorBody {
                    code: "error",
                    message: format!("{err:#}"),
                },
            };
            if json {
                println!("{}", serde_json::json!({ "error": body }));
            } else {
                eprintln!("another: {}", body.message);
            }
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(serving: bool) {
    let default = if serving { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("ANOTHER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn open(home: Option<&Path>) -> anyhow::Result<Store> {
    let home = match home {
        Some(h) => h.to_path_buf(),
        None => another_core::default_home()?,
    };
    Ok(Store::open(&home)?)
}

fn body_arg(body: String) -> anyhow::Result<String> {
    if body == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(body)
    }
}

fn abs(path: &Path) -> anyhow::Result<String> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(p.to_string_lossy().into_owned())
}

/// Resolve `--project`: a path (ensured on the fly) or an existing slug; default cwd.
fn project_key(store: &Store, actor: &str, project: Option<String>) -> anyhow::Result<String> {
    let looks_like_path = |p: &str| p.contains('/') || p.contains('\\') || p == "." || p == "..";
    match project {
        Some(p) if !looks_like_path(&p) => Ok(p),
        Some(p) => Ok(store.ensure_project(actor, &abs(Path::new(&p))?)?.abs_path),
        None => Ok(store
            .ensure_project(actor, &abs(&std::env::current_dir()?)?)?
            .abs_path),
    }
}

fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|e| e.to_string())
        );
    } else {
        let text = human(value);
        if !text.is_empty() {
            println!("{}", text.trim_end());
        }
    }
    ExitCode::SUCCESS
}

fn service_command(cmd: &ServiceCmd, json: bool, home: PathBuf) -> anyhow::Result<ExitCode> {
    use crate::service;
    let user_home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir()?.join(home)
    };
    match cmd {
        ServiceCmd::Install {
            bind,
            auth,
            allow_remote_bind,
            allowed_hosts,
            trusted_proxy_header,
            federation,
        } => {
            // Make sure the data directory exists and is initialised before the service runs.
            Store::open(&home)?;
            let spec = service::ServiceSpec {
                // Keep symlinks (e.g. a stable ~/bin/another link) instead of pinning the
                // current target, so upgrades behind the link keep working.
                exe: {
                    let exe = std::env::current_exe()?;
                    if exe.is_absolute() {
                        exe
                    } else {
                        std::env::current_dir()?.join(exe)
                    }
                },
                home,
                bind: bind.clone(),
                auth: *auth,
                allow_remote_bind: *allow_remote_bind,
                allowed_hosts: allowed_hosts.clone(),
                trusted_proxy_header: trusted_proxy_header.map(|h| h.name().to_string()),
                federation: *federation,
            };
            let r = service::install(&spec, &user_home)?;
            Ok(emit(json, &r, |r| {
                let mut out = format!(
                    "{} the background service ({:?}) at {}\nlogs: {}",
                    r.action, r.platform, r.definition, r.logs
                );
                for w in &r.warnings {
                    out.push_str(&format!("\nwarning: {w}"));
                }
                for h in &r.hints {
                    out.push_str(&format!("\nhint: {h}"));
                }
                out
            }))
        }
        ServiceCmd::Uninstall => {
            let r = service::uninstall(&user_home)?;
            Ok(emit(json, &r, |r| {
                if r.removed {
                    format!("removed the background service ({})", r.definition)
                } else {
                    "the background service is not installed".to_string()
                }
            }))
        }
        ServiceCmd::Status { lines } => {
            let r = service::status(&user_home, &home, *lines)?;
            Ok(emit(json, &r, |r| {
                if !r.installed {
                    return "not installed (opt in with `another service install`)".to_string();
                }
                let mut out = format!(
                    "installed: {}\nloaded: {}\npid: {}\nbind: {}",
                    r.definition,
                    if r.loaded { "yes" } else { "no" },
                    r.pid.map_or("-".to_string(), |p| p.to_string()),
                    r.bind.as_deref().unwrap_or("-")
                );
                if !r.log_lines.is_empty() {
                    out.push_str("\nrecent log lines:");
                    for l in &r.log_lines {
                        out.push_str("\n  ");
                        out.push_str(l);
                    }
                }
                out
            }))
        }
        ServiceCmd::Start | ServiceCmd::Stop => {
            let start = matches!(cmd, ServiceCmd::Start);
            service::start_stop(&user_home, start)?;
            let what = if start { "started" } else { "stopped" };
            Ok(emit(
                json,
                &serde_json::json!({ "ok": true, "action": what }),
                |_| format!("{what} the background service"),
            ))
        }
        ServiceCmd::Logs { lines } => {
            let status = service::status(&user_home, &home, *lines)?;
            if !status.installed {
                anyhow::bail!("the background service is not installed");
            }
            Ok(emit(json, &status.log_lines, |l| l.join("\n")))
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let json = cli.json;
    let home = cli.home.as_deref();
    if let Command::Serve {
        stdio,
        bind,
        auth,
        allow_remote_bind,
        allowed_hosts,
        trusted_proxy_header,
        federation,
        sweep_secs,
    } = &cli.command
    {
        let store = open(home)?;
        let every = Duration::from_secs((*sweep_secs).max(1));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            if *stdio {
                crate::serve::serve_stdio(store, every).await
            } else {
                let opts = crate::serve::HttpOptions {
                    auth: *auth,
                    allow_remote_bind: *allow_remote_bind,
                    allowed_hosts: allowed_hosts.clone(),
                    trusted_proxy_header: *trusted_proxy_header,
                    federation: *federation,
                };
                crate::serve::serve_http(store, bind, opts, every).await
            }
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let another_home = || -> anyhow::Result<PathBuf> {
        Ok(match home {
            Some(h) => h.to_path_buf(),
            None => another_core::default_home()?,
        })
    };
    if let Command::Service(cmd) = &cli.command {
        return service_command(cmd, json, another_home()?);
    }
    if let Command::Hook(cmd) = &cli.command {
        return hook_command(cmd, json, another_home);
    }
    if let Command::Doctor { server } = &cli.command {
        // Before opening the store, so a broken store is reported, not fatal.
        let report = crate::doctor::run(&crate::doctor::DoctorOptions {
            another_home: another_home()?,
            user_home: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
            server: server.clone(),
        });
        let failed = report.failed > 0;
        emit(json, &report, crate::doctor::render);
        return Ok(if failed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }
    if let Command::Join {
        hub,
        code,
        write_mcp,
        hello,
    } = &cli.command
    {
        let home = another_home()?;
        let user_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone());
        let out = crate::join::join(&home, &user_home, hub, code, *write_mcp, *hello)?;
        return Ok(emit(json, &out, output::joined));
    }
    if let Command::Remote(cmd) = &cli.command {
        return remote_command(&another_home()?, json, cmd);
    }

    let store = open(home)?;
    let code = match cli.command {
        Command::Serve { .. }
        | Command::Hook(_)
        | Command::Join { .. }
        | Command::Remote(_)
        | Command::Doctor { .. } => {
            unreachable!("handled above")
        }
        Command::Invite(cmd) => invite_command(&store, json, cmd)?,
        Command::Agent(AgentCmd::Kick { name, reason }) => {
            let d = store.kick_agent(&name, reason.as_deref())?;
            emit(json, &d, output::departure)
        }
        Command::Agent(AgentCmd::Remotes) => {
            let list = store.remote_agents()?;
            emit(json, &list, |l| output::remote_agents(l))
        }
        Command::Init | Command::Status => {
            let status = output::Status {
                node: store.node()?,
                db_path: store
                    .db_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                projects: store.projects()?,
                agents: store.agents()?,
            };
            emit(json, &status, output::status)
        }
        Command::Service(_) => unreachable!("handled above"),
        Command::Project(ProjectCmd::Ensure { path }) => {
            let path = match path {
                Some(p) => abs(&p)?,
                None => abs(&std::env::current_dir()?)?,
            };
            let p = store.ensure_project("cli", &path)?;
            emit(json, &p, |p| format!("{}  {}", p.slug, p.abs_path))
        }
        Command::Project(ProjectCmd::List) => {
            let ps = store.projects()?;
            emit(json, &ps, |ps| {
                ps.iter()
                    .map(|p| format!("{}  {}\n", p.slug, p.abs_path))
                    .collect()
            })
        }
        Command::Agent(AgentCmd::Register {
            name,
            program,
            project,
            model,
            description,
        }) => {
            let project = project.as_deref().map(abs).transpose()?;
            let a = store.register_agent_trusted(RegisterAgent {
                name,
                program: program.parse::<Program>()?,
                project,
                model,
                description,
            })?;
            emit(json, &a, |a| {
                format!("registered {} ({})", a.address, a.program)
            })
        }
        Command::Agent(AgentCmd::List) => {
            let agents = store.agents()?;
            emit(json, &agents, |a| output::agents(a))
        }
        Command::Agent(AgentCmd::Show { name }) => {
            let a = store.agent(&name)?;
            emit(json, &a, |a| output::agents(std::slice::from_ref(a)))
        }
        Command::Send {
            from,
            to,
            cc,
            subject,
            body,
            importance,
            ack,
            request,
            due,
            thread,
        } => {
            let m = store.send_message(SendMessage {
                from,
                to,
                cc,
                subject,
                body: body_arg(body)?,
                importance: importance.parse::<Importance>()?,
                ack_required: ack,
                reply_to: None,
                thread_id: thread,
                request: (request || due.is_some()).then_some(NewRequest {
                    due_in_seconds: due,
                }),
            })?;
            emit(json, &m, output::sent)
        }
        Command::Reply {
            message_id,
            from,
            body,
            all,
            importance,
            ack,
        } => {
            let m = store.reply(Reply {
                from,
                message_id,
                body: body_arg(body)?,
                all,
                importance: importance.map(|i| i.parse()).transpose()?,
                ack_required: ack,
            })?;
            emit(json, &m, output::sent)
        }
        Command::Inbox {
            agent,
            unread,
            urgent,
            limit,
        } => {
            let inbox = store.inbox(
                &agent.agent,
                InboxQuery {
                    unread_only: unread,
                    urgent_only: urgent,
                    limit,
                },
            )?;
            emit(json, &inbox, output::inbox)
        }
        Command::Ack {
            agent,
            message_ids,
            read_only,
        } => {
            let out = store.acknowledge(&agent.agent, &message_ids, !read_only)?;
            emit(json, &out, |o| {
                format!(
                    "{} updated, {} unchanged",
                    o.updated.len(),
                    o.unchanged.len()
                )
            })
        }
        Command::Threads { agent, limit } => {
            let threads = store.threads(&agent.agent, limit)?;
            emit(json, &threads, |t| output::threads(t))
        }
        Command::Thread { thread_id } => {
            let thread = store.thread(&thread_id)?;
            emit(json, &thread, output::thread)
        }
        Command::Requests {
            status,
            owner,
            limit,
        } => {
            let reqs = store.requests(RequestFilter {
                status: status.map(|s| s.parse::<RequestStatus>()).transpose()?,
                owner,
                limit,
            })?;
            emit(json, &reqs, |r| output::requests(r))
        }
        Command::Request {
            action,
            request_id,
            agent,
            note,
            due,
        } => {
            let r = store.update_request(RequestUpdate {
                agent: agent.agent,
                request_id,
                action: action.parse::<RequestAction>()?,
                note,
                due_in_seconds: due,
            })?;
            emit(json, &r, |r| output::requests(std::slice::from_ref(r)))
        }
        Command::Hold(HoldCmd::Acquire {
            agent,
            project,
            paths,
            ttl,
            shared,
            reason,
            lane,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let report = store.acquire_holds(AcquireHolds {
                agent: agent.agent,
                project,
                paths,
                ttl_seconds: ttl,
                exclusive: !shared,
                reason,
                lane: lane.filter(|l| !l.trim().is_empty()),
            })?;
            let conflicted = !report.conflicts.is_empty();
            emit(json, &report, output::hold_report);
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Command::Hold(HoldCmd::Check {
            agent,
            project,
            paths,
            shared,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let conflicts = store.check_holds(&agent.agent, &project, &paths, !shared)?;
            let conflicted = !conflicts.is_empty();
            emit(json, &conflicts, |c| {
                if c.is_empty() {
                    "no conflicts".to_string()
                } else {
                    output::conflicts(c)
                }
            });
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Command::Hold(HoldCmd::Renew { agent, hold_ids }) => {
            let holds = store.renew_holds(&agent.agent, &hold_ids)?;
            emit(json, &holds, |h| output::holds(h))
        }
        Command::Hold(HoldCmd::Release { agent, hold_ids }) => {
            let ids = store.release_holds(&agent.agent, &hold_ids)?;
            emit(json, &ids, |ids| format!("released {} hold(s)", ids.len()))
        }
        Command::Holds {
            project,
            agent,
            by_lane: true,
        } => {
            let groups = store.holds_by_lane(HoldFilter { project, agent }, None)?;
            emit(json, &groups, |g| output::lane_holds(g))
        }
        Command::Holds { project, agent, .. } => {
            let holds = store.holds(HoldFilter { project, agent })?;
            emit(json, &holds, |h| output::holds(h))
        }
        Command::Heartbeat {
            agent,
            start,
            end,
            keep,
        } => {
            let phase = match (start, end) {
                (true, _) => SessionPhase::Start,
                (_, true) => SessionPhase::End,
                _ => SessionPhase::Beat,
            };
            let hb = store.session(
                &agent.agent,
                phase,
                SessionInfo {
                    keep_resources: keep,
                    ..Default::default()
                },
            )?;
            emit(json, &hb, output::heartbeat)
        }
        Command::Resources {
            kind,
            query,
            project,
            all,
            local,
        } => {
            let rs = store.find_resources(ResourceFilter {
                kind: kind.map(|k| k.parse::<ResourceKind>()).transpose()?,
                query,
                project,
                include_released: all,
                local_only: local,
            })?;
            emit(json, &rs, |r| output::resources(r))
        }
        Command::Resource(ResourceCmd::Register {
            kind,
            name,
            agent,
            endpoint,
            port,
            pid,
            project,
            purpose,
            exclusive,
            ttl,
            meta,
        }) => {
            let mut metadata = serde_json::Map::new();
            for kv in meta {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--meta expects KEY=VALUE, got {kv:?}"))?;
                metadata.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
            let project = project
                .map(|p| project_key(&store, &agent.agent, Some(p)))
                .transpose()?;
            let out = store.register_resource(RegisterResource {
                agent: agent.agent,
                kind: kind.parse()?,
                name,
                endpoint,
                port,
                pid,
                project,
                purpose,
                mode: if exclusive {
                    LeaseMode::Exclusive
                } else {
                    LeaseMode::Shared
                },
                ttl_seconds: ttl,
                metadata: Some(serde_json::Value::Object(metadata)),
            })?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Resource(ResourceCmd::Heartbeat { agent, targets }) => {
            let targets = targets
                .iter()
                .map(|t| ResourceRef::parse(t))
                .collect::<Result<Vec<_>, _>>()?;
            let rs = store.heartbeat_resources(&agent.agent, &targets)?;
            emit(json, &rs, |r| output::resources(r))
        }
        Command::Borrow { target, agent, ttl } => {
            let out = store.borrow_resource(&agent.agent, &ResourceRef::parse(&target)?, ttl)?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Release {
            target,
            agent,
            stopped,
        } => {
            let out =
                store.release_resource(&agent.agent, &ResourceRef::parse(&target)?, stopped)?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Orphans { no_scan } => {
            let report = crate::reap::orphans(&store, !no_scan)?;
            emit(json, &report, output::orphans)
        }
        Command::Reap {
            target,
            grace,
            confirm,
            force,
        } => {
            let plan = crate::reap::plan(&store, &target, grace)?;
            if confirm {
                let done = crate::reap::execute(&store, plan, force)?;
                emit(json, &done, output::reap_plan)
            } else {
                emit(json, &plan, output::reap_plan)
            }
        }
        Command::Token(TokenCmd::Create {
            agent,
            projects,
            label,
            expires,
        }) => {
            let expires = expires.as_deref().map(parse_secs).transpose()?;
            let t = store.create_token(&agent, &projects, label, expires)?;
            emit(json, &t, |t| {
                format!(
                    "token {} for {} (projects: {})\n\n{}\n\nThis is the only time the token is shown. \
                     Give it to the remote peer as `Authorization: Bearer <token>`.",
                    t.token.token_id,
                    t.token.agent,
                    if t.token.project_globs.is_empty() {
                        "none".to_string()
                    } else {
                        t.token.project_globs.join(", ")
                    },
                    t.secret
                )
            })
        }
        Command::Token(TokenCmd::List) => {
            let ts = store.tokens()?;
            emit(json, &ts, |ts| output::tokens(ts))
        }
        Command::Token(TokenCmd::Revoke { token_id }) => {
            let t = store.revoke_token(&token_id)?;
            emit(json, &t, |t| {
                format!("revoked token {} ({})", t.token_id, t.agent)
            })
        }
        Command::Node(NodeCmd::Show) => {
            let n = store.node()?;
            emit(json, &n, |n| {
                format!(
                    "node {} ({})\nkey fingerprint {}",
                    n.display_name, n.node_id, n.fingerprint
                )
            })
        }
        Command::Node(NodeCmd::Name { name: Some(name) }) => {
            let n = store.set_node_name(&name)?;
            emit(json, &n, |n| format!("this node is now {}", n.display_name))
        }
        Command::Node(NodeCmd::Name { name: None }) => {
            let n = store.node_entry()?;
            emit(json, &n, |n| {
                format!(
                    "{}\n(rename with `another node name <name>`; details with `another node show`)",
                    n.display_name
                )
            })
        }
        Command::Pair(PairCmd::Invite {
            url,
            perms,
            expires,
        }) => {
            let perms = perms
                .iter()
                .map(|p| p.parse::<another_core::Perm>())
                .collect::<Result<Vec<_>, _>>()?;
            let code = store.create_invite(&url, &perms, parse_secs(&expires)?)?;
            let node = store.node()?;
            let out = serde_json::json!({ "code": code, "hub": node.display_name, "fingerprint": node.fingerprint });
            emit(json, &out, |_| {
                format!(
                    "invite for hub {} (key {}), single use:\n\n{code}\n\nOn the spoke: another pair join <code>",
                    node.display_name, node.fingerprint
                )
            })
        }
        Command::Pair(PairCmd::Join { code }) => {
            let peer = crate::fed::join(&store, &code)?;
            emit(json, &peer, |p| {
                format!(
                    "paired with hub {} at {} (key {}); run `another sync`",
                    p.display_name,
                    p.url.as_deref().unwrap_or("-"),
                    p.fingerprint
                )
            })
        }
        Command::Pair(PairCmd::Revoke { node }) => {
            let p = store.revoke_peer(&node)?;
            emit(json, &p, |p| format!("revoked {}", p.display_name))
        }
        Command::Peers => {
            let ps = store.peers()?;
            emit(json, &ps, |p| output::peers(p))
        }
        Command::Sync { watch, every } => loop {
            let report = crate::fed::sync(&store);
            match report {
                Ok(r) => {
                    emit(json, &r, output::sync_report);
                }
                Err(e) if watch => eprintln!("another: sync failed: {e:#}"),
                Err(e) => return Err(e),
            }
            if !watch {
                break ExitCode::SUCCESS;
            }
            std::thread::sleep(Duration::from_secs(every.max(5)));
        },
        Command::Wake {
            cmd: Some(WakeCmd::Set { agent, via, target }),
            ..
        } => {
            let t = store.set_wake_target(&agent.agent, via.parse()?, &target, false)?;
            emit(json, &t, |t| match t {
                Some(t) => format!("wake {} via {} at {}", t.agent, t.via.as_str(), t.target),
                None => String::new(),
            })
        }
        Command::Wake {
            cmd: Some(WakeCmd::Clear { agent }),
            ..
        } => {
            let cleared = store.clear_wake_target(&agent.agent)?;
            emit(json, &serde_json::json!({ "cleared": cleared }), |_| {
                if cleared {
                    "cleared".into()
                } else {
                    "no wake target was set".into()
                }
            })
        }
        Command::Wake {
            cmd: Some(WakeCmd::List),
            ..
        } => {
            let ts = store.wake_targets()?;
            emit(json, &ts, |ts| output::wake_targets(ts))
        }
        Command::Wake { cmd: None, run } => {
            let opts = crate::wake::WakeOptions {
                via: run.via.as_deref().map(str::parse).transpose()?,
                target: run.target,
                message: run.message,
                force: run.force,
                dry_run: run.dry_run,
            };
            let plans = if run.all {
                crate::wake::wake_all(&store, &opts)?
            } else {
                let agent = run.agent.ok_or_else(|| {
                    anyhow::anyhow!("give --agent (or set ANOTHER_AGENT) or --all")
                })?;
                vec![crate::wake::wake(&store, &agent, &opts)?]
            };
            emit(json, &plans, |p| output::wake_plans(p))
        }
        Command::Lane(cmd) => lane_command(&store, json, cmd)?,
        Command::Webhook(cmd) => webhook_command(&store, json, cmd)?,
        Command::Decision(cmd) => decision_command(&store, json, cmd)?,
        Command::Spend { cmd, report } => spend_command(&store, json, cmd, report)?,
        Command::Summary {
            cmd: Some(SummaryCmd::Schedule { daily, to, off }),
            ..
        } => {
            let s = if off {
                store.set_summary_schedule(None, None)?
            } else if daily.is_some() || to.is_some() {
                let at = match daily {
                    Some(d) => Some(d),
                    None => store.summary_schedule()?.daily_at,
                };
                if at.is_none() {
                    anyhow::bail!("give --daily HH:MM to turn the daily summary on");
                }
                store.set_summary_schedule(at.as_deref(), to.as_deref())?
            } else {
                store.summary_schedule()?
            };
            emit(json, &s, |s| {
                match &s.daily_at {
                Some(at) => format!(
                    "daily summary at {at} (local time) to {}; last sent {}\nSent while `another serve` runs.",
                    s.to,
                    s.last_sent_at.as_deref().unwrap_or("never")
                ),
                None => "daily summary: off (pull it with `another summary`)".to_string(),
            }
            })
        }
        Command::Summary { since, cmd: None } => {
            let since = another_core::Since::parse(&since, store.now())?;
            let s = store.daily_summary(Some(since), None)?;
            emit(json, &s, |s| s.markdown.trim_end().to_string())
        }
        Command::Config { key, value } => {
            let list = match (key, value) {
                (None, _) => store.settings()?,
                (Some(k), None) => vec![store.setting(&k)?],
                (Some(k), Some(v)) => vec![store.set_setting(&k, &v)?],
            };
            emit(json, &list, |l| {
                l.iter()
                    .map(|s| {
                        format!(
                            "{} = {}{}  # {}\n",
                            s.key,
                            s.value,
                            if s.default { " (default)" } else { "" },
                            s.help
                        )
                    })
                    .collect()
            })
        }
        Command::Audit { limit } => {
            let entries = store.audit_log(limit)?;
            emit(json, &entries, |e| output::audit(e))
        }
        Command::Activity {
            since,
            limit,
            kinds,
            summary,
        } => {
            let since = since.map(|s| Since::parse(&s, store.now())).transpose()?;
            if summary {
                let sum = store.activity_summary(since)?;
                emit(json, &sum, |s| s.text.clone())
            } else {
                let feed = store.activity(ActivityQuery {
                    since,
                    limit,
                    kinds: kinds
                        .iter()
                        .map(|k| k.parse::<ActivityKind>())
                        .collect::<Result<_, _>>()?,
                })?;
                emit(json, &feed, output::activity)
            }
        }
        Command::Sweep => {
            let r = store.sweep()?;
            emit(json, &r, |r| {
                format!(
                    "expired {} hold(s), {} request(s) went stale",
                    r.expired_holds, r.stale_requests
                )
            })
        }
        Command::Events { limit } => {
            let events = store.events(limit)?;
            emit(json, &events, |e| output::events(e))
        }
    };
    Ok(code)
}

fn hook_command(
    cmd: &HookCmd,
    json: bool,
    another_home: impl Fn() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<ExitCode> {
    match cmd {
        HookCmd::Run { event, client } => {
            // Fail open: whatever happens, print what we have and exit 0.
            let store = another_home().and_then(|h| Ok(Store::open(&h)?));
            let env = another_home().and_then(HookEnv::detect);
            let text = hooks::run_fail_open(store, env, *event, *client);
            if !text.is_empty() {
                println!("{text}");
            }
            Ok(ExitCode::SUCCESS)
        }
        HookCmd::Install {
            client,
            dry_run,
            lane_reports,
            pre_edit,
        } => {
            let env = HookEnv::detect(another_home()?)?;
            let opts = hooks::HookOptions {
                lane_reports: *lane_reports,
                pre_edit: *pre_edit,
            };
            let report = hooks::install(&env, &clients(client), true, *dry_run, opts)?;
            Ok(emit(json, &report, output::hook_report))
        }
        HookCmd::Uninstall { client, dry_run } => {
            let env = HookEnv::detect(another_home()?)?;
            let report = hooks::install(
                &env,
                &clients(client),
                false,
                *dry_run,
                hooks::HookOptions::default(),
            )?;
            Ok(emit(json, &report, output::hook_report))
        }
        HookCmd::Status => {
            let env = HookEnv::detect(another_home()?)?;
            let rows: Vec<output::HookStatus> = Client::ALL
                .iter()
                .map(|c| {
                    let path = c.config_path(&env.home);
                    output::HookStatus {
                        client: c.as_str().to_string(),
                        installed: std::fs::read_to_string(&path)
                            .is_ok_and(|t| t.contains(hooks::MARKER)),
                        config: path.display().to_string(),
                    }
                })
                .collect();
            Ok(emit(json, &rows, |r| output::hook_status(r)))
        }
    }
}

fn spend_command(
    store: &Store,
    json: bool,
    cmd: Option<SpendCmd>,
    report: SpendReportArgs,
) -> anyhow::Result<ExitCode> {
    use another_core::{BorrowSpend, NewSpendAccount, RecordSpend, SpendAccountUpdate, SpendQuery};
    Ok(match cmd {
        None => {
            let r = store.spend_report(
                SpendQuery {
                    period: report.period.parse()?,
                    account: report.account,
                    lane: report.lane,
                    agent: report.agent_filter,
                },
                None,
            )?;
            emit(json, &r, |r| r.markdown.trim_end().to_string())
        }
        Some(SpendCmd::Account(SpendAccountCmd::Add {
            name,
            kind,
            unit,
            budget,
            period,
            reset_day,
            cost_per_hour,
            requires_approval,
            approver,
            project,
            description,
        })) => {
            let project = match project {
                Some(p) => Some(project_key(store, "local", Some(p))?),
                None => None,
            };
            let a = store.create_spend_account(
                NewSpendAccount {
                    name,
                    kind: kind.parse()?,
                    unit,
                    budget,
                    period: period.parse()?,
                    reset_day,
                    cost_per_hour,
                    requires_approval,
                    approver,
                    project,
                    description,
                },
                None,
            )?;
            emit(json, &a, |a| {
                output::spend_accounts(std::slice::from_ref(a))
            })
        }
        Some(SpendCmd::Account(SpendAccountCmd::List)) => {
            let list = store.spend_accounts(None)?;
            emit(json, &list, |l| output::spend_accounts(l))
        }
        Some(SpendCmd::Account(SpendAccountCmd::Update {
            name,
            budget,
            clear_budget,
            reset_day,
            cost_per_hour,
            requires_approval,
            approver,
            description,
        })) => {
            let a = store.update_spend_account(
                &name,
                SpendAccountUpdate {
                    budget,
                    clear_budget,
                    reset_day,
                    cost_per_hour,
                    requires_approval,
                    approver,
                    description,
                },
                None,
            )?;
            emit(json, &a, |a| {
                output::spend_accounts(std::slice::from_ref(a))
            })
        }
        Some(SpendCmd::Account(SpendAccountCmd::Archive { name })) => {
            let a = store.archive_spend_account(&name, None)?;
            emit(json, &a, |a| format!("archived spend account {}", a.name))
        }
        Some(SpendCmd::Record {
            agent,
            account,
            amount,
            lane,
            project,
            note,
        }) => {
            let e = store.record_spend(
                RecordSpend {
                    agent: Some(agent.agent),
                    account,
                    amount,
                    lane,
                    project,
                    note,
                    at: None,
                },
                None,
            )?;
            emit(json, &e, |e| {
                format!(
                    "recorded {} {} on {}{} [{}]",
                    e.amount,
                    e.unit,
                    e.account,
                    e.lane
                        .as_ref()
                        .map(|l| format!(" for lane {l}"))
                        .unwrap_or_default(),
                    e.entry_id
                )
            })
        }
        Some(SpendCmd::Borrow {
            agent,
            account,
            lane,
            cost_per_hour,
            ttl,
            hours,
            note,
        }) => {
            let o = store.borrow_spend(
                BorrowSpend {
                    agent: agent.agent,
                    account,
                    lane,
                    cost_per_hour,
                    ttl_seconds: ttl,
                    hours,
                    note,
                },
                None,
            )?;
            emit(json, &o, |o| {
                match &o.decision_id {
                Some(d) => format!(
                    "{} needs approval: asked decision {d}; the lease {} starts when it is approved",
                    o.lease.account, o.lease.lease_id
                ),
                None => output::spend_leases(std::slice::from_ref(&o.lease)),
            }
            })
        }
        Some(SpendCmd::Release { lease_id, agent }) => {
            let l = store.release_spend(&agent.agent, &lease_id, None)?;
            emit(json, &l, |l| output::spend_leases(std::slice::from_ref(l)))
        }
        Some(SpendCmd::Leases { all }) => {
            let list = store.spend_leases(all, None)?;
            emit(json, &list, |l| output::spend_leases(l))
        }
        Some(SpendCmd::Link { account, resource }) => {
            let r =
                store.link_resource_spend(&ResourceRef::parse(&resource)?, Some(&account), None)?;
            emit(json, &r, |r| output::resources(std::slice::from_ref(r)))
        }
        Some(SpendCmd::Unlink { resource }) => {
            let r = store.link_resource_spend(&ResourceRef::parse(&resource)?, None, None)?;
            emit(json, &r, |r| output::resources(std::slice::from_ref(r)))
        }
    })
}

fn invite_command(store: &Store, json: bool, cmd: InviteCmd) -> anyhow::Result<ExitCode> {
    Ok(match cmd {
        InviteCmd::Create {
            agent,
            program,
            projects,
            expires,
            uses,
            lane,
            idle_hours,
            persistent,
            token_ttl,
            print_snippet,
            hub_url,
        } => {
            let created = store.create_agent_invite(another_core::NewAgentInvite {
                agent,
                program: program.parse()?,
                project_globs: projects,
                lane,
                expires_in_seconds: Some(parse_secs(&expires)?),
                max_uses: Some(uses),
                ephemeral: !persistent,
                idle_hours,
                token_ttl_seconds: token_ttl.as_deref().map(parse_secs).transpose()?,
            })?;
            let snippet = if print_snippet {
                Some(crate::join::snippet(
                    hub_url.as_deref().unwrap_or_default(),
                    &created.code,
                    &created.invite.expires_at,
                ))
            } else {
                None
            };
            let out = serde_json::json!({ "invite": created.invite, "code": created.code, "snippet": snippet });
            emit(json, &out, |o| {
                let mut s = output::invites(std::slice::from_ref(&created.invite));
                s.push_str(&format!(
                    "code (single use, shown once): {}\n",
                    o["code"].as_str().unwrap_or_default()
                ));
                match o["snippet"].as_str() {
                    Some(sn) => s.push_str(&format!("\n{sn}")),
                    None => s.push_str(
                        "join with: another join <hub-url> <code>   (or POST <hub>/join)\n",
                    ),
                }
                s
            })
        }
        InviteCmd::List => {
            let list = store.agent_invites()?;
            emit(json, &list, |l| output::invites(l))
        }
        InviteCmd::Revoke { invite_id } => {
            let i = store.revoke_agent_invite(&invite_id)?;
            emit(json, &i, |i| {
                format!("revoked invite {} ({})", i.invite_id, i.agent)
            })
        }
    })
}

fn remote_command(home: &Path, json: bool, cmd: &RemoteCmd) -> anyhow::Result<ExitCode> {
    use crate::join::{call, forget, load_profile};
    use serde_json::json as j;
    let p = load_profile(home)?;
    let out = match cmd {
        RemoteCmd::Status => {
            let hb = call(
                &p,
                "POST",
                "/agent/heartbeat",
                Some(j!({ "phase": "beat" })),
            )?;
            j!({ "hub": p.hub, "agent": p.agent, "lane": p.lane, "project_globs": p.project_globs, "heartbeat": hb })
        }
        RemoteCmd::Heartbeat { end } => call(
            &p,
            "POST",
            "/agent/heartbeat",
            Some(j!({ "phase": if *end { "end" } else { "beat" } })),
        )?,
        RemoteCmd::Lanes => call(&p, "GET", "/lanes", None)?,
        RemoteCmd::Report {
            lane,
            summary,
            status,
            progress,
        } => {
            let lane = lane
                .clone()
                .or(p.lane.clone())
                .ok_or_else(|| anyhow::anyhow!("no lane: pass --lane"))?;
            call(
                &p,
                "POST",
                &format!("/lanes/{lane}/report"),
                Some(j!({ "summary": summary, "status": status, "progress": progress })),
            )?
        }
        RemoteCmd::Decisions => call(&p, "GET", "/decisions", None)?,
        RemoteCmd::Answer {
            decision,
            value,
            text,
            answered_by,
        } => call(
            &p,
            "POST",
            &format!("/decisions/{decision}/answer"),
            Some(j!({ "value": value, "text": text, "answered_by": answered_by })),
        )?,
        RemoteCmd::Leave => {
            let v = call(&p, "POST", "/agent/leave", None)?;
            forget(home)?;
            v
        }
    };
    Ok(emit(json, &out, |v| output::remote(cmd_name(cmd), v)))
}

fn cmd_name(cmd: &RemoteCmd) -> &'static str {
    match cmd {
        RemoteCmd::Status => "status",
        RemoteCmd::Heartbeat { .. } => "heartbeat",
        RemoteCmd::Lanes => "lanes",
        RemoteCmd::Report { .. } => "report",
        RemoteCmd::Decisions => "decisions",
        RemoteCmd::Answer { .. } => "answer",
        RemoteCmd::Leave => "leave",
    }
}

fn decision_command(store: &Store, json: bool, cmd: DecisionCmd) -> anyhow::Result<ExitCode> {
    use another_core::{
        decision_markdown, parse_deadline, AnswerDecision, DecisionFilter, DecisionOption,
        DecisionStatus, DecisionTimeout, NewDecision,
    };
    Ok(match cmd {
        DecisionCmd::Ask {
            agent,
            question,
            options,
            free_text,
            links,
            deadline,
            on_timeout,
            default,
            to,
            project,
            lane,
        } => {
            let options = options
                .iter()
                .map(|o| {
                    let (value, label) = o.split_once('=').unwrap_or((o, o));
                    DecisionOption {
                        value: value.trim().to_string(),
                        label: label.trim().to_string(),
                        description: None,
                    }
                })
                .collect();
            // Project: explicit, else the lane's, else the agent's own, else the cwd.
            let project = match (project, &lane) {
                (Some(p), _) => Some(project_key(store, &agent.agent, Some(p))?),
                (None, Some(_)) => None,
                (None, None) => match store.agent(&agent.agent)?.project {
                    Some(_) => None,
                    None => Some(project_key(store, &agent.agent, None)?),
                },
            };
            let d = store.ask_decision(
                NewDecision {
                    agent: agent.agent,
                    question,
                    options,
                    allow_free_text: free_text,
                    context_links: links,
                    deadline_at: deadline
                        .as_deref()
                        .map(|d| parse_deadline(d, store.now()))
                        .transpose()?,
                    on_timeout: on_timeout
                        .as_deref()
                        .map(str::parse::<DecisionTimeout>)
                        .transpose()?
                        .unwrap_or_default(),
                    default_value: default,
                    human: to,
                    project,
                    lane,
                },
                None,
            )?;
            emit(json, &d, |d| {
                format!(
                    "asked {} (decision {})\n{}",
                    d.human,
                    d.decision_id,
                    decision_markdown(d)
                )
            })
        }
        DecisionCmd::List {
            status,
            all,
            project,
            lane,
            human,
        } => {
            let list = store.decisions(
                DecisionFilter {
                    status: status
                        .as_deref()
                        .map(str::parse::<DecisionStatus>)
                        .transpose()?,
                    all,
                    project,
                    lane,
                    human,
                    limit: 100,
                },
                None,
            )?;
            emit(json, &list, |l| output::decisions(l))
        }
        DecisionCmd::Show { decision, markdown } => {
            let d = store.decision(&decision, None)?;
            if markdown && !json {
                println!("{}", decision_markdown(&d).trim_end());
                ExitCode::SUCCESS
            } else {
                emit(json, &d, decision_markdown)
            }
        }
        DecisionCmd::Answer {
            decision,
            value,
            text,
            answered_by,
            relayed_by,
        } => {
            let d = store.answer_decision(
                AnswerDecision {
                    decision,
                    value,
                    text,
                    answered_by,
                    relayed_by,
                },
                None,
            )?;
            let woke = crate::wake::wake_if_targeted(store, &d.asker).is_some_and(|p| p.sent);
            emit(json, &d, |d| {
                let a = d.answer.as_ref();
                format!(
                    "answered decision {}: {}; mailed to {}{}",
                    d.decision_id,
                    a.and_then(|a| a.label.clone().or(a.value.clone()))
                        .unwrap_or_else(|| "free text".into()),
                    d.asker,
                    if woke { " (woken)" } else { "" }
                )
            })
        }
        DecisionCmd::Cancel {
            decision,
            agent,
            reason,
        } => {
            let d = store.cancel_decision(&agent.agent, &decision, reason, None)?;
            emit(json, &d, |d| format!("withdrew decision {}", d.decision_id))
        }
    })
}

fn webhook_command(store: &Store, json: bool, cmd: WebhookCmd) -> anyhow::Result<ExitCode> {
    use another_core::{DeliveryStatus, NewWebhook, WebhookFilter};
    Ok(match cmd {
        WebhookCmd::Add {
            url,
            events,
            projects,
            all_projects,
            description,
        } => {
            if projects.is_empty() && !all_projects {
                anyhow::bail!("give --project GLOB (repeatable) or --all-projects");
            }
            let filters = events
                .iter()
                .map(|e| e.parse::<WebhookFilter>())
                .collect::<another_core::Result<Vec<_>>>()?;
            let created = store.create_webhook(
                NewWebhook {
                    url,
                    filters,
                    project_globs: (!all_projects).then_some(projects),
                    description,
                },
                None,
            )?;
            emit(json, &created, output::webhook_created)
        }
        WebhookCmd::List => {
            let hooks = store.webhooks(None)?;
            emit(json, &hooks, |h| output::webhooks(h))
        }
        WebhookCmd::Remove { webhook_id } => {
            let h = store.remove_webhook(&webhook_id, None)?;
            emit(json, &h, |h| {
                format!("removed webhook {} ({})", h.webhook_id, h.url)
            })
        }
        WebhookCmd::Test { webhook_id } => {
            let d = store.queue_test_delivery(&webhook_id)?;
            let result = crate::push::deliver(store, &d)?;
            let ok = result.status == DeliveryStatus::Delivered;
            let code = emit(json, &result, |r| {
                output::deliveries(std::slice::from_ref(r))
            });
            if ok {
                code
            } else {
                ExitCode::FAILURE
            }
        }
        WebhookCmd::Dead { webhook, limit } => {
            let list =
                store.webhook_deliveries(webhook.as_deref(), Some(DeliveryStatus::Dead), limit)?;
            emit(json, &list, |l| output::deliveries(l))
        }
        WebhookCmd::Deliveries { webhook, limit } => {
            let list = store.webhook_deliveries(webhook.as_deref(), None, limit)?;
            emit(json, &list, |l| output::deliveries(l))
        }
        WebhookCmd::Retry { delivery_id } => {
            let d = store.retry_delivery(&delivery_id)?;
            emit(json, &d, |d| {
                format!(
                    "re-queued delivery {}; the next `another serve` delivery pass sends it",
                    d.delivery_id
                )
            })
        }
    })
}

fn lane_command(store: &Store, json: bool, cmd: LaneCmd) -> anyhow::Result<ExitCode> {
    use another_core::{
        lane_markdown, LaneFilter, LaneLinkKind, LaneReportInput, LaneStatus, LaneUpdate, NewLane,
        ReportKind,
    };
    let status = |s: Option<String>| -> anyhow::Result<Option<LaneStatus>> {
        Ok(s.map(|v| v.parse::<LaneStatus>()).transpose()?)
    };
    Ok(match cmd {
        LaneCmd::Create {
            name,
            agent,
            project,
            lead,
            charter,
            status: st,
        } => {
            let project = project_key(store, &agent.agent, project)?;
            let d = store.create_lane(
                NewLane {
                    agent: agent.agent,
                    name,
                    project,
                    lead,
                    charter: body_arg(charter)?,
                    status: status(st)?,
                },
                None,
            )?;
            emit(json, &d, output::lane_detail)
        }
        LaneCmd::List {
            status: st,
            project,
            lead,
            all,
        } => {
            let lanes = store.lanes(
                LaneFilter {
                    status: status(st)?,
                    project,
                    lead,
                    include_closed: all,
                },
                None,
            )?;
            emit(json, &lanes, |l| output::lanes(l))
        }
        LaneCmd::Show { lane, markdown } => {
            let d = store.lane(&lane, None)?;
            if markdown && !json {
                println!("{}", lane_markdown(&d).trim_end());
                ExitCode::SUCCESS
            } else {
                emit(json, &d, output::lane_detail)
            }
        }
        LaneCmd::Update {
            lane,
            agent,
            status: st,
            charter,
            lead,
            link_threads,
            link_requests,
            link_holds,
            link_messages,
        } => {
            let mut links = Vec::new();
            links.extend(link_threads.into_iter().map(|i| (LaneLinkKind::Thread, i)));
            links.extend(
                link_requests
                    .into_iter()
                    .map(|i| (LaneLinkKind::Request, i)),
            );
            links.extend(link_holds.into_iter().map(|i| (LaneLinkKind::Hold, i)));
            links.extend(
                link_messages
                    .into_iter()
                    .map(|i| (LaneLinkKind::Message, i)),
            );
            let d = store.update_lane(
                LaneUpdate {
                    agent: agent.agent,
                    lane,
                    status: status(st)?,
                    charter,
                    lead,
                    links,
                },
                None,
            )?;
            emit(json, &d, output::lane_detail)
        }
        LaneCmd::Report {
            lane,
            agent,
            summary,
            progress,
            status: st,
            links,
            artifacts,
        } => {
            let r = store.report_lane(
                LaneReportInput {
                    agent: agent.agent,
                    lane,
                    kind: ReportKind::Status,
                    summary: body_arg(summary)?,
                    progress,
                    status: status(st)?,
                    links,
                    artifacts,
                    outcome: None,
                    pr_url: None,
                },
                None,
            )?;
            emit(json, &r, output::lane_report)
        }
        LaneCmd::Done {
            lane,
            agent,
            outcome,
            pr,
            summary,
            artifacts,
        } => {
            let r = store.report_lane(
                LaneReportInput {
                    agent: agent.agent,
                    lane,
                    kind: ReportKind::Done,
                    summary: summary.unwrap_or_else(|| outcome.clone()),
                    progress: Some(100),
                    status: None,
                    links: vec![],
                    artifacts,
                    outcome: Some(outcome),
                    pr_url: pr,
                },
                None,
            )?;
            emit(json, &r, output::lane_report)
        }
        LaneCmd::Close {
            lane,
            agent,
            abandon,
            outcome,
        } => {
            let d = store.close_lane(&agent.agent, &lane, abandon, outcome, None)?;
            emit(json, &d, output::lane_detail)
        }
        LaneCmd::Export { lane, out, dir } => {
            let d = store.lane(&lane, None)?;
            let md = lane_markdown(&d);
            let mut written = Vec::new();
            match (out, dir) {
                (Some(path), _) => {
                    std::fs::write(&path, &md)?;
                    written.push(path.display().to_string());
                }
                (None, Some(dir)) => {
                    std::fs::create_dir_all(&dir)?;
                    let status = dir.join(format!("{}-STATUS.md", d.lane.name));
                    std::fs::write(&status, &md)?;
                    written.push(status.display().to_string());
                    if d.lane.status == LaneStatus::Done {
                        let done = dir.join(format!("DONE-{}.md", d.lane.name));
                        std::fs::write(&done, &md)?;
                        written.push(done.display().to_string());
                    }
                }
                (None, None) => {
                    print!("{md}");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            emit(json, &written, |w| format!("wrote {}", w.join(", ")))
        }
    })
}
