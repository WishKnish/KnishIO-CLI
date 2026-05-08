//! Integration tests for `knishio cell` ABAC subcommands.
//!
//! These tests exercise the live validator stack via the same `docker exec
//! psql` path the CLI uses internally. They are marked `#[ignore]` so the
//! default `cargo test` invocation stays fast (per project convention on
//! external-drive build environments). Run explicitly with:
//!
//!   cargo test --test cell_integration -- --ignored
//!
//! Each test creates a unique `INT_TEST_<nanos>_<n>` slug to avoid collisions
//! with other test runs and the live ABAC fixtures (test-perm/test-priv/etc.).
//! Tests clean up their own cells via direct DELETE.

use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const VALID_BUNDLE_A: &str =
    "9a2411f2af1a801a1e4262e74a743eb5ef6ef0dccf3826851f7d861d51fd41d4";
const VALID_BUNDLE_B: &str =
    "e5f9541360208766e921cba1fdd788e7ff9d8d4e30ce1691d0389f2b22c9917c";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_slug() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("INT_TEST_{nanos}_{n}")
}

fn knishio() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knishio"));
    cmd.arg("--insecure");
    cmd
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("knishio CLI execution failed");
    if !out.status.success() {
        eprintln!(
            "STDOUT: {}\nSTDERR: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out
}

fn cleanup(slug: &str) {
    let _ = knishio()
        .args(["psql", "-c"])
        .arg(format!("DELETE FROM cells WHERE slug = '{slug}';"))
        .output();
}

#[test]
#[ignore = "requires running validator stack"]
fn grant_then_show_reflects_bundle() {
    let slug = unique_slug();
    let create = run(knishio().args(["cell", "create", &slug, "--mode", "permissioned"]));
    assert!(create.status.success(), "cell create failed");

    let grant = run(knishio().args(["cell", "grant", &slug, VALID_BUNDLE_A]));
    assert!(grant.status.success(), "cell grant failed");

    let show = run(knishio().args(["cell", "show", &slug]));
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains(VALID_BUNDLE_A),
        "show output missing bundle: {stdout}"
    );
    assert!(
        stdout.contains("Authorized bundles: 1"),
        "show output didn't reflect grant: {stdout}"
    );

    cleanup(&slug);
}

#[test]
#[ignore = "requires running validator stack"]
fn double_grant_is_idempotent() {
    let slug = unique_slug();
    run(knishio().args(["cell", "create", &slug, "--mode", "permissioned"]));
    run(knishio().args(["cell", "grant", &slug, VALID_BUNDLE_A]));
    run(knishio().args(["cell", "grant", &slug, VALID_BUNDLE_A])); // duplicate

    let show = run(knishio().args(["cell", "show", &slug]));
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains("Authorized bundles: 1"),
        "double-grant produced != 1 entries: {stdout}"
    );

    cleanup(&slug);
}

#[test]
#[ignore = "requires running validator stack"]
fn revoke_absent_bundle_is_graceful_no_op() {
    let slug = unique_slug();
    run(knishio().args(["cell", "create", &slug, "--mode", "permissioned"]));

    // Revoke a bundle that was never granted.
    let revoke = run(knishio().args(["cell", "revoke", &slug, VALID_BUNDLE_A]));
    assert!(revoke.status.success(), "graceful no-op should exit 0");
    // output::info writes to stderr by convention (see src/output.rs).
    let stderr = String::from_utf8_lossy(&revoke.stderr);
    assert!(
        stderr.contains("no change"),
        "revoke-absent should print no-change message to stderr: {stderr}"
    );

    cleanup(&slug);
}

#[test]
#[ignore = "requires running validator stack"]
fn set_mode_persists_via_show() {
    // Regression test for the §C JSONB write bug: jsonb_set didn't auto-create
    // intermediate path objects, so set-mode silently no-op'd on fresh cells.
    let slug = unique_slug();
    run(knishio().args(["cell", "create", &slug])); // no mode flag → defaults to open
    run(knishio().args(["cell", "set-mode", &slug, "private"]));

    let show = run(knishio().args(["cell", "show", &slug]));
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains("Mode: private"),
        "set-mode didn't persist: {stdout}"
    );

    cleanup(&slug);
}

#[test]
#[ignore = "requires running validator stack"]
fn from_file_imports_multiple_bundles() {
    let slug = unique_slug();
    run(knishio().args(["cell", "create", &slug, "--mode", "permissioned"]));

    // Write a file with 2 valid bundles + a comment + a blank line.
    let path = std::env::temp_dir().join(format!("{slug}.txt"));
    let contents = format!(
        "# header comment\n{VALID_BUNDLE_A}\n\n# inline comment\n{VALID_BUNDLE_B}\n"
    );
    std::fs::write(&path, contents).unwrap();

    let import = run(knishio().args([
        "cell",
        "grant",
        &slug,
        "--from-file",
        path.to_str().unwrap(),
    ]));
    assert!(import.status.success(), "bulk import failed");

    let show = run(knishio().args(["cell", "show", &slug]));
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains("Authorized bundles: 2"),
        "expected 2 authorized after import: {stdout}"
    );
    assert!(stdout.contains(VALID_BUNDLE_A));
    assert!(stdout.contains(VALID_BUNDLE_B));

    let _ = std::fs::remove_file(&path);
    cleanup(&slug);
}

#[test]
#[ignore = "requires running validator stack"]
fn audit_event_emitted_on_grant() {
    let slug = unique_slug();
    run(knishio().args(["cell", "create", &slug, "--mode", "permissioned"]));
    run(knishio().args(["cell", "grant", &slug, VALID_BUNDLE_A]));

    // Query audit_events for our specific slug.
    let q = run(knishio().args(["psql", "-c"]).arg(format!(
        "SELECT action FROM audit_events WHERE category = 'config' AND target_id = '{slug}' ORDER BY id DESC LIMIT 1;"
    )));
    let stdout = String::from_utf8_lossy(&q.stdout);
    assert!(
        stdout.contains("cell_grant"),
        "audit_events row missing for cell_grant on {slug}: {stdout}"
    );

    cleanup(&slug);
}
