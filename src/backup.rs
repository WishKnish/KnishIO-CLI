//! `knishio backup` / `knishio restore` — database backup and restore.
//!
//! Goes through `PsqlTransport`, so these work on a Docker stack, a bare-metal host
//! (`sudo -u postgres`), or a remote server over ssh — one dispatch, shared with
//! cell/audit. Previously this module hardcoded `docker exec` at every site and kept
//! its own container check, which made `knishio backup` fail on a runbook host for the
//! same reason cell/audit did (CLI-8): the docker-only assumption was re-implemented
//! per surface rather than living in one place.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::config::Config;
use crate::output;
use crate::psql::PsqlTransport;

/// Create a compressed database backup.
///
/// Output path defaults to `./backups/knishio_YYYYMMDD_HHMMSS.sql.gz`.
pub async fn backup(t: &PsqlTransport, output_path: Option<&str>) -> Result<()> {
    // No container pre-check: transport resolution already failed loudly (naming every
    // mechanism it tried) if nothing could reach a database.

    // Determine output path
    let timestamp = chrono_timestamp();
    let default_path = format!("backups/knishio_{}.sql", timestamp);
    let dest = output_path.unwrap_or(&default_path);

    // Ensure parent directory exists
    if let Some(parent) = Path::new(dest).parent() {
        std::fs::create_dir_all(parent)
            .context("Failed to create backup directory")?;
    }

    output::info(&format!("Backing up database to {}...", dest));

    let dump = t.pg_dump().await?;

    std::fs::write(dest, &dump)
        .with_context(|| format!("Failed to write backup to {}", dest))?;

    let size_mb = dump.len() as f64 / (1024.0 * 1024.0);
    output::success(&format!("Backup complete: {} ({:.1} MB)", dest, size_mb));

    Ok(())
}

/// Restore a database from a backup file.
///
/// After restore, runs /db-check to verify consistency.
pub async fn restore(
    cfg: &Config,
    t: &PsqlTransport,
    backup_path: &str,
    skip_verify: bool,
) -> Result<()> {
    let db_name = t.db_name().to_string();

    // Verify backup file exists
    if !Path::new(backup_path).exists() {
        anyhow::bail!("Backup file not found: {}", backup_path);
    }

    output::warn(&format!("Restoring database from {}...", backup_path));
    output::warn("This will overwrite the current database contents.");

    // Read backup file
    let sql_content = std::fs::read(backup_path)
        .with_context(|| format!("Failed to read backup file: {}", backup_path))?;

    // Drop and recreate the database to ensure clean state
    output::info("Dropping and recreating database...");

    // Terminate existing connections
    let terminate_sql = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}' AND pid <> pg_backend_pid();",
        db_name
    );
    // All three against the `postgres` maintenance database — the target is being
    // dropped, so we cannot be connected to it.
    t.exec_on_db("postgres", &terminate_sql).await?;
    t.exec_on_db("postgres", &format!("DROP DATABASE IF EXISTS \"{}\";", db_name))
        .await?;
    t.exec_on_db("postgres", &format!("CREATE DATABASE \"{}\";", db_name))
        .await?;

    // Pipe the SQL dump into psql
    output::info("Restoring data...");
    let (program, args) = t.psql_stdin_argv(&db_name);
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start psql for restore")?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&sql_content).await
            .context("Failed to write backup data to psql")?;
        drop(stdin); // Close stdin to signal EOF
    }

    let restore_output = child.wait_with_output().await?;
    if !restore_output.status.success() {
        let stderr = String::from_utf8_lossy(&restore_output.stderr);
        // psql may emit warnings for things like "role already exists" — only fail on real errors
        if stderr.contains("FATAL") || stderr.contains("could not connect") {
            anyhow::bail!("Restore failed: {}", stderr.trim());
        }
    }

    output::success("Database restored successfully");

    // Verify via /db-check
    if !skip_verify {
        output::info("Verifying database consistency...");
        println!();
        crate::health::db_check(&cfg.validator.url, cfg.validator.insecure_tls).await?;
    }

    Ok(())
}

/// List available backup files in the backups directory.
pub async fn list() -> Result<()> {
    let backups_dir = Path::new("backups");
    if !backups_dir.exists() {
        output::info("No backups directory found");
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(backups_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "sql" || ext == "gz")
        })
        .collect();

    if entries.is_empty() {
        output::info("No backups found in backups/");
        return Ok(());
    }

    // Sort by modification time (newest first)
    entries.sort_by(|a, b| {
        b.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .cmp(
                &a.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
    });

    output::info(&format!("Found {} backup(s):", entries.len()));
    for entry in &entries {
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let size_str = if size > 1_048_576 {
            format!("{:.1} MB", size as f64 / 1_048_576.0)
        } else {
            format!("{:.0} KB", size as f64 / 1024.0)
        };
        println!("  {} ({})", entry.path().display(), size_str);
    }

    Ok(())
}

/// Generate a timestamp string for backup filenames.
fn chrono_timestamp() -> String {
    use std::time::SystemTime;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert to YYYYMMDD_HHMMSS without pulling in chrono
    let secs_per_day = 86400u64;
    let days = now / secs_per_day;
    let time_of_day = now % secs_per_day;

    // Compute year/month/day from days since epoch (simplified)
    let (year, month, day) = days_to_date(days);
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
