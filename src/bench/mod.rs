//! Benchmark module — generates and executes benchmark plans as a library.
//!
//! Replaces the former subprocess delegation to the standalone `knishio-bench`
//! binary. All molecule generation, HTTP injection, report generation, and
//! latency plotting are now performed in-process.

pub mod execute;
pub mod generate;
pub mod plot;

use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::{cell, output};

// ═══════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════

/// Cell slug used inside the SQLite plan file during generation.
/// At execution time this is replaced with the actual target cell slug.
pub const FIXTURE_CELL_SLUG: &str = "FIXTURE";

/// Prefix for auto-generated token slugs.
pub const BENCH_TOKEN_PREFIX: &str = "BENCH";

/// Diverse metaType pool for realistic bonding patterns.
/// Exercises M_TIER1 (same-type M->M), M_TIER2 (M->C same-type), and
/// M_TIER3 (MetaType class fallback) bonding tiers.
pub const BENCH_META_TYPES: &[&str] = &[
    "userProfile",
    "assetMetadata",
    "deviceTelemetry",
    "accessPolicy",
    "contentIndex",
];

/// Rule metaType targets — rules reference the meta types they govern.
pub const BENCH_RULE_TARGETS: &[&str] = &[
    "userProfile",
    "assetMetadata",
    "accessPolicy",
];

/// Number of distinct metaIds per metaType for realistic DAG branching.
pub const META_IDS_PER_TYPE: usize = 3;

// ═══════════════════════════════════════════════════════════════
// Public API
// ═══════════════════════════════════════════════════════════════

/// Generate a benchmark plan (pre-signed molecules) into a SQLite file.
pub fn generate(args: generate::GenerateArgs) -> Result<()> {
    generate::generate(args)
}

/// Execute a benchmark plan against validator endpoint(s).
/// Automatically creates the target cell before injection and
/// purges it afterward unless `keep` is set.
pub async fn execute(
    args: execute::ExecuteArgs,
    config: &Config,
    keep: bool,
) -> Result<()> {
    // Resolve cell slug — always in the BENCH_CLI_ namespace for safety.
    let slug = resolve_cell_slug(args.cell_slug.as_deref())?;

    // Ensure cell exists on the validator before injection
    cell::create(config, &slug, Some("Benchmark Cell"), "active").await?;

    output::info(&format!("Executing benchmark plan: {}", args.plan));
    let exec_args = execute::ExecuteArgs {
        plan: args.plan,
        endpoint: args.endpoint,
        endpoints: args.endpoints,
        strategy: args.strategy,
        concurrency: args.concurrency,
        cell_slug: Some(slug.clone()),
        csv: args.csv,
        plot: args.plot,
        insecure_tls: args.insecure_tls,
    };

    execute::execute(exec_args).await?;

    // Auto-cleanup benchmark data (reports already saved to disk)
    if !keep {
        output::info(&format!("Cleaning up benchmark cell '{}'...", slug));
        cell::purge(config, &slug).await?;
    }

    Ok(())
}

/// Convenience: generate a temp plan, execute it, then clean up.
pub async fn run(
    gen_args: generate::GenerateArgs,
    exec_args: execute::ExecuteArgs,
    config: &Config,
    keep: bool,
) -> Result<()> {
    let plan_path = format!("bench-plan-{}.db", std::process::id());

    // Generate the plan into a temp file
    let gen = generate::GenerateArgs {
        identities: gen_args.identities,
        types: gen_args.types,
        metas_per_identity: gen_args.metas_per_identity,
        transfers_per_identity: gen_args.transfers_per_identity,
        rules_per_identity: gen_args.rules_per_identity,
        burns_per_identity: gen_args.burns_per_identity,
        token_amount: gen_args.token_amount,
        output: plan_path.clone(),
    };
    generate::generate(gen)?;

    // Execute the plan
    let exec = execute::ExecuteArgs {
        plan: plan_path.clone(),
        endpoint: exec_args.endpoint,
        endpoints: exec_args.endpoints,
        strategy: exec_args.strategy,
        concurrency: exec_args.concurrency,
        cell_slug: exec_args.cell_slug,
        csv: exec_args.csv,
        plot: exec_args.plot,
        insecure_tls: exec_args.insecure_tls,
    };
    execute(exec, config, keep).await?;

    // Clean up temp plan file
    let _ = std::fs::remove_file(&plan_path);

    Ok(())
}

/// Purge benchmark data for a specific cell or all BENCH_CLI_* cells.
pub async fn clean(config: &Config, cell_slug: Option<&str>, all: bool) -> Result<()> {
    if let Some(slug) = cell_slug {
        cell::purge(config, slug).await?;
    } else if all {
        let slugs = cell::list_bench_slugs(config).await?;
        if slugs.is_empty() {
            output::info("No active benchmark cells found");
            return Ok(());
        }
        output::info(&format!("Purging {} benchmark cell(s)...", slugs.len()));
        for slug in &slugs {
            cell::purge(config, slug).await?;
        }
    } else {
        output::error("Specify --cell-slug <SLUG> or --all");
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Internal helpers
// ═══════════════════════════════════════════════════════════════

/// Resolve a cell slug into the BENCH_CLI_ namespace.
fn resolve_cell_slug(slug: Option<&str>) -> Result<String> {
    match slug {
        Some(s) if s.starts_with(cell::BENCH_PREFIX) => Ok(s.to_string()),
        Some(s) => Ok(format!("{}{s}", cell::BENCH_PREFIX)),
        None => {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("System clock before UNIX epoch")?
                .as_secs();
            Ok(format!("BENCH_CLI_{ts}"))
        }
    }
}
