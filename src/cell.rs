//! Cell management via `docker exec psql`.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::config::Config;
use crate::output;

// ── Input Validation ────────────────────────────────────────

const SLUG_MAX_LEN: usize = 64;
const NAME_MAX_LEN: usize = 256;
const BUNDLE_LEN: usize = 64;
const VALID_STATUSES: &[&str] = &["active", "paused", "archived"];
pub const VALID_MODES: &[&str] = &["open", "permissioned", "private"];
pub const BENCH_PREFIX: &str = "BENCH_CLI_";

pub(crate) fn validate_slug(slug: &str) -> Result<()> {
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

fn validate_mode(mode: &str) -> Result<()> {
    if !VALID_MODES.contains(&mode) {
        anyhow::bail!(
            "Invalid mode '{}'. Must be one of: {}",
            mode,
            VALID_MODES.join(", ")
        );
    }
    Ok(())
}

pub(crate) fn validate_bundle(bundle: &str) -> Result<()> {
    if bundle.len() != BUNDLE_LEN {
        anyhow::bail!(
            "Bundle hash must be exactly {} characters, got {}",
            BUNDLE_LEN,
            bundle.len()
        );
    }
    if !bundle.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        anyhow::bail!("Bundle hash must be lowercase hexadecimal (0-9, a-f)");
    }
    Ok(())
}

#[cfg(test)]
mod validator_tests {
    use super::*;

