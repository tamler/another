//! Another daemon and CLI: MCP tools over the core store, served on stdio or loopback HTTP.

pub mod cli;
pub mod fed;
pub mod hooks;
mod http_decisions;
pub mod mcp;
mod mcp_decisions;
mod mcp_lanes;
mod mcp_push;
mod mcp_spend;
mod output;
pub mod push;
pub mod reap;
pub mod remote;
pub mod scan;
pub mod serve;
pub mod wake;
