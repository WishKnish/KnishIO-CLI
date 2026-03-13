//! Benchmark delegation — locates and runs the knishio-bench binary.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::output;

/// Run `knishio-bench generate` with the given args.
pub async fn generate(
    bench_bin: &Path,
    identities: usize,
    types: &[String],
    metas_per_identity: usize,
    transfers_per_identity: usize,
    rules_per_identity: usize,
    burns_per_identity: usize,
    token_amount: f64,
    output_path: &str,
) -> Result<()> {
    output::info("Generating benchmark plan...");
    let mut cmd = Command::new(bench_bin);
    cmd.arg("generate")
        .arg("--identities")
        .arg(identities.to_string())
        .arg("--types")
        .arg(types.join(","))
        .arg("--metas-per-identity")
        .arg(metas_per_identity.to_string())
        .arg("--transfers-per-identity")
        .arg(transfers_per_identity.to_string())
        .arg("--rules-per-identity")
        .arg(rules_per_identity.to_string())
        .arg("--burns-per-identity")
        .arg(burns_per_identity.to_string())
        .arg("--token-amount")
        .arg(token_amount.to_string())
        .arg("-o")
        .arg(output_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd.status().await.context("Failed to run knishio-bench")?;
    if status.success() {
        output::success(&format!("Plan written to {}", output_path));
    } else {
        output::error("Benchmark generation failed");
    }
    Ok(())
}

/// Run `knishio-bench execute` with the given args.
pub async fn execute(
    bench_bin: &Path,
    plan_path: &str,
    endpoint: &str,
    concurrency: usize,
    cell_slug: Option<&str>,
) -> Result<()> {
    output::info(&format!("Executing benchmark plan: {}", plan_path));
    let mut cmd = Command::new(bench_bin);
    cmd.arg("execute")
        .arg(plan_path)
        .arg("--endpoint")
        .arg(endpoint)
        .arg("--concurrency")
        .arg(concurrency.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some(slug) = cell_slug {
        cmd.arg("--cell-slug").arg(slug);
    }

    let status = cmd.status().await.context("Failed to run knishio-bench")?;
    if status.success() {
        output::success("Benchmark complete");
    } else {
        output::error("Benchmark execution failed");
    }
    Ok(())
}

/// Convenience: generate + execute in one shot.
pub async fn run(
    bench_bin: &Path,
    identities: usize,
    types: &[String],
    metas_per_identity: usize,
    transfers_per_identity: usize,
    rules_per_identity: usize,
    burns_per_identity: usize,
    token_amount: f64,
    endpoint: &str,
    concurrency: usize,
    cell_slug: Option<&str>,
) -> Result<()> {
    let plan_path = format!("bench-plan-{}.db", std::process::id());

    generate(
        bench_bin,
        identities,
        types,
        metas_per_identity,
        transfers_per_identity,
        rules_per_identity,
        burns_per_identity,
        token_amount,
        &plan_path,
    )
    .await?;

    execute(bench_bin, &plan_path, endpoint, concurrency, cell_slug).await?;

    // Clean up temp plan file
    let _ = std::fs::remove_file(&plan_path);

    Ok(())
}
