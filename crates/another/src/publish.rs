//! `another remote up`: run the authenticated server and print the URL a hosted app pastes.
//!
//! A named Cloudflare tunnel (or any other tunnel) is the durable path. With no
//! `--public-url`, this starts `cloudflared tunnel --url` and uses the hostname it prints.
//! That hostname is different every run. No domain is built into the binary.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use another_core::Store;
use anyhow::{bail, Context, Result};

use crate::oauth_http::parse_public_url;
use crate::serve::{serve_http, HttpOptions};

const NAMED_TUNNEL: &str = "\
Named Cloudflare Tunnel (pick your own hostname; none is built in):

  cloudflared tunnel create another
  cloudflared tunnel route dns another another.example.com

~/.cloudflared/config.yml:

  tunnel: <tunnel-id>
  credentials-file: /path/to/<tunnel-id>.json
  ingress:
    - hostname: another.example.com
      service: http://127.0.0.1:7467
    - service: http_status:404

  cloudflared tunnel run another
  another remote up --public-url https://another.example.com

Then paste https://another.example.com/mcp into ChatGPT or Claude.
Approve the client on this machine:

  another oauth approve <code> --agent NAME --project '/path/*'";

/// Run `another serve --auth` for hosted MCP clients and print the URL to paste.
pub async fn remote_up(
    store: Store,
    bind: &str,
    public_url: Option<&str>,
    sweep_every: Duration,
) -> Result<()> {
    if bind.ends_with(":0") {
        bail!("remote up needs a fixed bind port so the tunnel has somewhere to forward");
    }
    let local = if bind.starts_with("http://") || bind.starts_with("https://") {
        bail!("--bind is host:port, not a URL");
    } else {
        format!("http://{bind}")
    };
    let tunnel = match public_url {
        Some(_) => None,
        None => Some(start_quick_tunnel(&local)?),
    };
    let public = match public_url {
        Some(url) => url.to_string(),
        None => tunnel.as_ref().expect("tunnel").url.clone(),
    };
    let oauth = parse_public_url(&public)?;
    eprintln!("another: point the tunnel at {local}");
    eprintln!(
        "Paste this server URL into ChatGPT or Claude:\n  {}",
        oauth.resource
    );
    eprintln!(
        "When the approval page opens, run on this machine:\n  \
         another oauth approve <code> --agent NAME --project '/path/*'"
    );
    if tunnel.is_some() {
        eprintln!(
            "This is a quick tunnel. The URL changes every run. For a stable hostname, use a named tunnel:\n{NAMED_TUNNEL}"
        );
    }
    let host = oauth.host_value();
    let opts = HttpOptions {
        auth: true,
        allowed_hosts: vec![host],
        oauth: Some(oauth),
        ..Default::default()
    };
    serve_http(store, bind, opts, sweep_every).await?;
    drop(tunnel);
    Ok(())
}

struct QuickTunnel {
    child: Child,
    url: String,
}

impl Drop for QuickTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_quick_tunnel(local_url: &str) -> Result<QuickTunnel> {
    let mut child = Command::new("cloudflared")
        .args(["tunnel", "--url", local_url, "--no-autoupdate"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("cloudflared is not installed.\n{NAMED_TUNNEL}")
            } else {
                anyhow::anyhow!("could not start cloudflared: {e}\n{NAMED_TUNNEL}")
            }
        })?;
    let stdout = child.stdout.take().context("cloudflared stdout")?;
    let stderr = child.stderr.take().context("cloudflared stderr")?;
    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone());
    spawn_reader(stderr, tx);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut seen = String::new();
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            let _ = child.kill();
            bail!(
                "cloudflared did not print a tunnel URL within 45s.\n{}\n{NAMED_TUNNEL}",
                tail(&seen)
            );
        }
        match rx.recv_timeout(wait) {
            Ok(line) => {
                eprintln!("cloudflared: {line}");
                if seen.len() < 4000 {
                    seen.push_str(&line);
                    seen.push('\n');
                }
                if let Some(url) = parse_quick_tunnel_url(&line) {
                    return Ok(QuickTunnel { child, url });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                bail!(
                    "cloudflared did not print a tunnel URL within 45s.\n{}\n{NAMED_TUNNEL}",
                    tail(&seen)
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().ok();
                bail!(
                    "cloudflared exited before printing a URL ({status:?}).\n{}\n{NAMED_TUNNEL}",
                    tail(&seen)
                );
            }
        }
    }
}

fn spawn_reader(reader: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let lines = BufReader::new(reader).lines();
        for line in lines.map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

/// Pull a `https://*.trycloudflare.com` origin out of a cloudflared log line.
pub fn parse_quick_tunnel_url(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(start) = rest.find("https://") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '|' | '>'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ',', ')', ']']);
        if let Some(host) = url.trim_start_matches("https://").split('/').next() {
            if host.ends_with(".trycloudflare.com") && host != "api.trycloudflare.com" {
                return Some(format!("https://{host}"));
            }
        }
        let step = end.max("https://".len());
        if step >= candidate.len() {
            break;
        }
        rest = &candidate[step..];
    }
    None
}

fn tail(text: &str) -> String {
    let start = text.len().saturating_sub(600);
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quick_tunnel_urls() {
        assert_eq!(
            parse_quick_tunnel_url(
                "2026-11-10 INF Requesting new quick Tunnel on trycloudflare.com"
            ),
            None
        );
        assert_eq!(
            parse_quick_tunnel_url("INF |  https://widget-abc.trycloudflare.com  |").as_deref(),
            Some("https://widget-abc.trycloudflare.com")
        );
        assert_eq!(
            parse_quick_tunnel_url("see https://api.trycloudflare.com/ for docs"),
            None
        );
    }
}
