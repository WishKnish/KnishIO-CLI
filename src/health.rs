//! Health check endpoints via HTTP.

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::Value;
use std::time::Duration;

use crate::output;

fn client(insecure_tls: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));

    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder.build().context("Failed to build HTTP client")
}

async fn get_endpoint(
    base: &str,
    path: &str,
    insecure_tls: bool,
) -> Result<(u16, String)> {
    let url = format!("{}{}", base, path);
    let resp = client(insecure_tls)?
        .get(&url)
        .send()
        .await
        .map_err(|e| {
            let err_str = format!("{:?}", e).to_lowercase();
            if err_str.contains("certificate")
                || err_str.contains("ssl")
                || err_str.contains("tls")
                || err_str.contains("verify")
                || err_str.contains("handshake")
            {
                anyhow::anyhow!(
                    "TLS certificate error: {}\n\
                     Hint: set insecure_tls = true in knishio.toml or KNISHIO_INSECURE_TLS=true for self-signed certs",
                    e
                )
            } else {
                anyhow::anyhow!("Connection failed: {} — is the validator running?", e)
            }
        })?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

pub async fn healthz(base: &str, insecure_tls: bool) -> Result<()> {
    let (status, body) = get_endpoint(base, "/healthz", insecure_tls).await?;
    if status == 200 {
        output::success(&format!("Healthy ({})", base));
    } else {
        output::error(&format!("Unhealthy — HTTP {} : {}", status, body));
    }
    Ok(())
}

/// Richer health probe: hits `/health` (not `/healthz`) which includes
/// DB connectivity + latency, cache stats, and the validator version.
/// Used when `knishio health --full` is invoked.
pub async fn health_full(base: &str, insecure_tls: bool) -> Result<()> {
    let (status, body) = get_endpoint(base, "/health", insecure_tls).await?;

    if status != 200 {
        output::error(&format!("Unhealthy — HTTP {} : {}", status, body));
        return Ok(());
    }

    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            output::error("Could not parse /health response as JSON");
            println!("{}", body);
            return Ok(());
        }
    };

    let overall = json
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let version = json
        .get("version")
        .and_then(|s| s.as_str())
        .unwrap_or("?");

    if overall == "healthy" {
        output::success(&format!("Healthy ({}) · v{}", base, version));
    } else {
        output::error(&format!("Status: {} ({}) · v{}", overall, base, version));
    }

    output::header("Database");
    if let Some(db) = json.get("database") {
        let status = db.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        let latency = db.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let status_label = if status == "connected" {
            status.green().to_string()
        } else {
            status.yellow().to_string()
        };
        println!("  status        {}", status_label);
        println!("  latency       {} ms", latency);
    } else {
        println!("  {}", "(no database section in response)".dimmed());
    }

    output::header("Query-embedding cache");
    if let Some(cache) = json.get("cache") {
        let entries = cache.get("entries").and_then(|v| v.as_u64()).unwrap_or(0);
        let hit_ratio = cache
            .get("hit_ratio")
            .and_then(|v| v.as_str())
            .unwrap_or("0.00");
        println!("  entries       {}", entries);
        println!("  hit ratio     {}", hit_ratio);
    } else {
        println!("  {}", "(no cache section in response)".dimmed());
    }

    Ok(())
}

pub async fn readyz(base: &str, full: bool, insecure_tls: bool) -> Result<()> {
    let (status, body) = get_endpoint(base, "/readyz", insecure_tls).await?;
    if status == 200 {
        output::success("Ready");
    } else {
        output::error(&format!("Not ready — HTTP {}", status));
    }
    if full {
        if let Ok(json) = serde_json::from_str::<Value>(&body) {
            println!("{}", serde_json::to_string_pretty(&json).unwrap_or(body));
        } else {
            println!("{}", body);
        }
    }
    Ok(())
}

pub async fn db_check(base: &str, insecure_tls: bool) -> Result<()> {
    let (_status, body) = get_endpoint(base, "/db-check", insecure_tls).await?;

    if let Ok(json) = serde_json::from_str::<Value>(&body) {
        let is_consistent = json
            .get("consistency")
            .and_then(|c| c.get("is_consistent"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_consistent {
            output::success("Database consistency check passed");
        } else {
            output::error("Database consistency check FAILED");
        }

        if let Some(migrations) = json.get("migrations") {
            let applied = migrations
                .get("applied_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let expected = migrations
                .get("expected_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let is_current = migrations
                .get("is_current")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            output::header("Migrations");
            println!("  Applied: {} / {} expected", applied, expected);
            if is_current {
                println!("  {}", "Up to date".green());
            } else {
                println!("  {}", "Migrations pending!".yellow());
            }

            if let Some(failed) = migrations.get("failed_migrations").and_then(|v| v.as_array()) {
                if !failed.is_empty() {
                    output::header("Failed Migrations");
                    for m in failed {
                        println!("  {} {}", "•".red(), m.as_str().unwrap_or("unknown"));
                    }
                }
            }
        }

        if let Some(consistency) = json.get("consistency") {
            let mut issues: Vec<String> = Vec::new();

            if let Some(tables) = consistency.get("missing_tables").and_then(|v| v.as_array()) {
                for t in tables {
                    issues.push(format!("Missing table: {}", t.as_str().unwrap_or("?")));
                }
            }
            if let Some(triggers) = consistency.get("missing_triggers").and_then(|v| v.as_array())
            {
                for t in triggers {
                    issues.push(format!("Missing trigger: {}", t.as_str().unwrap_or("?")));
                }
            }

            if !issues.is_empty() {
                output::header("Issues");
                for issue in &issues {
                    println!("  {} {}", "•".red(), issue);
                }
            }
        }
    } else {
        println!("{}", body);
    }
    Ok(())
}
