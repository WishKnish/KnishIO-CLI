//! Cell management via `docker exec psql`.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::config::Config;
use crate::output;

// ── Input Validation ────────────────────────────────────────

const SLUG_MAX_LEN: usize = 64;
const NAME_MAX_LEN: usize = 256;
const VALID_STATUSES: &[&str] = &["active", "paused", "archived"];
pub const BENCH_PREFIX: &str = "BENCH_CLI_";

fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug.len() > SLUG_MAX_LEN {
        anyhow::bail!("Slug must be 1-{} characters", SLUG_MAX_LEN);
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Slug must contain only alphanumeric characters, dashes, and underscores"
        );
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > NAME_MAX_LEN {
        anyhow::bail!("Name must be 1-{} characters", NAME_MAX_LEN);
    }
    if name.contains('\0') || name.chars().any(|c| c.is_control() && c != ' ') {
        anyhow::bail!("Name must not contain null bytes or control characters");
    }
    Ok(())
}

fn validate_status(status: &str) -> Result<()> {
    if !VALID_STATUSES.contains(&status) {
        anyhow::bail!(
            "Invalid status '{}'. Must be one of: {}",
            status,
            VALID_STATUSES.join(", ")
        );
    }
    Ok(())
}

// ── Database Operations ─────────────────────────────────────

/// Execute a SQL statement inside the Postgres container.
async fn psql(config: &Config, sql: &str) -> Result<String> {
    let out = Command::new("docker")
        .args([
            "exec",
            &config.docker.postgres_container,
            "psql",
            "-U",
            &config.database.user,
            "-d",
            &config.database.name,
            "-t",
            "-A",
            "-c",
            sql,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to exec into postgres container — is the stack running?")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Scrub error: don't expose raw SQL errors with schema details
        if stderr.contains("does not exist") {
            anyhow::bail!("Database operation failed — run `knishio db` to check consistency");
        } else if stderr.contains("connection refused") || stderr.contains("could not connect") {
            anyhow::bail!("Cannot connect to database — is the stack running?");
        } else {
            anyhow::bail!("Database operation failed (run with RUST_LOG=debug for details)");
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn create(config: &Config, slug: &str, name: Option<&str>, status: &str) -> Result<()> {
    validate_slug(slug)?;
    validate_status(status)?;
    let display_name = name.unwrap_or(slug);
    if let Some(n) = name {
        validate_name(n)?;
    }

    let sql = format!(
        "INSERT INTO cells (slug, name, status) VALUES ('{}', '{}', '{}') \
         ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name, status = EXCLUDED.status",
        slug.replace('\'', "''"),
        display_name.replace('\'', "''"),
        status.replace('\'', "''"),
    );
    psql(config, &sql).await?;
    output::success(&format!("Cell '{}' created (status: {})", slug, status));
    Ok(())
}

pub async fn list(config: &Config) -> Result<()> {
    let sql = "SELECT slug, name, status, created_at FROM cells ORDER BY created_at";
    let result = psql(config, sql).await?;
    if result.is_empty() {
        output::info("No cells found");
        return Ok(());
    }

    output::header("Cells");
    println!(
        "{:<20} {:<30} {:<12} {}",
        "SLUG", "NAME", "STATUS", "CREATED"
    );
    println!("{}", "-".repeat(80));
    for line in result.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 4 {
            println!(
                "{:<20} {:<30} {:<12} {}",
                parts[0], parts[1], parts[2], parts[3]
            );
        }
    }
    Ok(())
}

/// Purge all data associated with a benchmark cell, then hard-delete it.
/// SAFETY: Only cells with the BENCH_CLI_ prefix can be purged.
/// Atoms, bonds, and cascades auto-cascade from molecule deletion.
/// used_positions intentionally NOT touched (OTS anti-replay is global).
pub async fn purge(config: &Config, slug: &str) -> Result<()> {
    validate_slug(slug)?;

    // SAFETY: Refuse to purge non-benchmark cells
    if !slug.starts_with(BENCH_PREFIX) {
        anyhow::bail!(
            "Refusing to purge non-benchmark cell '{}'. Only cells with '{}' prefix can be purged.",
            slug, BENCH_PREFIX
        );
    }

    let escaped = slug.replace('\'', "''");
    // Disable the cascade_before_molecule_delete trigger to avoid
    // "tuple already modified" errors during bulk cell purge.
    // Bond migration (osmosis) is pointless when deleting the entire cell.
    let sql = format!(
        "BEGIN; \
         DELETE FROM metas WHERE molecular_hash IN (SELECT molecular_hash FROM molecules WHERE cell_slug = '{escaped}'); \
         DELETE FROM audit_events WHERE cell_slug = '{escaped}'; \
         DELETE FROM user_activity WHERE cell_slug = '{escaped}'; \
         DELETE FROM active_sessions WHERE cell_slug = '{escaped}'; \
         DELETE FROM batches WHERE cell_slug = '{escaped}'; \
         DELETE FROM osmosis_snapshots WHERE cell_slug = '{escaped}'; \
         DELETE FROM sync_state WHERE cell_slug = '{escaped}'; \
         DELETE FROM auth_tokens WHERE cell_slug = '{escaped}'; \
         DELETE FROM molecular_cascades WHERE cell_slug = '{escaped}'; \
         ALTER TABLE molecules DISABLE TRIGGER cascade_before_molecule_delete; \
         DELETE FROM molecules WHERE cell_slug = '{escaped}'; \
         ALTER TABLE molecules ENABLE TRIGGER cascade_before_molecule_delete; \
         DELETE FROM cells WHERE slug = '{escaped}'; \
         COMMIT;"
    );
    psql(config, &sql).await?;
    output::success(&format!("Cell '{}' purged and deleted", slug));
    Ok(())
}

/// List all benchmark cell slugs (BENCH_CLI_*), including archived.
pub async fn list_bench_slugs(config: &Config) -> Result<Vec<String>> {
    let sql = "SELECT slug FROM cells WHERE slug LIKE 'BENCH_CLI_%' ORDER BY created_at";
    let result = psql(config, sql).await?;
    Ok(result.lines().filter(|l| !l.is_empty()).map(|l| l.to_string()).collect())
}

pub async fn set_status(config: &Config, slug: &str, status: &str) -> Result<()> {
    validate_slug(slug)?;
    validate_status(status)?;

    let sql = format!(
        "UPDATE cells SET status = '{}' WHERE slug = '{}'",
        status.replace('\'', "''"),
        slug.replace('\'', "''"),
    );
    let _result = psql(config, &sql).await?;
    let check = psql(
        config,
        &format!(
            "SELECT status FROM cells WHERE slug = '{}'",
            slug.replace('\'', "''")
        ),
    )
    .await?;
    if check.is_empty() {
        output::error(&format!("Cell '{}' not found", slug));
    } else {
        output::success(&format!("Cell '{}' → {}", slug, status));
    }
    Ok(())
}
