//! Another daemon and CLI: MCP tools over the core store, served on stdio or loopback HTTP.

mod atomic_write;
pub mod cli;
pub mod daemon;
pub mod dashboard;
pub mod doctor;
pub mod fed;
pub mod hooks;
pub mod mcp;
pub mod needs;
pub mod oauth_http;
mod output;
pub mod publish;
pub mod queue_cli;
pub mod reap;
pub mod remote;
pub mod scan;
pub mod serve;
pub mod templates;
pub mod transport;
pub mod wake;
