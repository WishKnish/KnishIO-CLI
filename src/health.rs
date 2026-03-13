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
