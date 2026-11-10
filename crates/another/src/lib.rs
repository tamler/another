//! Another daemon and CLI: MCP tools over the core store, served on stdio or loopback HTTP.

pub mod cli;
pub mod fed;
pub mod hooks;
pub mod mcp;
pub mod oauth_http;
mod output;
pub mod publish;
pub mod reap;
pub mod remote;
pub mod scan;
pub mod serve;
pub mod wake;
