//! `knishio audit list` — query the `audit_events` table via `docker exec psql`.
//!
//! Wraps a SELECT against the existing audit table populated by the validator's
//! src/audit.rs. No new validator endpoint or schema; the CLI just lifts a
//! formatted view of recent events without operators having to remember the
//! psql column shape.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::process::Stdio;
use tokio::process::Command;

use crate::cell::{validate_bundle, validate_slug};
use crate::config::Config;
use crate::output;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;

/// Filters accepted by `audit list`. All optional except `limit`.
pub struct ListFilters {
    pub action: Option<String>,
    pub category: Option<String>,
    pub bundle: Option<String>,
    pub cell: Option<String>,
    pub severity: Option<String>,
    /// Duration suffix (`30m`, `2h`, `7d`) or bare epoch seconds.
    pub since: Option<String>,
    pub limit: u32,
}

impl Default for ListFilters {
    fn default() -> Self {
        Self {
            action: None,
            category: None,
            bundle: None,
            cell: None,
            severity: None,
            since: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// Run a SELECT against audit_events and pretty-print the results.
pub async fn list(config: &Config, filters: ListFilters) -> Result<()> {
    if filters.limit == 0 || filters.limit > MAX_LIMIT {
        bail!("--limit must be between 1 and {}", MAX_LIMIT);
    }
    if let Some(b) = &filters.bundle {
        validate_bundle(b)
            .context("--bundle must be 64-char lowercase hex (the canonical bundle-hash form)")?;
    }
    if let Some(s) = &filters.cell {
        validate_slug(s).context("--cell must be a valid cell slug")?;
    }

    let now = unix_now()?;
    let since_epoch = filters
        .since
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;

    // ── Build the WHERE clause ────────────────────────────────────
    // SQL string-quote everything we splice in (single-quote escape).
    // Bundles + slugs are pre-validated; action/category/severity have
    // free-form input but we never trust it — escape and rely on the
    // VARCHAR column types to discard absurd values.
    let mut wheres: Vec<String> = vec!["1=1".into()];
    if let Some(v) = &filters.action {
        wheres.push(format!("action = '{}'", esc(v)));
    }
    if let Some(v) = &filters.category {
        wheres.push(format!("category = '{}'", esc(v)));
    }
    if let Some(v) = &filters.bundle {
        wheres.push(format!("actor_bundle = '{}'", esc(v)));
    }
    if let Some(v) = &filters.cell {
        wheres.push(format!("cell_slug = '{}'", esc(v)));
    }
    if let Some(v) = &filters.severity {
        wheres.push(format!("severity = '{}'", esc(v)));
    }
    if let Some(epoch) = since_epoch {
        wheres.push(format!("created_at >= {}", epoch));
    }

    // ORDER BY created_at DESC uses the partial-index on created_at — fast
    // even for million-row audit tables (validator ships an index on it).
    let sql = format!(
        "SELECT created_at, COALESCE(severity, 'info'), category, action, \
                COALESCE(actor_bundle, ''), COALESCE(cell_slug, ''), \
                COALESCE(target_type, ''), COALESCE(target_id, '') \
         FROM audit_events \
         WHERE {} \
         ORDER BY created_at DESC \
         LIMIT {}",
        wheres.join(" AND "),
        filters.limit
    );

    let raw = psql(config, &sql).await?;

    if raw.trim().is_empty() {
        output::info("No audit events match the given filters.");
        return Ok(());
    }

    // ── Render ───────────────────────────────────────────────────
    let descriptor = describe_filters(&filters, since_epoch, now);
    output::header(&format!("Audit Events ({})", descriptor));

    println!(
        "{:<14} {:<8} {:<14} {:<24} {:<14} {:<16} {}",
        "AGE", "SEVERITY", "CATEGORY", "ACTION", "CELL", "BUNDLE", "TARGET"
    );
    println!("{}", "-".repeat(120));

    let mut row_count = 0usize;
    for line in raw.lines() {
        let cols: Vec<&str> = line.split('|').collect();
        if cols.len() < 8 {
            continue;
        }
        let ts: i64 = cols[0].parse().unwrap_or(0);
        let age = format_age(now.saturating_sub(ts));
        let severity = colorize_severity(cols[1]);
        let target = if cols[6].is_empty() {
            cols[7].to_string()
        } else {
            format!("{}={}", cols[6], cols[7])
        };

        println!(
            "{:<14} {:<8} {:<14} {:<24} {:<14} {:<16} {}",
            age,
            severity,
            truncate(cols[2], 14),
            truncate(cols[3], 24),
            truncate(cols[5], 14),
            truncate(cols[4], 16),
            truncate(&target, 50),
        );
        row_count += 1;
    }

    println!();
    output::info(&format!(
        "{} event{} shown (limit {})",
        row_count,
        if row_count == 1 { "" } else { "s" },
        filters.limit
    ));
    Ok(())
}

fn describe_filters(filters: &ListFilters, since_epoch: Option<i64>, now: i64) -> String {
    let mut parts = Vec::new();
    if let Some(epoch) = since_epoch {
        parts.push(format!("since={}", format_age(now.saturating_sub(epoch))));
    }
    if let Some(v) = &filters.action {
        parts.push(format!("action={}", v));
    }
    if let Some(v) = &filters.category {
        parts.push(format!("category={}", v));
    }
    if let Some(v) = &filters.bundle {
        parts.push(format!("bundle={}", truncate(v, 12)));
    }
    if let Some(v) = &filters.cell {
        parts.push(format!("cell={}", v));
    }
    if let Some(v) = &filters.severity {
        parts.push(format!("severity={}", v));
    }
    if parts.is_empty() {
        format!("last {}", filters.limit)
    } else {
        parts.join(", ")
    }
}

fn colorize_severity(sev: &str) -> String {
    match sev {
        "critical" => "critical".red().to_string(),
        "warn" => "warn".yellow().to_string(),
        "info" => "info".to_string(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}

/// Format a duration as a compact relative string: `5s ago`, `12m ago`,
/// `2.3h ago`, `4d ago`. Operators care more about recency than wall-clock.
fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{}s ago", s)
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        let h = s as f64 / 3600.0;
        format!("{:.1}h ago", h)
    } else {
        format!("{}d ago", s / 86400)
    }
}

/// Parse `--since`. Accepts:
/// - `30s`, `15m`, `2h`, `7d` (relative to now)
/// - bare epoch seconds (e.g. `1762553625`)
fn parse_since(s: &str, now: i64) -> Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("--since cannot be empty");
    }
    if let Ok(epoch) = s.parse::<i64>() {
        return Ok(epoch);
    }
    // Suffix duration: split off the trailing unit char.
    let unit = s
        .chars()
        .last()
        .ok_or_else(|| anyhow::anyhow!("--since: empty value"))?;
    let num_part = &s[..s.len() - unit.len_utf8()];
    let n: i64 = num_part.parse().with_context(|| {
        format!(
            "--since '{}': prefix '{}' is not an integer (expected '30s'/'15m'/'2h'/'7d')",
            s, num_part
        )
    })?;
    let secs = match unit {
        's' => n,
        'm' => n * 60,
        'h' => n * 3600,
        'd' => n * 86400,
        other => bail!(
            "--since '{}': unit '{}' must be one of s/m/h/d (e.g. '30m', '2h', '7d')",
            s,
            other
        ),
    };
    Ok(now - secs)
}

fn unix_now() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("System clock before Unix epoch — cannot compute --since")?
        .as_secs() as i64)
}

fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Local copy of the psql shell-out helper (mirrors the pattern in cell.rs /
/// embed.rs). Keeps the per-module SQL plumbing self-contained.
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
            "-q",
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
        if stderr.contains("does not exist") {
            anyhow::bail!(
                "audit_events table missing — has the validator started and run migrations?"
            );
        }
        if stderr.contains("connection refused") || stderr.contains("could not connect") {
            anyhow::bail!("Cannot connect to database — is the stack running?");
        }
        anyhow::bail!("psql query failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_minutes() {
        let now = 1_000_000;
        assert_eq!(parse_since("30m", now).unwrap(), now - 30 * 60);
    }

    #[test]
    fn parse_since_hours_and_days() {
        let now = 2_000_000;
        assert_eq!(parse_since("2h", now).unwrap(), now - 2 * 3600);
        assert_eq!(parse_since("7d", now).unwrap(), now - 7 * 86400);
    }

    #[test]
    fn parse_since_bare_epoch() {
        let now = 2_000_000;
        assert_eq!(parse_since("1700000000", now).unwrap(), 1_700_000_000);
    }

    #[test]
    fn parse_since_rejects_bad_unit() {
        assert!(parse_since("5x", 0).is_err());
        assert!(parse_since("", 0).is_err());
        assert!(parse_since("h", 0).is_err());
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(0), "0s ago");
        assert_eq!(format_age(45), "45s ago");
        assert_eq!(format_age(120), "2m ago");
        assert_eq!(format_age(3700), "1.0h ago");
        assert_eq!(format_age(90_000), "1d ago");
    }

    #[test]
    fn esc_doubles_single_quotes() {
        assert_eq!(esc("o'reilly"), "o''reilly");
        assert_eq!(esc("plain"), "plain");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("longerstring", 5), "long…");
    }
}
