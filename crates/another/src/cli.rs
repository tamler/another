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
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde::Serialize;

use crate::hooks::{self, Client, HookEnv, HookEvent};
use crate::output;

#[derive(Debug, Parser)]
#[command(
    name = "another",
    version,
    about = "Local-first inbox, activity feed and resource registry for coding agents"
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
        /// Public origin for OAuth, e.g. https://another.example.com (no path; requires --auth).
        /// Hosted apps such as ChatGPT and Claude connect to `<origin>/mcp`.
        #[arg(long, requires = "auth")]
        public_url: Option<String>,
        /// Act as a federation hub: serve /fed/* for paired spokes (node-signed requests).
        #[arg(long)]
        federation: bool,
        /// Serve the local dashboard at /ui.
        /// On by default for a loopback server without --auth.
        /// With --auth it is read-only and omits message bodies. Refused on a non-loopback bind.
        #[arg(long, conflicts_with = "no_dashboard")]
        dashboard: bool,
        /// Do not serve the dashboard.
        #[arg(long)]
        no_dashboard: bool,
        /// Seconds between sweeps that expire holds and stale requests.
        #[arg(long, default_value_t = 30)]
        sweep_secs: u64,
        /// Do not wake idle Herdr agents when new mail arrives.
        /// (Push delivery runs only on a loopback server without --auth.)
        #[arg(long)]
        no_wake: bool,
        /// Seconds between checks for new mail to push to idle Herdr agents.
        #[arg(long, default_value_t = 2)]
        wake_secs: u64,
    },
    /// Keep `another serve` running in the background (macOS launchd LaunchAgent).
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Register projects or list them.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Register agents, list them, or show one.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Send a message.
    Send {
        /// Sender: name or name@project (default: $ANOTHER_AGENT). `--agent`/`-a` also work,
        /// as on every other command.
        #[arg(
            long,
            short = 'f',
            visible_alias = "agent",
            visible_short_alias = 'a',
            env = "ANOTHER_AGENT"
        )]
        from: String,
        /// Recipients (repeat or comma-separate).
        #[arg(long, short = 't', value_delimiter = ',', required = true)]
        to: Vec<String>,
        /// Copied recipients (repeat or comma-separate).
        #[arg(long, value_delimiter = ',')]
        cc: Vec<String>,
        /// One line of text: the subject shown in inboxes and notices.
        #[arg(long, short = 's')]
        subject: String,
        /// Message body; `-` reads stdin.
        #[arg(long, short = 'b', default_value = "")]
        body: String,
        /// low | normal | high | urgent
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
        /// The message to reply to (ids are shown by `another inbox` and `another thread`).
        message_id: String,
        /// Sender: name or name@project (default: $ANOTHER_AGENT). `--agent`/`-a` also work.
        #[arg(
            long,
            short = 'f',
            visible_alias = "agent",
            visible_short_alias = 'a',
            env = "ANOTHER_AGENT"
        )]
        from: String,
        /// Reply body; `-` reads stdin.
        #[arg(long, short = 'b')]
        body: String,
        /// Reply to all original recipients.
        #[arg(long)]
        all: bool,
        /// low | normal | high | urgent (default: the original's)
        #[arg(long, short = 'i')]
        importance: Option<String>,
        /// Ask recipients to acknowledge.
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
        /// Most messages to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Mark messages read and acknowledged.
    Ack {
        #[command(flatten)]
        agent: AgentArg,
        /// Messages to mark (ids from `another inbox`).
        #[arg(required = true)]
        message_ids: Vec<String>,
        /// Mark messages read (changes state) without acknowledging. --read-only is a compatibility alias.
        #[arg(long, visible_alias = "read-only")]
        mark_read_only: bool,
    },
    /// List threads an agent is part of.
    Threads {
        #[command(flatten)]
        agent: AgentArg,
        /// Most threads to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show every message in a thread.
    Thread {
        /// The thread (ids from `another threads` or `another inbox`).
        thread_id: String,
    },
    /// List requests.
    Requests {
        /// open | claimed | done | stale | cancelled
        #[arg(long)]
        status: Option<String>,
        /// Owner name or name@project.
        #[arg(long)]
        owner: Option<String>,
        /// Most requests to show.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Claim, release, complete or cancel a request.
    Request {
        /// claim | release | complete | cancel
        action: String,
        /// The request (ids from `another requests` or `another inbox`).
        request_id: String,
        #[command(flatten)]
        agent: AgentArg,
        /// On complete or cancel: a note (a completion note is mailed to the requester).
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
        /// Only this agent's holds (name or name@project).
        #[arg(long, short = 'a')]
        agent: Option<String>,
    },
    /// Session heartbeat: renew holds; with --end release them and go offline.
    Heartbeat {
        #[command(flatten)]
        agent: AgentArg,
        /// Session is starting: record a `session.started` event.
        #[arg(long, conflicts_with = "end")]
        start: bool,
        /// Session is ending: release the agent's holds and leases and mark it offline.
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
        /// The resource: its id or kind:name (from `another resources`).
        target: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Release your lease on a resource (id or kind:name).
    Release {
        /// The resource: its id or kind:name (from `another resources`).
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
        /// What to reap: a resource id, kind:name, or pid:N (from `another orphans`).
        target: String,
        /// Seconds the target must have been orphaned (or alive, for pid targets).
        #[arg(long, default_value_t = crate::reap::DEFAULT_GRACE_SECONDS)]
        grace: i64,
        /// Human confirmation: stop the process (Windows drops hard). Agents never pass this.
        #[arg(long)]
        confirm: bool,
        /// On Linux, with --confirm: SIGKILL if the process survives SIGTERM.
        #[arg(long, requires = "confirm")]
        force: bool,
    },
    /// Create, list or revoke bearer tokens for remote peers.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Approve, deny, list or revoke OAuth grants for hosted apps.
    #[command(subcommand)]
    Oauth(OauthCmd),
    /// Publish the authenticated server for ChatGPT, Claude and other hosted MCP clients.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Show the audit log of remote (token-authenticated) calls.
    Audit {
        /// Most entries to show (the newest).
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
        /// Seconds between syncs with --watch.
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
    /// Open the local dashboard in a browser (loopback only).
    ///
    /// Useful when agents talk to Another over stdio and `another serve` is not running.
    /// A loopback `another serve` without `--auth` already serves the same UI at `/ui`.
    Dashboard {
        /// HTTP bind address. Loopback only. Defaults beside `another serve` (7466)
        /// and the usual `--auth` example (7467).
        #[arg(long, default_value = "127.0.0.1:7468")]
        bind: String,
        /// Do not open a browser; print the URL instead.
        #[arg(long)]
        no_open: bool,
    },
    /// Show the activity feed (who is doing what), or a summary of it.
    Activity {
        /// Start point: a duration (30m, 2h, 1d), an RFC 3339 time, or an event id cursor.
        #[arg(long)]
        since: Option<String>,
        /// Most items to show.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Only these categories: mail, request, hold, resource, session, system, federation.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Print a summary (counts, active agents, highlights) instead of items.
        #[arg(long)]
        summary: bool,
    },
    /// Count coordination evidence: edits stopped by a hold, holds the edit hook took, mail
    /// pushed into running sessions and whether it was acted on, requests claimed and done.
    Stats {
        /// Start point: a duration (30m, 48h, 2d), an RFC 3339 time, or an event id.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Read-only work board, per project: who holds which paths and until when, which paths
    /// the edit hook claimed for each agent recently, edits it stopped, and what is waiting
    /// on whom (unread mail, acknowledgements, pending requests). Writes nothing.
    Board {
        /// Only this project (slug or path) and sibling checkouts of the same repository.
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Window for recent and stopped edits: a duration (30m, 24h, 2d) or an RFC 3339 time.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Items per list (at most 200).
        #[arg(long, default_value_t = another_core::DEFAULT_BOARD_LIMIT)]
        limit: u32,
    },
    /// Install, remove or run client hook packs.
    #[command(subcommand)]
    Hook(HookCmd),
    /// Expire lapsed holds and mark overdue requests stale now.
    Sweep,
    /// Show the most recent event-log records.
    Events {
        /// Most records to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Print a shell completion script, e.g. `another completions zsh > ~/.zfunc/_another`.
    Completions {
        /// bash | zsh | fish | powershell | elvish
        shell: clap_complete::Shell,
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
        /// Where to wake it: a tmux pane (%3), a cmux surface id, or a Herdr pane id.
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
    /// Drop automatically captured targets whose pane is gone or whose agent has left
    /// (session ended, or not seen for a day). Targets set by hand are kept.
    Prune {
        /// Show what would be dropped without dropping it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// launchd label (change it only to run a second, separate daemon).
    #[arg(long, default_value = crate::daemon::DEFAULT_LABEL)]
    label: String,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    /// Write the LaunchAgent and load it: `another serve` on loopback, restarted if it exits.
    Install {
        #[command(flatten)]
        args: DaemonArgs,
        /// Loopback address to serve on.
        #[arg(long, default_value = crate::serve::DEFAULT_BIND)]
        bind: String,
        /// Write the plist but do not load it.
        #[arg(long)]
        no_load: bool,
        /// Show what would change without writing or loading anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Unload the LaunchAgent and remove its plist.
    Uninstall {
        #[command(flatten)]
        args: DaemonArgs,
        /// Show what would be removed without unloading or removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show whether the daemon is installed, loaded, running and answering.
    Status {
        #[command(flatten)]
        args: DaemonArgs,
    },
    /// Restart the daemon (`launchctl kickstart -k`).
    Restart {
        #[command(flatten)]
        args: DaemonArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCmd {
    /// Show this node's id, name and key fingerprint.
    Show,
    /// Rename this node (do this before pairing; names must be unique across nodes).
    /// Without a name, prints the current one.
    Name {
        /// The new name (omit it to print the current one).
        name: Option<String>,
    },
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
    Join {
        /// The invite code printed by the hub's `another pair invite`.
        code: String,
    },
    /// Stop accepting (or using) a paired node, by name or id.
    Revoke {
        /// The paired node's name or id (see `another peers`).
        node: String,
    },
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
    List,
    /// Revoke a token by id (takes effect on the next request).
    Revoke {
        /// The token's id (see `another token list`).
        token_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum OauthCmd {
    /// Show clients waiting for a local approval.
    Pending,
    /// Approve one waiting client and bind it to a single agent. This is the only approval path.
    Approve {
        /// Code shown on the consent page and by `another oauth pending` (for example abcd-efgh).
        user_code: String,
        /// The agent this grant acts as. Register it first.
        #[arg(long, short = 'a')]
        agent: String,
        /// Project path or slug glob this grant may touch (repeatable). Omit for mailbox and feed only.
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        /// A note to recognise the grant by.
        #[arg(long)]
        label: Option<String>,
    },
    /// Reject a waiting client.
    Deny {
        /// Code shown on the consent page and by `another oauth pending`.
        user_code: String,
    },
    /// List grants (each one is a single agent).
    List,
    /// Revoke a grant and every token issued for it.
    Revoke {
        /// The grant's id (see `another oauth list`).
        grant_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCmd {
    /// Run an authenticated server and print the URL to paste into ChatGPT or Claude.
    Up {
        /// Loopback address the tunnel forwards to.
        #[arg(long, default_value = "127.0.0.1:7467")]
        bind: String,
        /// Origin the tunnel already serves, such as https://another.example.com.
        /// Without this, `cloudflared` is started and its quick-tunnel URL is used.
        #[arg(long)]
        public_url: Option<String>,
        /// Seconds between sweeps of lapsed holds and overdue requests.
        #[arg(long, default_value_t = 30)]
        sweep_secs: u64,
    },
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
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("invalid duration {raw:?}: too large"))
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
        /// Where to reach it, e.g. http://127.0.0.1:11434.
        #[arg(long)]
        endpoint: Option<String>,
        /// The port it listens on.
        #[arg(long)]
        port: Option<u16>,
        /// The process id serving it (used by `another orphans` and `reap`).
        #[arg(long)]
        pid: Option<u32>,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// What it is for, in a few words.
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
        /// Resource ids or kind:name to renew (default: all of your leases).
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
    },
    /// Remove Another hooks from client configs, leaving other hooks untouched.
    Uninstall {
        /// Clients to remove the hooks from (repeat or comma-separate).
        #[arg(long, value_delimiter = ',', required = true)]
        client: Vec<ClientSel>,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show hook configuration presence; Codex trust and execution are not checked.
    Status,
    /// Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0.
    Run {
        /// The hook event the client fired.
        event: HookEvent,
        /// The client that fired it.
        #[arg(long)]
        client: Client,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// Register a project by path (default: current directory).
    Ensure {
        /// The project directory (default: the current directory).
        path: Option<PathBuf>,
    },
    List,
}

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Register or update an agent.
    Register {
        /// The agent's name (with --project, its address is name@<project>).
        name: String,
        /// claude | grok | codex | cursor | grok_bot | other
        #[arg(long)]
        program: String,
        /// Project path to scope the name to (enables name@project).
        #[arg(long)]
        project: Option<PathBuf>,
        /// The model the agent runs on, for display.
        #[arg(long)]
        model: Option<String>,
        /// What the agent does, for display.
        #[arg(long)]
        description: Option<String>,
    },
    List,
    Show {
        /// The agent: name or name@project.
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct ProjectArg {
    /// Project path or slug (default: current directory).
    #[arg(long, short = 'p')]
    pub project: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum HoldCmd {
    /// Ask a local hold's owner to release it; reuse an open ask and close it when the hold ends.
    RequestRelease {
        #[command(flatten)]
        agent: AgentArg,
        /// Hold id from `another holds` or a conflict report.
        hold_id: String,
    },
    /// Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted).
    Acquire {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        /// Files, directories or globs, relative to the project.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
        /// Shared instead of exclusive.
        #[arg(long)]
        shared: bool,
        /// Why you hold them, shown to anyone they block.
        #[arg(long)]
        reason: Option<String>,
        /// Allow a pattern that covers the whole project (`**`, `.`, `**/*.rs`).
        /// Refused without this flag because it blocks every other agent in the repository.
        #[arg(long)]
        repo_wide: bool,
    },
    /// Report conflicts for paths without acquiring (exit code 3 on conflict).
    Check {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        /// Files, directories or globs, relative to the project.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Check as a shared request: only exclusive holds conflict.
        #[arg(long)]
        shared: bool,
    },
    /// Renew your holds (all, or the given ids).
    Renew {
        #[command(flatten)]
        agent: AgentArg,
        /// Hold ids to renew (default: all of yours).
        hold_ids: Vec<String>,
    },
    /// Release your holds (all, or the given ids).
    Release {
        #[command(flatten)]
        agent: AgentArg,
        /// Hold ids to release (default: all of yours).
        hold_ids: Vec<String>,
    },
}

/// Exit status for a conflict: holds that overlap, or any `conflict` error (acting on a
/// closed request, claiming or completing a request another agent has claimed). Hooks and
/// scripts branch on it. Other errors exit 1, including `forbidden` (for example releasing
/// a request someone else owns), and bad arguments exit 2 (clap).
pub const EXIT_CONFLICT: u8 = 3;

/// Whether `--json` was passed as an option: before any `--`, after which every argument
/// is a plain value (review R1-3).
fn json_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|a| a != "--")
        .any(|a| a == "--json")
}

/// A bad command line under `--json`, as a JSON error on stdout (exit 2, as clap), so a
/// script reading stdout always gets an object. Kept out of `main` (and never inlined) so
/// its temporaries do not grow the frame every command starts in: a debug build's main
/// thread on Windows has only 1 MB of stack (review R1-1).
#[inline(never)]
fn usage_error_json(e: &clap::Error) -> ExitCode {
    // clap's error paragraph (up to its blank line, before "Usage:"), on one line.
    let text = e.to_string();
    let message = text
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_start_matches("error: ")
        .to_string();
    println!(
        "{}",
        serde_json::json!({ "error": {
            "code": "usage",
            "message": message,
            "hint": "Run the command with --help for its arguments.",
        }})
    );
    ExitCode::from(2)
}

/// `another completions <shell>`, outside `run` for the same stack reason.
#[inline(never)]
fn completions_command(shell: clap_complete::Shell) -> anyhow::Result<ExitCode> {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "another", &mut script);
    // A closed pipe (`| head`) is not an error.
    match std::io::Write::write_all(&mut std::io::stdout(), &script) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// Stack for the thread the binary runs on.
const CLI_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Run `f` (the whole binary: plugin dispatch, then the CLI) on a thread with a known stack.
/// clap's derive builds the whole command tree in a few very large generated functions; in
/// a debug build their frames use most of a 1 MB stack, which is the main-thread default on
/// Windows, so `another init` and `another --version` overflowed there once every argument
/// had help text (review R1-1, reproduced on macOS with `ulimit -s 1024`). Plugin dispatch
/// builds the same tree to check built-in names, so it runs on this thread too. If the
/// thread cannot be started, `f` runs on the calling thread.
pub fn with_cli_stack(f: fn() -> ExitCode) -> ExitCode {
    match std::thread::Builder::new()
        .name("another".into())
        .stack_size(CLI_STACK_BYTES)
        .spawn(f)
    {
        // A panic has already printed its message; exit as an uncaught panic would.
        Ok(handle) => handle.join().unwrap_or(ExitCode::from(101)),
        Err(_) => f(),
    }
}

/// The CLI: parse the command line and run it. The binary calls it through
/// [`with_cli_stack`].
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // --help and --version still print as usual.
        Err(e) if e.use_stderr() && json_requested() => return usage_error_json(&e),
        Err(e) => e.exit(),
    };
    init_tracing(matches!(
        cli.command,
        Command::Serve { .. } | Command::Dashboard { .. }
    ));
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
            let hint = cli_hint(body.code, &body.message);
            if json {
                let mut out = serde_json::json!({ "error": body });
                if let Some(hint) = hint {
                    out["error"]["hint"] = serde_json::json!(hint);
                }
                println!("{out}");
            } else {
                eprintln!("another: {}", body.message);
                if let Some(hint) = hint {
                    eprintln!("hint: {hint}");
                }
            }
            if body.code == "conflict" {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// What to do next, for errors whose message only says what happened. The message comes
/// from the core and is shared with MCP, so the CLI's own commands are named here. An
/// error whose message already says what to do (a bad duration, an ambiguous name that
/// lists the choices) gets none.
fn cli_hint(code: &str, message: &str) -> Option<&'static str> {
    if code == "not_found" {
        let hint = if message.starts_with("unknown agent") {
            "List agents with `another agent list`; register one with `another agent register NAME --program PROGRAM --project DIR`."
        } else if message.starts_with("unknown request") {
            "List requests with `another requests` (`--status`, `--owner` to filter)."
        } else if message.starts_with("unknown message") || message.contains("was not delivered to")
        {
            "Message ids are shown by `another inbox --agent NAME` and `another thread THREAD_ID`."
        } else if message.starts_with("unknown thread") {
            "List threads with `another threads --agent NAME`."
        } else if message.starts_with("no live hold") {
            "List live holds with `another holds`; this one may have expired or been released."
        } else if message.starts_with("no registered resource")
            || message.starts_with("unknown resource")
        {
            "List resources with `another resources`."
        } else if message.starts_with("unknown token") {
            "List tokens with `another token list`."
        } else {
            return None;
        };
        return Some(hint);
    }
    if code == "db" && message.contains("database is locked") {
        return Some("Another process is writing to the database; run the command again.");
    }
    None
}

fn init_tracing(serving: bool) {
    let default = if serving { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("ANOTHER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // Colour only on a terminal: the daemon's log is a file under launchd.
    use std::io::IsTerminal;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
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

/// A project directory as the filesystem resolves it, the way the edit hook resolves files,
/// so a symlinked spelling names the same project instead of registering a second one. A
/// project already registered under the spelling as given, and not under the resolved one
/// (rows from before paths were resolved), keeps its row.
fn project_dir(store: &Store, path: &Path) -> anyhow::Result<String> {
    let given = abs(path)?;
    let real = hooks::resolve_existing(Path::new(&given))
        .to_string_lossy()
        .into_owned();
    if real != given && store.project(&real).is_err() && store.project(&given).is_ok() {
        return Ok(given);
    }
    Ok(real)
}

/// `another board`. Opens the database the way hooks do: nothing is created, migrated or
/// swept, so reading the board leaves the database as it was.
fn board_command(
    home: &Path,
    json: bool,
    project: Option<&str>,
    since: &str,
    limit: u32,
) -> anyhow::Result<ExitCode> {
    if !home.join(another_core::DB_FILE).exists() {
        anyhow::bail!(
            "no Another database in {}; run `another init` first",
            home.display()
        );
    }
    let store = Store::open_quick(home, Duration::from_secs(5))?;
    let since_ms = match Since::parse(since, store.now())? {
        Since::Time(t) => t,
        Since::After(_) => anyhow::bail!(
            "board --since takes a duration like 24h or an RFC 3339 time, not an event id"
        ),
    };
    let looks_like_path = |p: &str| p.contains('/') || p.contains('\\') || p == "." || p == "..";
    let project = match project {
        Some(p) if looks_like_path(p) => Some(abs(Path::new(p))?),
        Some(p) => Some(p.to_string()),
        None => None,
    };
    let board = store.board(another_core::BoardQuery {
        project,
        since_ms: Some(since_ms),
        limit,
        confined: false,
    })?;
    Ok(emit(json, &board, output::board))
}

/// Resolve `--project`: a path (ensured on the fly) or an existing slug; default cwd.
fn project_key(store: &Store, actor: &str, project: Option<String>) -> anyhow::Result<String> {
    let looks_like_path = |p: &str| p.contains('/') || p.contains('\\') || p == "." || p == "..";
    match project {
        Some(p) if !looks_like_path(&p) => Ok(p),
        Some(p) => Ok(store
            .ensure_project(actor, &project_dir(store, Path::new(&p))?)?
            .abs_path),
        None => Ok(store
            .ensure_project(actor, &project_dir(store, &std::env::current_dir()?)?)?
            .abs_path),
    }
}

/// Hold paths resolved through symlinks to the project-relative path they reach, so a hold
/// taken through an alias covers the real files, as the edit guard sees them. For a glob,
/// its literal directory part is resolved and the pattern kept (`alias/**` with
/// `alias -> src` becomes `src/**`). Paths that resolve outside the project are passed on
/// as given.
fn resolve_hold_paths(store: &Store, project: &str, paths: Vec<String>) -> Vec<String> {
    let Ok(p) = store.project(project) else {
        return paths;
    };
    let root = Path::new(&p.abs_path);
    let real_root = hooks::resolve_existing(root);
    // Project-relative form of a literal path ("" for the root), or None if outside.
    let relative = |literal: &str| -> Option<String> {
        let full = if Path::new(literal).is_absolute() {
            PathBuf::from(literal)
        } else {
            root.join(literal)
        };
        let real = hooks::resolve_existing(&full);
        real.strip_prefix(&real_root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned())
    };
    paths
        .into_iter()
        .map(|raw| match raw.find(['*', '?', '[', '{']) {
            Some(meta) => {
                // The literal directories before the first wildcard, if any.
                let Some(slash) = raw[..meta].rfind('/') else {
                    return raw;
                };
                let (dir, pattern) = (&raw[..slash], &raw[slash + 1..]);
                match relative(dir) {
                    Some(rel) if rel.is_empty() => pattern.to_string(),
                    Some(rel) => format!("{rel}/{pattern}"),
                    None => raw,
                }
            }
            None => match relative(&raw) {
                Some(rel) if !rel.is_empty() => rel,
                _ => raw,
            },
        })
        .collect()
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

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let json = cli.json;
    let home = cli.home.as_deref();
    if let Command::Serve {
        stdio,
        bind,
        auth,
        allow_remote_bind,
        allowed_hosts,
        public_url,
        federation,
        dashboard,
        no_dashboard,
        sweep_secs,
        no_wake,
        wake_secs,
    } = &cli.command
    {
        let store = open(home)?;
        let every = Duration::from_secs((*sweep_secs).max(1));
        let oauth = match public_url {
            Some(url) => Some(crate::oauth_http::parse_public_url(url)?),
            None => None,
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let dashboard_flag = match (*dashboard, *no_dashboard) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        };
        if *stdio && *dashboard {
            eprintln!("another: --dashboard needs the HTTP server; stdio has no dashboard");
        }
        let result = rt.block_on(async {
            if *stdio {
                crate::serve::serve_stdio(store, every).await
            } else {
                let opts = crate::serve::HttpOptions {
                    auth: *auth,
                    allow_remote_bind: *allow_remote_bind,
                    allowed_hosts: allowed_hosts.clone(),
                    oauth,
                    // `serve_http` replaces this with the port it actually bound.
                    bound_port: 0,
                    federation: *federation,
                    dashboard_flag,
                    dashboard: crate::dashboard::DashboardMode::Off,
                    dashboard_bound: None,
                    // Push belongs to the local daemon, not to a server published for
                    // remote peers (`--auth`), so two servers never wake the same agent.
                    push_every: (!*auth && !*no_wake)
                        .then(|| Duration::from_secs((*wake_secs).max(1))),
                };
                crate::serve::serve_http(store, bind, opts, every).await
            }
        });
        // Background passes are told to stop on shutdown; never wait on one for long.
        rt.shutdown_timeout(Duration::from_secs(3));
        result?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Command::Remote(RemoteCmd::Up {
        bind,
        public_url,
        sweep_secs,
    }) = &cli.command
    {
        let store = open(home)?;
        let every = Duration::from_secs((*sweep_secs).max(1));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(crate::publish::remote_up(
            store,
            bind,
            public_url.as_deref(),
            every,
        ))?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Command::Dashboard { bind, no_open } = &cli.command {
        let store = open(home)?;
        let bind = bind.clone();
        let open_browser = !*no_open && !json;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(crate::dashboard::serve_standalone(
            store,
            &bind,
            open_browser,
            json,
        ))?;
        return Ok(ExitCode::SUCCESS);
    }

    let another_home = || -> anyhow::Result<PathBuf> {
        Ok(match home {
            Some(h) => h.to_path_buf(),
            None => another_core::default_home()?,
        })
    };
    if let Command::Hook(cmd) = &cli.command {
        return hook_command(cmd, json, another_home);
    }
    if let Command::Daemon(cmd) = &cli.command {
        return daemon_command(cmd, json, another_home()?);
    }
    if let Command::Board {
        project,
        since,
        limit,
    } = &cli.command
    {
        return board_command(&another_home()?, json, project.as_deref(), since, *limit);
    }
    // Needs no database: generated from the command definitions.
    if let Command::Completions { shell } = &cli.command {
        return completions_command(*shell);
    }

    let store = open(home)?;
    let code = match cli.command {
        Command::Serve { .. }
        | Command::Dashboard { .. }
        | Command::Hook(_)
        | Command::Daemon(_)
        | Command::Board { .. }
        | Command::Completions { .. }
        | Command::Remote(_) => {
            unreachable!("handled above")
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
        Command::Project(ProjectCmd::Ensure { path }) => {
            let path = match path {
                Some(p) => project_dir(&store, &p)?,
                None => project_dir(&store, &std::env::current_dir()?)?,
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
            let a = store.register_agent(RegisterAgent {
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
            mark_read_only,
        } => {
            let out = store.acknowledge(&agent.agent, &message_ids, !mark_read_only)?;
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
            repo_wide,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let paths = resolve_hold_paths(&store, &project, paths);
            let report = store.acquire_holds(AcquireHolds {
                agent: agent.agent,
                project,
                paths,
                ttl_seconds: ttl,
                exclusive: !shared,
                reason,
                repo_wide,
            })?;
            let conflicted = !report.conflicts.is_empty();
            let emitted = emit(json, &report, output::hold_report);
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                emitted
            }
        }
        Command::Hold(HoldCmd::Check {
            agent,
            project,
            paths,
            shared,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let paths = resolve_hold_paths(&store, &project, paths);
            let conflicts = store.check_holds(&agent.agent, &project, &paths, !shared)?;
            let conflicted = !conflicts.is_empty();
            let emitted = emit(json, &conflicts, |c| {
                if c.is_empty() {
                    "no conflicts".to_string()
                } else {
                    output::conflicts(c)
                }
            });
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                emitted
            }
        }
        Command::Hold(HoldCmd::RequestRelease { agent, hold_id }) => {
            let message = store.request_hold_release(&agent.agent, &hold_id)?;
            emit(json, &message, output::sent)
        }
        Command::Hold(HoldCmd::Renew { agent, hold_ids }) => {
            let holds = store.renew_holds(&agent.agent, &hold_ids)?;
            emit(json, &holds, |h| output::holds(h))
        }
        Command::Hold(HoldCmd::Release { agent, hold_ids }) => {
            let ids = store.release_holds(&agent.agent, &hold_ids)?;
            emit(json, &ids, |ids| format!("released {} hold(s)", ids.len()))
        }
        Command::Holds { project, agent } => {
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
        Command::Oauth(OauthCmd::Pending) => {
            let rows = store.oauth_pending()?;
            emit(json, &rows, |rows| {
                if rows.is_empty() {
                    "no clients are waiting. Connect the app, then approve the code it shows."
                        .into()
                } else {
                    rows.iter()
                        .map(|r| {
                            format!(
                                "{}  {}  redirect host {}  expires {}",
                                r.user_code, r.client_name, r.redirect_host, r.expires_at
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        Command::Oauth(OauthCmd::Approve {
            user_code,
            agent,
            projects,
            label,
        }) => {
            let grant = store.approve_oauth(another_core::Approval {
                user_code,
                agent,
                project_globs: projects,
                label,
            })?;
            emit(json, &grant, |g| {
                format!(
                    "approved {} (redirect host from the request) as {}\nprojects: {}\ngrant {}",
                    g.client_name,
                    g.agent,
                    if g.project_globs.is_empty() {
                        "none: mailbox, heartbeat and the activity feed only".into()
                    } else {
                        g.project_globs.join(", ")
                    },
                    g.grant_id
                )
            })
        }
        Command::Oauth(OauthCmd::Deny { user_code }) => {
            let row = store.deny_oauth(&user_code)?;
            emit(json, &row, |r| {
                format!("denied {} ({})", r.user_code, r.client_name)
            })
        }
        Command::Oauth(OauthCmd::List) => {
            let grants = store.oauth_grants()?;
            emit(json, &grants, |grants| {
                if grants.is_empty() {
                    "no OAuth grants".into()
                } else {
                    grants
                        .iter()
                        .map(|g| {
                            let state = if g.revoked_at.is_some() {
                                "revoked"
                            } else {
                                "active"
                            };
                            format!(
                                "{}  {}  {}  {}  projects {}",
                                g.grant_id,
                                state,
                                g.client_name,
                                g.agent,
                                if g.project_globs.is_empty() {
                                    "none".into()
                                } else {
                                    g.project_globs.join(",")
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        Command::Oauth(OauthCmd::Revoke { grant_id }) => {
            let g = store.revoke_oauth_grant(&grant_id)?;
            emit(json, &g, |g| {
                format!(
                    "revoked grant {} ({}, {})",
                    g.grant_id, g.client_name, g.agent
                )
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
                    let emitted = emit(json, &r, output::sync_report);
                    if !watch {
                        break emitted;
                    }
                }
                Err(e) if watch => eprintln!("another: sync failed: {e:#}"),
                Err(e) => return Err(e),
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
        Command::Wake {
            cmd: Some(WakeCmd::Prune { dry_run }),
            ..
        } => {
            let rows = crate::wake::prune(&store, dry_run)?;
            emit(json, &rows, |rows| output::prune_rows(rows, dry_run))
        }
        Command::Wake { cmd: None, run } => {
            let opts = crate::wake::WakeOptions {
                via: run.via.as_deref().map(str::parse).transpose()?,
                target: run.target,
                message: run.message,
                force: run.force,
                dry_run: run.dry_run,
                cancel: None,
            };
            let plans = if run.all {
                crate::wake::wake_all(&store, &opts)?
            } else {
                let agent = run.agent.ok_or_else(|| {
                    anyhow::anyhow!("give --agent (or set ANOTHER_AGENT) or --all")
                })?;
                vec![crate::wake::wake(&store, &agent, &opts)?]
            };
            let code = emit(json, &plans, |p| output::wake_plans(p));
            // `--all` reports a failed wake per agent and carries on; the exit status
            // still says that something failed.
            let failed: Vec<_> = plans.iter().filter(|p| p.error.is_some()).collect();
            if failed.is_empty() {
                code
            } else {
                for p in failed {
                    eprintln!(
                        "another: wake failed for {}: {}",
                        p.agent,
                        p.error.as_deref().unwrap_or_default()
                    );
                }
                ExitCode::FAILURE
            }
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
        Command::Stats { since } => {
            let since = Since::parse(&since, store.now())?;
            let stats = store.trial_stats(Some(since))?;
            emit(json, &stats, output::trial_stats)
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

fn daemon_command(cmd: &DaemonCmd, json: bool, another_home: PathBuf) -> anyhow::Result<ExitCode> {
    use crate::daemon::{self, DaemonEnv};
    let env = |args: &DaemonArgs, bind: &str| {
        DaemonEnv::detect(another_home.clone(), args.label.clone(), bind.to_string())
    };
    match cmd {
        DaemonCmd::Install {
            args,
            bind,
            no_load,
            dry_run,
        } => {
            let report = daemon::install(&env(args, bind)?, !*no_load, *dry_run)?;
            Ok(emit(json, &report, output::daemon_report))
        }
        DaemonCmd::Uninstall { args, dry_run } => {
            let report = daemon::uninstall(&env(args, crate::serve::DEFAULT_BIND)?, *dry_run)?;
            Ok(emit(json, &report, output::daemon_report))
        }
        DaemonCmd::Status { args } => {
            let status = daemon::status(&env(args, crate::serve::DEFAULT_BIND)?)?;
            Ok(emit(json, &status, output::daemon_status))
        }
        DaemonCmd::Restart { args } => {
            daemon::restart(&env(args, crate::serve::DEFAULT_BIND)?)?;
            Ok(emit(
                json,
                &serde_json::json!({ "restarted": args.label }),
                |_| format!("restarted {}", args.label),
            ))
        }
    }
}

fn hook_command(
    cmd: &HookCmd,
    json: bool,
    another_home: impl Fn() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<ExitCode> {
    match cmd {
        HookCmd::Run { event, client } => {
            // Fail open: whatever happens, print what we have and exit 0.
            let home = another_home().ok();
            // Tool hooks run on every tool call: open without migrating, with a short lock
            // wait, so a busy or missing database costs milliseconds, not seconds.
            let store = match &home {
                Some(h) if event.is_tool_event() => {
                    Store::open_quick(h, event.busy_timeout()).map_err(anyhow::Error::from)
                }
                Some(h) => Store::open(h).map_err(anyhow::Error::from),
                None => Err(anyhow::anyhow!("no Another home directory")),
            };
            let env = another_home().and_then(HookEnv::detect);
            let text = hooks::run_fail_open(store, env, home.as_deref(), *event, *client);
            if !text.is_empty() {
                println!("{text}");
            }
            Ok(ExitCode::SUCCESS)
        }
        HookCmd::Install { client, dry_run } | HookCmd::Uninstall { client, dry_run } => {
            let install = matches!(cmd, HookCmd::Install { .. });
            let env = HookEnv::detect(another_home()?)?;
            let report = hooks::install(&env, &clients(client), install, *dry_run)?;
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
                        trust_note: (*c == Client::Codex).then_some(hooks::CODEX_TRUST_NOTE),
                    }
                })
                .collect();
            Ok(emit(json, &rows, |r| output::hook_status(r)))
        }
    }
}
