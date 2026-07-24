//! `knishio p2p status` — query the validator's `/p2p/status` endpoint.
//!
//! The validator exposes a read-only snapshot of P2P state (peer counts by
//! status, top peers by reputation, configured bootstrap list). When P2P
//! is disabled at startup the endpoint returns `{"enabled": false}` with
//! HTTP 200 so this CLI renders a graceful "disabled" message.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;

use crate::config::Config;
use crate::output;

/// Mirrors `knishio_validator::p2p::P2pStatusSnapshot` for deserialization.
/// The CLI duplicates the type rather than depending on the validator crate
/// to keep the build matrix small (CLI ships separately).
#[derive(Debug, Deserialize)]
struct P2pStatusBody {
    enabled: bool,
    self_host: String,
    bootstrap_peers: Vec<String>,
    counts: PeerCounts,
    top_peers: Vec<PeerSummary>,
}

#[derive(Debug, Deserialize)]
struct PeerCounts {
    active: i64,
    suspended: i64,
    banned: i64,
    stale: i64,
    total: i64,
}

#[derive(Debug, Deserialize)]
struct PeerSummary {
    host: String,
    status: String,
    reputation_score: f32,
    total_valid: i64,
    total_invalid: i64,
    avg_latency_ms: f32,
    last_seen_at: i64,
}

pub async fn status(config: &Config) -> Result<()> {
    let url = format!("{}/p2p/status", config.validator.url);
    let client = http_client(config.validator.insecure_tls)?;
    let body: P2pStatusBody = client
        .get(&url)
        .send()
        .await
        .context("Failed to GET /p2p/status — is the validator running?")?
        .error_for_status()
        .context("/p2p/status returned a non-success status")?
        .json()
        .await
        .context("/p2p/status response was not valid JSON")?;

    output::header("P2P Status");

    if !body.enabled {
        output::info("P2P disabled (P2P_ENABLED=false at validator startup)");
        if !body.self_host.is_empty() {
            println!("  Configured self_host: {}", body.self_host);
        }
        if !body.bootstrap_peers.is_empty() {
            println!(
                "  Configured bootstrap peers: {}",
                body.bootstrap_peers.len()
            );
        }
        return Ok(());
    }

    println!("  Self host:        {}", body.self_host.cyan());
    println!("  Bootstrap peers:  {}", body.bootstrap_peers.len());

    let now = unix_now();
    println!();
    output::header("Peer Counts");
    println!(
        "  {:<12} {}  {:<12} {}",
        "active:".green(),
        body.counts.active,
        "stale:".yellow(),
        body.counts.stale
    );
    println!(
        "  {:<12} {}  {:<12} {}",
        "suspended:".yellow(),
        body.counts.suspended,
        "banned:".red(),
        body.counts.banned
    );
    println!("  {:<12} {}", "total:", body.counts.total);

    if body.top_peers.is_empty() {
        println!();
        output::info("No peers in registry yet");
        return Ok(());
    }

    println!();
    output::header(&format!("Top {} peers by reputation", body.top_peers.len()));
    println!(
        "{:<40} {:<10} {:<10} {:<8} {:<8} {:<10} LAST_SEEN",
        "HOST", "STATUS", "REP", "VALID", "INVALID", "LATENCY"
    );
    println!("{}", "-".repeat(110));
    for p in &body.top_peers {
        let status = colorize_peer_status(&p.status);
        let host = truncate(&p.host, 40);
        println!(
            "{:<40} {:<10} {:<10.2} {:<8} {:<8} {:<10} {}",
            host,
            status,
            p.reputation_score,
            p.total_valid,
            p.total_invalid,
            format!("{:.1}ms", p.avg_latency_ms),
            format_age(now.saturating_sub(p.last_seen_at)),
        );
    }
    Ok(())
}

fn http_client(insecure_tls: bool) -> Result<reqwest::Client> {
    crate::http::client(insecure_tls, crate::http::SHORT_TIMEOUT)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}

fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{}s ago", s)
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{:.1}h ago", s as f64 / 3600.0)
    } else {
        format!("{}d ago", s / 86400)
    }
}

fn colorize_peer_status(s: &str) -> String {
    match s {
        "active" => s.green().to_string(),
        "stale" => s.yellow().to_string(),
        "suspended" => s.yellow().to_string(),
        "banned" => s.red().to_string(),
        other => other.to_string(),
    }
}
