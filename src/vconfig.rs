//! `knishio config show` / `rate-limit status` / `reconciliation status` —
//! read the validator's runtime config from its read-only `GET /config`
//! endpoint (a redacted snapshot; secrets are omitted server-side).
//!
//! The validator's config is env-derived and static post-startup, so `/config`
//! reflects the running values. `reconciliation status` also scrapes `/metrics`
//! for the live worker counters.

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::Value;
use std::time::Duration;

use crate::config::Config;
use crate::output;

fn http_client(insecure_tls: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

async fn fetch_config(config: &Config) -> Result<Value> {
    let url = format!("{}/config", config.validator.url);
    http_client(config.validator.insecure_tls)?
        .get(&url)
        .send()
        .await
        .context("Failed to GET /config — is the validator running?")?
        .error_for_status()
        .context("/config returned a non-success status")?
        .json()
        .await
        .context("/config response was not valid JSON")
}

fn fmt_val(v: &Value) -> String {
    match v {
        Value::Null => "(none)".dimmed().to_string(),
        Value::Bool(true) => "true".green().to_string(),
        Value::Bool(false) => "false".yellow().to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn print_section(title: &str, obj: &Value) {
    println!("\n  {}", title.bold());
    match obj.as_object() {
        Some(map) => {
            for (k, v) in map {
                println!("    {:<28} {}", k, fmt_val(v));
            }
        }
        None => println!("    {}", "(section absent)".dimmed()),
    }
}

/// `knishio config show` — the full redacted runtime-config snapshot.
pub async fn show(config: &Config) -> Result<()> {
    let cfg = fetch_config(config).await?;
    output::header("Validator runtime config");
    for section in [
        "server",
        "database",
        "auth",
        "tls",
        "rate_limit",
        "reconciliation",
        "observability",
        "features",
    ] {
        print_section(section, &cfg[section]);
    }
    println!();
    output::info("Secrets (JWT secret, DATABASE_URL) are omitted by the validator.");
    Ok(())
}

/// `knishio rate-limit status` — the rate-limit section of `/config`.
pub async fn rate_limit_status(config: &Config) -> Result<()> {
    let cfg = fetch_config(config).await?;
    output::header("Rate limit");
    print_section("rate_limit", &cfg["rate_limit"]);
    Ok(())
}

/// `knishio reconciliation status` — the reconciliation config (`/config`) plus
/// the live worker counters (`/metrics`).
pub async fn reconciliation_status(config: &Config) -> Result<()> {
    let cfg = fetch_config(config).await?;
    output::header("Reconciliation");
    print_section("config", &cfg["reconciliation"]);

    let metrics = fetch_metrics(config).await.unwrap_or_default();
    println!("\n  {}", "activity (from /metrics)".bold());
    for (label, metric) in [
        ("bonds_reconciled_total", "knishio_bonds_reconciled_total"),
        ("bonds_reconcile_failed_total", "knishio_bonds_reconcile_failed_total"),
        ("pending_swept_total", "knishio_pending_swept_total"),
    ] {
        let v = scrape_counter(&metrics, metric).unwrap_or(0.0);
        println!("    {:<28} {}", label, v);
    }
    Ok(())
}

async fn fetch_metrics(config: &Config) -> Result<String> {
    let url = format!("{}/metrics", config.validator.url);
    let body = http_client(config.validator.insecure_tls)?
        .get(&url)
        .send()
        .await
        .context("Failed to GET /metrics")?
        .text()
        .await
        .context("/metrics response was not text")?;
    Ok(body)
}

/// Scrape an unlabelled Prometheus counter (`name <value>`) from the text body.
fn scrape_counter(body: &str, name: &str) -> Option<f64> {
    body.lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix(name)?;
            // Match the exact metric (no labels): "name <value>" — reject "name_other".
            let rest = rest.trim_start();
            if rest.starts_with('{') {
                return None; // labelled variant — these counters are unlabelled
            }
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
}