    #[test]
    fn bundle_must_be_64_chars() {
        assert!(validate_bundle("").is_err());
        assert!(validate_bundle("9a24").is_err());
        assert!(validate_bundle(&"a".repeat(63)).is_err());
        assert!(validate_bundle(&"a".repeat(65)).is_err());
        assert!(validate_bundle(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn bundle_rejects_uppercase() {
        // Validator uses string equality; case mismatch silently breaks enforcement.
        assert!(validate_bundle("9A2411F2AF1A801A1E4262E74A743EB5EF6EF0DCCF3826851F7D861D51FD41D4").is_err());
    }

    #[test]
    fn bundle_rejects_non_hex() {
        let mut s = "a".repeat(63);
        s.push('z');
        assert!(validate_bundle(&s).is_err());
    }

    #[test]
    fn bundle_accepts_canonical() {
        assert!(validate_bundle("9a2411f2af1a801a1e4262e74a743eb5ef6ef0dccf3826851f7d861d51fd41d4").is_ok());
    }

    #[test]
    fn mode_only_accepts_three_values() {
        assert!(validate_mode("open").is_ok());
        assert!(validate_mode("permissioned").is_ok());
        assert!(validate_mode("private").is_ok());
        assert!(validate_mode("Open").is_err()); // case-sensitive
        assert!(validate_mode("public").is_err());
        assert!(validate_mode("").is_err());
    }
}


// ── Database Operations ─────────────────────────────────────

/// Execute a SQL statement inside the Postgres container.
///
/// `-q` suppresses command tags ("UPDATE N", "INSERT N") so callers parsing
/// `RETURNING` clauses see only the actual rows. Without `-q`, an UPDATE that
/// matches 0 rows would still emit "UPDATE 0" on stdout, defeating the
/// "empty output ⇒ no row matched" idiom that C-1 relies on.
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

/// Best-effort INSERT into the validator's `audit_events` table.
///
/// Mirrors the validator's own fire-and-forget audit emission pattern: any
/// failure (table missing on older deployments, JSON malformed, etc.) is
/// silently ignored so it never causes a successful operator command to
/// appear failed. The validator itself uses `tokio::spawn` + `warn!()` for
/// the same reason.
///
/// Inputs (slug + bundle hashes) are pre-validated by callers, so SQL
/// quote-escaping is belt-and-suspenders. Mode strings come from the
/// whitelist (`open|permissioned|private`) and list keys (`authorized_bundles`
/// or `admin_bundles`) are constants.
async fn emit_audit_event(
    config: &Config,
    action: &str,
    slug: &str,
    details: serde_json::Value,
) {
    let escaped_slug = slug.replace('\'', "''");
    let escaped_action = action.replace('\'', "''");
    let details_json = serde_json::to_string(&details)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('\'', "''");
    let sql = format!(
        "INSERT INTO audit_events \
         (category, action, target_type, target_id, details, cell_slug, severity) \
         VALUES ('config', '{escaped_action}', 'cell', '{escaped_slug}', \
                 '{details_json}'::jsonb, '{escaped_slug}', 'info')"
    );
    let _ = psql(config, &sql).await;
}

// ── ABAC Permission Management (WP-017) ─────────────────────
//
// Cell config JSONB shape under `config->'access'`:
//   {
//     "mode": "open" | "permissioned" | "private",
//     "authorized_bundles": [<bundle_hash>, ...],
//     "admin_bundles":      [<bundle_hash>, ...]
//   }
//
// Enforcement read path: validator's
// `servers/knishio-validator-rust/src/db/repositories/cell.rs` `check_access()`.
// Reject reasons formatted there must match exactly to keep audit captures stable.

/// Show full cell record with parsed access state.
pub async fn show(config: &Config, slug: &str) -> Result<()> {
    validate_slug(slug)?;
    let escaped = slug.replace('\'', "''");

    let sql = format!(
        "SELECT \
             slug, \
             name, \
             status, \
             created_at, \
             COALESCE(config->'access'->>'mode', 'open'), \
             COALESCE(jsonb_array_length(config->'access'->'authorized_bundles'), 0), \
             COALESCE(jsonb_array_length(config->'access'->'admin_bundles'), 0), \
             COALESCE(config->'access'->'authorized_bundles', '[]'::jsonb), \
             COALESCE(config->'access'->'admin_bundles', '[]'::jsonb) \
         FROM cells WHERE slug = '{escaped}'"
    );
    let result = psql(config, &sql).await?;
    if result.is_empty() {
        anyhow::bail!("Cell '{}' not found", slug);
    }

    // psql -t -A delimits with `|`; JSONB arrays can contain commas but no pipes,
    // so a 9-way split is safe here.
    let parts: Vec<&str> = result.splitn(9, '|').collect();
    if parts.len() < 9 {
        anyhow::bail!("Unexpected psql output shape (expected 9 fields, got {})", parts.len());
    }

    output::header(&format!("Cell: {}", parts[0]));
    println!("  Name:    {}", parts[1]);
    println!("  Status:  {}", parts[2]);
    println!("  Created: {}", parts[3]);
    println!("  Access:");
    println!("    Mode: {}", parts[4]);
    println!("    Authorized bundles: {}", parts[5]);
    print_jsonb_list(parts[7], "      ");
    println!("    Admin bundles: {}", parts[6]);
    print_jsonb_list(parts[8], "      ");
    Ok(())
}

/// Pretty-print a JSONB string array on its own indented lines.
fn print_jsonb_list(raw: &str, indent: &str) {
    // Strip outer brackets and split on commas; tolerate empty arrays.
    let trimmed = raw.trim_start_matches('[').trim_end_matches(']').trim();
    if trimmed.is_empty() {
        return;
    }
    for entry in trimmed.split(',') {
        let cleaned = entry.trim().trim_matches('"');
        if !cleaned.is_empty() {
            println!("{indent}{cleaned}");
        }
    }
}

/// Set the cell's ABAC mode. Initializes empty `authorized_bundles` and
/// `admin_bundles` arrays if missing, so subsequent grant/admin commands
/// never see a NULL JSONB key.
pub async fn set_mode(config: &Config, slug: &str, mode: &str) -> Result<()> {
    validate_slug(slug)?;
    validate_mode(mode)?;

    let escaped_slug = slug.replace('\'', "''");
    let escaped_mode = mode.replace('\'', "''");
    // jsonb_set's create_missing flag only affects the final path key — it
    // does NOT auto-create intermediate objects. Replacing the whole `access`
    // key with a freshly built object sidesteps that quirk and preserves any
    // existing bundles via COALESCE on the source row.
    //
    // C-1: RETURNING slug folds the existence check into the same statement,
    // dropping a second DB round-trip + closing the TOCTOU window.
    let sql = format!(
        "UPDATE cells SET config = jsonb_set( \
             COALESCE(config, '{{}}'::jsonb), \
             '{{access}}', \
             jsonb_build_object( \
                 'mode', '{escaped_mode}'::text, \
                 'authorized_bundles', COALESCE(config->'access'->'authorized_bundles', '[]'::jsonb), \
                 'admin_bundles', COALESCE(config->'access'->'admin_bundles', '[]'::jsonb), \
                 'allow_guest', COALESCE(config->'access'->'allow_guest', to_jsonb('{escaped_mode}' = 'open')) \
             ) \
         ) WHERE slug = '{escaped_slug}' RETURNING slug"
    );
    let returned = psql(config, &sql).await?;
    if returned.trim().is_empty() {
        anyhow::bail!("Cell '{}' not found", slug);
    }
    emit_audit_event(config, "cell_set_mode", slug, serde_json::json!({"mode": mode})).await;
    output::success(&format!("Cell '{}' → mode={}", slug, mode));

    // Post-condition warning: tightening to permissioned/private with empty list
    // bricks the cell until a grant is issued.
    if mode == "permissioned" || mode == "private" {
        let key = if mode == "permissioned" { "authorized_bundles" } else { "admin_bundles" };
        let count_sql = format!(
            "SELECT COALESCE(jsonb_array_length(config->'access'->'{key}'), 0) \
             FROM cells WHERE slug = '{escaped_slug}'"
        );
        let count = psql(config, &count_sql).await?;
        if count.trim() == "0" {
            output::warn(&format!(
                "{} list is empty — cell will reject all molecules until a bundle is added",
                key
            ));
        }
    }
    Ok(())
}

/// Set the cell's guest-auth policy (SEC-010 / Batch AG): whether the cell permits
/// GUEST (anonymous) auth-token issuance + guest reads. Decoupled from `mode` — a
/// `permissioned` cell may still allow anonymous reads. Merges into the existing
/// `access` object so mode/authorized_bundles/admin_bundles are preserved.
pub async fn set_allow_guest(config: &Config, slug: &str, allow: bool) -> Result<()> {
    validate_slug(slug)?;
    let escaped_slug = slug.replace('\'', "''");
    // `||` merges allow_guest into the existing access object (preserving its other
    // keys) and creates it when absent — jsonb_set's create_missing only affects the
    // final path key, not intermediate objects. RETURNING folds the existence check in.
    let sql = format!(
        "UPDATE cells SET config = jsonb_set( \
             COALESCE(config, '{{}}'::jsonb), \
             '{{access}}', \
             COALESCE(config->'access', '{{}}'::jsonb) || jsonb_build_object('allow_guest', {allow}) \
         ) WHERE slug = '{escaped_slug}' RETURNING slug"
    );
    let returned = psql(config, &sql).await?;
    if returned.trim().is_empty() {
        anyhow::bail!("Cell '{}' not found", slug);
    }
    emit_audit_event(config, "cell_set_allow_guest", slug, serde_json::json!({"allow_guest": allow})).await;
    output::success(&format!("Cell '{}' → allow_guest={}", slug, allow));
    Ok(())
}

/// Add `bundle` to `config->'access'->'authorized_bundles'` (idempotent).
pub async fn grant(config: &Config, slug: &str, bundle: &str) -> Result<()> {
    add_to_access_list(config, slug, "authorized_bundles", bundle).await?;
    output::success(&format!("Cell '{}' authorized: {}", slug, bundle));
    Ok(())
}

/// Remove `bundle` from `config->'access'->'authorized_bundles'`.
pub async fn revoke(config: &Config, slug: &str, bundle: &str) -> Result<()> {
    let removed = remove_from_access_list(config, slug, "authorized_bundles", bundle).await?;
    if removed {
        output::success(&format!("Cell '{}' revoked: {}", slug, bundle));
    } else {
        output::info(&format!("Bundle was not in cell '{}' authorized list; no change", slug));
    }
    Ok(())
}

/// Add `bundle` to `config->'access'->'admin_bundles'` (idempotent).
pub async fn add_admin(config: &Config, slug: &str, bundle: &str) -> Result<()> {
    add_to_access_list(config, slug, "admin_bundles", bundle).await?;
    output::success(&format!("Cell '{}' admin added: {}", slug, bundle));
    Ok(())
}

/// Bulk-grant from a file. Pre-validates ALL bundles before applying any
/// (fail-fast: a malformed bundle on line 47 of a 50-line file aborts the
/// whole batch instead of half-onboarding the cell).
pub async fn grant_from_file(
    config: &Config,
    slug: &str,
    path: &std::path::Path,
) -> Result<()> {
    bulk_apply(config, slug, path, "authorized_bundles").await
}

/// Bulk add admins from a file. Same fail-fast semantics as `grant_from_file`.
pub async fn add_admin_from_file(
    config: &Config,
    slug: &str,
    path: &std::path::Path,
) -> Result<()> {
    bulk_apply(config, slug, path, "admin_bundles").await
}

/// Shared bulk-import path. Reads the file once, validates every line, then
/// applies in order. Each grant is idempotent so a partial-then-resumed run
/// is safe (already-granted bundles are no-ops).
async fn bulk_apply(
    config: &Config,
    slug: &str,
    path: &std::path::Path,
    key: &str,
) -> Result<()> {
    debug_assert!(key == "authorized_bundles" || key == "admin_bundles");

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let bundles: Vec<&str> = contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    if bundles.is_empty() {
        anyhow::bail!("No bundles found in {} (empty or all comments)", path.display());
    }

    // Pre-validate everything before any DB write.
    for (i, bundle) in bundles.iter().enumerate() {
        validate_bundle(bundle)
            .with_context(|| format!("Line {}: invalid bundle '{}'", i + 1, bundle))?;
    }

    let what = if key == "authorized_bundles" { "bundle(s) authorized" } else { "admin(s) added" };
    output::info(&format!("Importing {} from {}...", bundles.len(), path.display()));
    for bundle in &bundles {
        add_to_access_list(config, slug, key, bundle).await?;
    }
    output::success(&format!("Cell '{}': {} {}", slug, bundles.len(), what));
    Ok(())
}

/// Remove `bundle` from `config->'access'->'admin_bundles'`.
pub async fn remove_admin(config: &Config, slug: &str, bundle: &str) -> Result<()> {
    let removed = remove_from_access_list(config, slug, "admin_bundles", bundle).await?;
    if removed {
        output::success(&format!("Cell '{}' admin removed: {}", slug, bundle));
    } else {
        output::info(&format!("Bundle was not in cell '{}' admin list; no change", slug));
    }
    Ok(())
}

/// Append `bundle` to the named JSONB string array under `config->'access'`,
/// deduplicating against existing entries. `key` must be a static, vetted
/// string (callers pass `"authorized_bundles"` or `"admin_bundles"`).
///
/// Replaces the whole `access` object atomically (preserving mode and the
/// untouched list) — this is the only safe pattern because PostgreSQL's
/// `jsonb_set` does not auto-create intermediate path keys.
async fn add_to_access_list(
    config: &Config,
    slug: &str,
    key: &str,
    bundle: &str,
) -> Result<()> {
    validate_slug(slug)?;
    validate_bundle(bundle)?;
    debug_assert!(key == "authorized_bundles" || key == "admin_bundles");

    let escaped_slug = slug.replace('\'', "''");
    let other_key = if key == "authorized_bundles" { "admin_bundles" } else { "authorized_bundles" };
    // bundle was just validated: 64 lowercase hex chars, no quotes possible.
    let merged_array = format!(
        "( \
             SELECT to_jsonb(array_agg(DISTINCT b)) \
             FROM ( \
                 SELECT jsonb_array_elements_text( \
                     COALESCE(config->'access'->'{key}', '[]'::jsonb) \
                 ) AS b \
                 UNION \
                 SELECT '{bundle}' AS b \
             ) merged \
         )"
    );
    // C-1: RETURNING slug folds the existence check into the UPDATE, closing
    // the TOCTOU window that a separate cell_exists() pre-check would leave open.
    let sql = format!(
        "UPDATE cells SET config = jsonb_set( \
             COALESCE(config, '{{}}'::jsonb), \
             '{{access}}', \
             jsonb_build_object( \
                 'mode', COALESCE(config->'access'->>'mode', 'open'), \
                 '{key}', {merged_array}, \
                 '{other_key}', COALESCE(config->'access'->'{other_key}', '[]'::jsonb) \
             ) \
         ) WHERE slug = '{escaped_slug}' RETURNING slug"
    );
    let returned = psql(config, &sql).await?;
    if returned.trim().is_empty() {
        anyhow::bail!("Cell '{}' not found", slug);
    }
    let action = if key == "authorized_bundles" { "cell_grant" } else { "cell_add_admin" };
    emit_audit_event(
        config,
        action,
        slug,
        serde_json::json!({"bundle": bundle, "list": key}),
    ).await;
    Ok(())
}

/// Remove `bundle` from the named JSONB string array under `config->'access'`.
/// Returns `true` if the bundle was present and removed, `false` if absent.
async fn remove_from_access_list(
    config: &Config,
    slug: &str,
    key: &str,
    bundle: &str,
) -> Result<bool> {
    validate_slug(slug)?;
    validate_bundle(bundle)?;
    debug_assert!(key == "authorized_bundles" || key == "admin_bundles");

    let escaped_slug = slug.replace('\'', "''");
    let other_key = if key == "authorized_bundles" { "admin_bundles" } else { "authorized_bundles" };

    let filtered_array = format!(
        "COALESCE( \
             ( \
                 SELECT to_jsonb(array_agg(b)) \
                 FROM jsonb_array_elements_text( \
                     COALESCE(config->'access'->'{key}', '[]'::jsonb) \
                 ) AS b \
                 WHERE b != '{bundle}' \
             ), \
             '[]'::jsonb \
         )"
    );
    // C-1: single-statement CTE captures was_present AND existence in one round-trip.
    // Output shape (psql -t -A): "t|<slug>" | "f|<slug>" | "" (empty if cell missing).
    // Slug validation restricts to [A-Za-z0-9_-] so '|' can never appear in the slug.
    let sql = format!(
        "WITH before AS ( \
             SELECT slug, \
                    COALESCE(config->'access'->'{key}' @> to_jsonb('{bundle}'::text), false) AS was_present \
             FROM cells WHERE slug = '{escaped_slug}' \
         ), updated AS ( \
             UPDATE cells SET config = jsonb_set( \
                 COALESCE(config, '{{}}'::jsonb), \
                 '{{access}}', \
                 jsonb_build_object( \
                     'mode', COALESCE(config->'access'->>'mode', 'open'), \
                     '{key}', {filtered_array}, \
                     '{other_key}', COALESCE(config->'access'->'{other_key}', '[]'::jsonb) \
                 ) \
             ) WHERE slug = '{escaped_slug}' RETURNING slug \
         ) \
         SELECT (SELECT was_present FROM before)::text || '|' || COALESCE((SELECT slug FROM updated), '') \
         FROM before"
    );
    let returned = psql(config, &sql).await?;
    let line = returned.trim();
    // Cell not found: CTE `before` returns no rows, so the outer SELECT also empty.
    if line.is_empty() {
        anyhow::bail!("Cell '{}' not found", slug);
    }
    // PostgreSQL's `boolean::text` produces "true"/"false" (not "t"/"f").
    let (was_present_str, _updated_slug) = line.split_once('|').unwrap_or(("false", ""));
    let was_present = was_present_str == "true";
    if was_present {
        let action = if key == "authorized_bundles" { "cell_revoke" } else { "cell_remove_admin" };
        emit_audit_event(
            config,
            action,
            slug,
            serde_json::json!({"bundle": bundle, "list": key}),
        ).await;
    }
    Ok(was_present)
}
