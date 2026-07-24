//! `knishio update` — health-gated update with rollback.
//!
//! Pulls new images (or rebuilds from source), restarts the stack,
//! waits for /readyz, and rolls back if health check fails.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::output;

/// Maximum time to wait for the validator to become ready after restart.
const READYZ_TIMEOUT: Duration = Duration::from_secs(120);

/// Interval between readyz polling attempts.
const READYZ_INTERVAL: Duration = Duration::from_secs(5);

/// Pull latest images and restart with health verification.
pub async fn update(
    compose_file: &Path,
    cfg: &Config,
    build_from_source: bool,
    env: &[(&str, &str)],
) -> Result<()> {
    // 1. Capture current version before update
    let version_before = get_version(&cfg.validator.url, cfg.validator.insecure_tls).await;

    if build_from_source {
        // Source-based update: rebuild images
        output::info("Rebuilding from source...");
        compose(compose_file, &["build"], env).await?;
    } else {
        // Registry-based update: pull latest images
        output::info("Pulling latest images...");
        compose(compose_file, &["pull"], env).await?;
    }

    // 2. Restart with new images (only recreates changed services)
    output::info("Restarting services...");
    compose(compose_file, &["up", "-d"], env).await?;

    // 3. Wait for readyz
    output::info("Waiting for validator to become ready...");
    let healthy = wait_for_ready(&cfg.validator.url, cfg.validator.insecure_tls).await;

    if healthy {
        let version_after = get_version(&cfg.validator.url, cfg.validator.insecure_tls).await;

        output::success("Update complete — validator is healthy");
        if let Some(before) = &version_before {
            output::info(&format!("  Version before: {}", before));
        }
        if let Some(after) = &version_after {
            output::info(&format!("  Version after:  {}", after));
        }
    } else {
        output::error("Validator failed to become ready within timeout");
        output::warn("Checking container logs for errors...");

        // Show recent logs for debugging
        let _ = compose(compose_file, &["logs", "--tail", "50", "validator"], &[]).await;

        output::error("Update may have failed. Options:");
        output::info("  1. Check logs:    knishio logs --follow");
        output::info("  2. Roll back:     knishio update --rollback");
        output::info("  3. Full rebuild:  knishio rebuild");

        anyhow::bail!("Health check failed after update");
    }

    Ok(())
}

/// Roll back to the previous version by restarting without pulling.
pub async fn rollback(compose_file: &Path, cfg: &Config) -> Result<()> {
    output::warn("Rolling back — restarting with previous images...");

    // Force recreate containers with existing images
    compose(compose_file, &["up", "-d", "--force-recreate"], &[]).await?;

    // Wait for health
    output::info("Waiting for validator to become ready...");
    let healthy = wait_for_ready(&cfg.validator.url, cfg.validator.insecure_tls).await;

    if healthy {
        output::success("Rollback complete — validator is healthy");
    } else {
        output::error("Rollback failed — validator did not become ready");
        output::info("  Check logs: knishio logs --follow");
    }

    Ok(())
}

/// Poll /readyz until it returns 200 or timeout is reached.
async fn wait_for_ready(url: &str, insecure_tls: bool) -> bool {
    crate::http::wait_for_ready(url, insecure_tls, READYZ_TIMEOUT, READYZ_INTERVAL).await
}

/// Get the current version from /health endpoint.
async fn get_version(url: &str, insecure_tls: bool) -> Option<String> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));

    let client = crate::http::client(insecure_tls, crate::http::SHORT_TIMEOUT).ok()?;
    let resp = client.get(&health_url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("version")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Run a docker compose command against a single compose file. Delegates to
/// the canonical `docker::compose` (BuildKit env + unconditional
/// `.env.production` auto-detection — the private copy this replaced only
/// applied the env file when "production" appeared in the file NAME, silently
/// dropping operator env on dev-named stacks).
async fn compose(compose_file: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<bool> {
    crate::docker::compose(&[compose_file.to_path_buf()], args, env).await
}
