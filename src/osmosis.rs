//! `knishio osmosis status` — query the validator's `/osmosis/status` endpoint.
//!
//! Reports the pruning worker's last-run timestamp, lifetime totals, queue
//! depth estimate, and dry-run flag. Always returns a populated snapshot;
//! `enabled: false` when OSMOSIS_ENABLED=false at validator startup.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;
use std::time::Duration;

use crate::config::Config;
use crate::output;

/// Mirrors `knishio_validator::osmosis::OsmosisStatusSnapshot`.
#[derive(Debug, Deserialize)]
struct OsmosisStatusBody {
    enabled: bool,
    dry_run: bool,
    retention_days: u64,
    interval_secs: u64,
    last_run_at: Option<i64>,
    last_rejected_pruned: u64,
    last_accepted_pruned: u64,
    total_rejected_pruned_lifetime: u64,
    total_accepted_pruned_lifetime: u64,
    /// `-1` from the validator means "queue depth not yet measured".
    queue_depth_estimate: i64,
    cycle_count: u64,
}

pub async fn status(config: &Config) -> Result<()> {
    let url = format!("{}/osmosis/status", config.validator.url);
    let client = http_client(config.validator.insecure_tls)?;
    let body: OsmosisStatusBody = client
        .get(&url)
        .send()
        .await
        .context("Failed to GET /osmosis/status — is the validator running?")?
        .error_for_status()
        .context("/osmosis/status returned a non-success status")?
        .json()
        .await
        .context("/osmosis/status response was not valid JSON")?;

    output::header("Osmosis Pruning Status");

    if !body.enabled {
        output::info("Osmosis disabled (OSMOSIS_ENABLED=false at validator startup)");
        println!("  Configured retention: {}d", body.retention_days);
        println!("  Configured interval:  {}s", body.interval_secs);
        return Ok(());
    }

    let mode = if body.dry_run {
        "dry-run".yellow().to_string()
    } else {
        "live".green().to_string()
    };
    let now = unix_now();

    println!("  Mode:               {}", mode);
    println!("  Retention:          {}d", body.retention_days);
    println!("  Interval:           {}s", body.interval_secs);
    match body.last_run_at {
        Some(ts) => println!(
            "  Last run:           {} ({})",
            ts,
            format_age(now.saturating_sub(ts))
        ),
        None => println!("  Last run:           {}", "never (worker just started)".dimmed()),
    }
    println!("  Cycles completed:   {}", body.cycle_count);

    println!();
    output::header("Last Cycle");
    println!(
        "  Rejected pruned:    {}",
        format_count(body.last_rejected_pruned)
    );
    println!(
        "  Accepted pruned:    {}{}",
        format_count(body.last_accepted_pruned),
        if body.dry_run { "  (dry-run — not actually deleted)".dimmed().to_string() } else { String::new() }
    );

    println!();
    output::header("Lifetime Totals");
    println!(
        "  Rejected pruned:    {}",
        format_count(body.total_rejected_pruned_lifetime)
    );
    println!(
        "  Accepted pruned:    {}",
        format_count(body.total_accepted_pruned_lifetime)
    );

    if body.queue_depth_estimate >= 0 {
        println!();
        output::header("Queue");
        println!(
            "  Estimated eligible: {}",
            format_count(body.queue_depth_estimate as u64)
        );
    }

    if body.dry_run {
        println!();
        output::warn(
            "Dry-run mode is on — the worker scans + counts but does NOT delete. \
             Set OSMOSIS_DRY_RUN=false in .env.production and restart to enable live pruning.",
        );
    }

    Ok(())
}

fn http_client(insecure_tls: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
