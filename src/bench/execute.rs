//! Benchmark plan execution — reads a SQLite plan and injects molecules
//! into validator endpoint(s) via GraphQL, measuring latency and throughput.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::Connection;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::plot;

// ═══════════════════════════════════════════════════════════════
// Public Args & Types
// ═══════════════════════════════════════════════════════════════

/// Arguments for the `execute` subcommand.
pub struct ExecuteArgs {
    pub plan: String,
    pub endpoint: Option<String>,
    pub endpoints: Option<Vec<String>>,
    pub strategy: Strategy,
    pub concurrency: usize,
    pub cell_slug: Option<String>,
    pub csv: Option<String>,
    pub plot: Option<String>,
    pub insecure_tls: bool,
}

#[derive(Clone)]
pub enum Strategy {
    RoundRobin,
    Random,
}

// ═══════════════════════════════════════════════════════════════
// Internal Types
// ═══════════════════════════════════════════════════════════════

pub(crate) struct ExecResult {
    pub mol_type: String,
    pub phase: i32,
    pub endpoint: String,
    pub http_status: u16,
    pub validator_status: String,
    pub reason: Option<String>,
    pub latency_ms: u64,
    pub dag_index: usize,
}

struct Stats {
    count: usize,
    accepted: usize,
    rejected: usize,
    errors: usize,
    latencies: Vec<u64>,
    total_ms: u64,
}

impl Stats {
    fn new() -> Self {
        Stats {
            count: 0,
            accepted: 0,
            rejected: 0,
            errors: 0,
            latencies: Vec::new(),
            total_ms: 0,
        }
    }

    fn record(&mut self, result: &ExecResult) {
        self.count += 1;
        self.latencies.push(result.latency_ms);
        match result.validator_status.as_str() {
            "accepted" => self.accepted += 1,
            "rejected" => self.rejected += 1,
            _ => self.errors += 1,
        }
    }

    fn percentile(&self, p: f64) -> u64 {
        if self.latencies.is_empty() {
            return 0;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort();
        let idx = ((sorted.len() as f64 * p / 100.0) as usize).min(sorted.len() - 1);
        sorted[idx]
    }

    fn min(&self) -> u64 {
        self.latencies.iter().copied().min().unwrap_or(0)
    }

    fn max(&self) -> u64 {
        self.latencies.iter().copied().max().unwrap_or(0)
    }

    fn avg(&self) -> f64 {
        if self.latencies.is_empty() {
            return 0.0;
        }
        self.latencies.iter().sum::<u64>() as f64 / self.latencies.len() as f64
    }

    fn throughput(&self) -> f64 {
        if self.total_ms == 0 {
            return 0.0;
        }
        self.count as f64 * 1000.0 / self.total_ms as f64
    }

    /// Throughput based on sum of individual latencies (for endpoint-level stats
    /// where wall-clock total_ms isn't tracked).
    fn throughput_from_latencies(&self) -> f64 {
        let sum: u64 = self.latencies.iter().sum();
        if sum == 0 {
            return 0.0;
        }
        self.count as f64 * 1000.0 / sum as f64
    }

    fn print_latency_line(&self) {
        println!(
            "   Latency:   min={}ms  avg={:.0}ms  p50={}ms  p95={}ms  p99={}ms  max={}ms",
            self.min(),
            self.avg(),
            self.percentile(50.0),
            self.percentile(95.0),
            self.percentile(99.0),
            self.max()
        );
    }
}

// ═══════════════════════════════════════════════════════════════
// HTTP Injection
// ═══════════════════════════════════════════════════════════════

async fn inject_molecule(
    client: &reqwest::Client,
    endpoint: &str,
    mol_json: serde_json::Value,
    mol_type: String,
    phase: i32,
) -> ExecResult {
    let gql_url = format!("{endpoint}/graphql");

    let query = serde_json::json!({
        "query": "mutation ProposeMolecule($molecule: MoleculeInput!) { ProposeMolecule(molecule: $molecule) { status molecularHash reason payload } }",
        "variables": { "molecule": mol_json }
    });

    let start = Instant::now();
    let resp = client.post(&gql_url).json(&query).send().await;
    let latency_ms = start.elapsed().as_millis() as u64;

    match resp {
        Ok(response) => {
            let http_status = response.status().as_u16();
            let body: serde_json::Value = response.json().await.unwrap_or_default();

            let status = body
                .pointer("/data/ProposeMolecule/status")
                .and_then(|v| v.as_str())
                .unwrap_or("error")
                .to_string();
            let reason = body
                .pointer("/data/ProposeMolecule/reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    body.pointer("/errors/0/message")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });

            ExecResult {
                mol_type,
                phase,
                endpoint: endpoint.to_string(),
                http_status,
                validator_status: status,
                reason,
                latency_ms,
                dag_index: 0,
            }
        }
        Err(e) => ExecResult {
            mol_type,
            phase,
            endpoint: endpoint.to_string(),
            http_status: 0,
            validator_status: "error".to_string(),
            reason: {
                let mut msg = e.to_string();
                let mut source = std::error::Error::source(&e);
                while let Some(cause) = source {
                    msg.push_str(": ");
                    msg.push_str(&cause.to_string());
                    source = std::error::Error::source(cause);
                }
                Some(msg)
            },
            latency_ms,
            dag_index: 0,
        },
    }
}

fn select_endpoint<'a>(endpoints: &'a [String], strategy: &Strategy, idx: usize) -> &'a str {
    match strategy {
        Strategy::RoundRobin => &endpoints[idx % endpoints.len()],
        Strategy::Random => {
            use rand::Rng;
            let i = rand::thread_rng().gen_range(0..endpoints.len());
            &endpoints[i]
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// Execute Command
// ═══════════════════════════════════════════════════════════════

pub async fn execute(args: ExecuteArgs) -> Result<()> {
    // Resolve endpoints
    let endpoints: Vec<String> = if let Some(ref ep) = args.endpoint {
        vec![ep.clone()]
    } else if let Some(ref eps) = args.endpoints {
        eps.clone()
    } else {
        anyhow::bail!("Either --endpoint or --endpoints must be provided");
    };

    // Cell slug
    let cell_slug = args.cell_slug.unwrap_or_else(|| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("BENCH_CLI_{ts}")
    });

    // Open plan
    let conn = Connection::open(&args.plan)
        .with_context(|| format!("Failed to open plan file: {}", args.plan))?;

    // Read config
    let total_molecules: i64 = conn
        .query_row("SELECT COUNT(*) FROM molecules", [], |r| r.get(0))
        .context("Failed to count molecules in plan")?;

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!(" KnishIO Benchmark Executor");
    println!("═══════════════════════════════════════════════════════════════");
    println!(" Plan:              {}", args.plan);
    println!(" Molecules:         {total_molecules}");
    println!(" Endpoints:         {}", endpoints.join(", "));
    println!(
        " Strategy:          {:?}",
        match args.strategy {
            Strategy::RoundRobin => "round-robin",
            Strategy::Random => "random",
        }
    );
    println!(" Concurrency:       {}", args.concurrency);
    println!(" Cell slug:         {cell_slug}");
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    let mut client_builder =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if args.insecure_tls {
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }
    let client = client_builder
        .build()
        .context("Failed to build HTTP client")?;

    // Progress bar
    let pb = ProgressBar::new(total_molecules as u64);
    pb.set_style(
        ProgressStyle::with_template(
            " {spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({per_sec}) {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let mut all_results: Vec<ExecResult> = Vec::new();
    let mut phase_stats: HashMap<i32, Stats> = HashMap::new();
    let mut type_stats: HashMap<String, Stats> = HashMap::new();
    let mut endpoint_stats: HashMap<String, Stats> = HashMap::new();
    let mut rejected_details: Vec<(String, String, String)> = Vec::new();
    let mut error_details: Vec<(String, String, String, String, u16)> = Vec::new();
    let mut dag_accepted: usize = 0;

    // Phase 0 + Phase 1: sequential injection
    for phase in 0..=1 {
        let phase_name = if phase == 0 { "Auth" } else { "Setup" };
        let mut stmt = conn
            .prepare(
                "SELECT id, identity_idx, mol_type, molecular_hash, payload_json
                 FROM molecules WHERE phase = ?1 ORDER BY global_order ASC",
            )
            .context("Failed to prepare phase query")?;

        let rows: Vec<(i64, i64, String, String, String)> = stmt
            .query_map([phase], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .context("Failed to query phase rows")?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            continue;
        }

        pb.set_message(format!("Phase {phase} ({phase_name})"));
        let phase_start = Instant::now();

        for (idx, (_id, _identity_idx, mol_type, mol_hash, payload_json)) in
            rows.iter().enumerate()
        {
            let mut mol_json: serde_json::Value =
                serde_json::from_str(payload_json).context("Failed to parse payload JSON")?;
            mol_json["cellSlug"] = serde_json::json!(cell_slug);

            let ep = select_endpoint(&endpoints, &args.strategy, idx);
            let mut result =
                inject_molecule(&client, ep, mol_json, mol_type.clone(), phase).await;

            if result.validator_status == "accepted" {
                dag_accepted += 1;
            }
            result.dag_index = dag_accepted;

            if result.validator_status == "rejected" {
                rejected_details.push((
                    mol_hash.clone(),
                    mol_type.clone(),
                    result.reason.clone().unwrap_or_default(),
                ));
            } else if result.validator_status != "accepted" {
                error_details.push((
                    mol_hash.clone(),
                    mol_type.clone(),
                    result.reason.clone().unwrap_or_default(),
                    result.endpoint.clone(),
                    result.http_status,
                ));
            }

            phase_stats
                .entry(phase)
                .or_insert_with(Stats::new)
                .record(&result);
            type_stats
                .entry(mol_type.clone())
                .or_insert_with(Stats::new)
                .record(&result);
            endpoint_stats
                .entry(ep.to_string())
                .or_insert_with(Stats::new)
                .record(&result);

            all_results.push(result);
            pb.inc(1);
        }

        let phase_elapsed = phase_start.elapsed().as_millis() as u64;
        if let Some(stats) = phase_stats.get_mut(&phase) {
            stats.total_ms = phase_elapsed;
        }
    }

    // Phase 2: concurrent injection
    let mut stmt = conn
        .prepare(
            "SELECT id, identity_idx, mol_type, molecular_hash, payload_json
             FROM molecules WHERE phase = 2 ORDER BY chain_order ASC, identity_idx ASC",
        )
        .context("Failed to prepare phase 2 query")?;

    let phase2_rows: Vec<(i64, i64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .context("Failed to query phase 2 rows")?
        .filter_map(|r| r.ok())
        .collect();

    // Cap concurrency to identity count
    let num_identities: usize = conn
        .query_row(
            "SELECT COUNT(DISTINCT identity_idx) FROM molecules WHERE phase = 2",
            [],
            |row| row.get(0),
        )
        .unwrap_or(args.concurrency);
    let effective_concurrency = args.concurrency.min(num_identities);

    if effective_concurrency < args.concurrency {
        eprintln!(
            " Note: concurrency capped to {} (= identity count) to preserve ContinuID chain ordering",
            effective_concurrency
        );
    }

    if !phase2_rows.is_empty() {
        pb.set_message("Phase 2 (Test)");
        let phase2_start = Instant::now();

        for chunk in phase2_rows.chunks(effective_concurrency) {
            let mut futures = Vec::new();

            for (idx, (_id, _identity_idx, mol_type, mol_hash, payload_json)) in
                chunk.iter().enumerate()
            {
                let mut mol_json: serde_json::Value =
                    serde_json::from_str(payload_json).context("Failed to parse payload JSON")?;
                mol_json["cellSlug"] = serde_json::json!(cell_slug);

                let ep = select_endpoint(
                    &endpoints,
                    &args.strategy,
                    all_results.len() + idx,
                )
                .to_string();
                let client = client.clone();
                let mol_type = mol_type.clone();
                let mol_hash = mol_hash.clone();

                futures.push(async move {
                    let result = inject_molecule(&client, &ep, mol_json, mol_type, 2).await;
                    (result, mol_hash)
                });
            }

            let results: Vec<(ExecResult, String)> =
                futures_util::future::join_all(futures).await;

            for (mut result, mol_hash) in results {
                if result.validator_status == "accepted" {
                    dag_accepted += 1;
                }
                result.dag_index = dag_accepted;

                if result.validator_status == "rejected" {
                    rejected_details.push((
                        mol_hash,
                        result.mol_type.clone(),
                        result.reason.clone().unwrap_or_default(),
                    ));
                } else if result.validator_status != "accepted" {
                    error_details.push((
                        mol_hash,
                        result.mol_type.clone(),
                        result.reason.clone().unwrap_or_default(),
                        result.endpoint.clone(),
                        result.http_status,
                    ));
                }

                phase_stats
                    .entry(2)
                    .or_insert_with(Stats::new)
                    .record(&result);
                type_stats
                    .entry(result.mol_type.clone())
                    .or_insert_with(Stats::new)
                    .record(&result);
                endpoint_stats
                    .entry(result.endpoint.clone())
                    .or_insert_with(Stats::new)
                    .record(&result);

                all_results.push(result);
                pb.inc(1);
            }
        }

        let phase2_elapsed = phase2_start.elapsed().as_millis() as u64;
        if let Some(stats) = phase_stats.get_mut(&2) {
            stats.total_ms = phase2_elapsed;
        }
    }

    pb.finish_with_message("done");

    // ── Print execution report ──
    let overall_start_to_end: u64 = phase_stats.values().map(|s| s.total_ms).sum();

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!(" KnishIO Benchmark Execution Report");
    println!("═══════════════════════════════════════════════════════════════");
    println!(" Plan:              {}", args.plan);
    println!(" Endpoints:         {}", endpoints.join(", "));
    if effective_concurrency < args.concurrency {
        println!(
            " Concurrency:       {} (capped from {} to match identity count)",
            effective_concurrency, args.concurrency
        );
    } else {
        println!(" Concurrency:       {}", args.concurrency);
    }
    println!(" Cell slug:         {cell_slug}");
    println!();

    for (phase, label) in [
        (0, "Auth (sequential)"),
        (1, "Setup (sequential)"),
        (
            2,
            &format!("Test (concurrency={})", effective_concurrency),
        ),
    ] {
        if let Some(stats) = phase_stats.get(&phase) {
            println!(" Phase {phase} -- {label}");
            println!(
                "   Submitted: {}   Accepted: {}   Rejected: {}   Errors: {}",
                stats.count, stats.accepted, stats.rejected, stats.errors
            );
            stats.print_latency_line();
            println!("   Throughput: {:.1} mol/s", stats.throughput());
            println!();
        }
    }

    // By type breakdown
    if type_stats.len() > 1 {
        println!(" By molecule type:");
        println!(
            " {:<18} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
            "Type", "Count", "Avg", "P50", "P95", "P99", "Max"
        );
        println!(" {}", "-".repeat(72));
        for mol_type in &[
            "auth",
            "token-create",
            "token-request",
            "meta",
            "value-transfer",
            "rule",
            "burn",
        ] {
            if let Some(stats) = type_stats.get(*mol_type) {
                println!(
                    " {:<18} {:>6} {:>5}ms {:>5}ms {:>5}ms {:>5}ms {:>5}ms",
                    mol_type,
                    stats.count,
                    stats.avg() as u64,
                    stats.percentile(50.0),
                    stats.percentile(95.0),
                    stats.percentile(99.0),
                    stats.max()
                );
            }
        }
        println!();
    }

    // By endpoint breakdown (if multi-endpoint)
    if endpoint_stats.len() > 1 {
        println!(" By endpoint:");
        println!(
            " {:<30} {:>6} {:>6} {:>12}",
            "Endpoint", "Count", "Avg", "Throughput"
        );
        println!(" {}", "-".repeat(60));
        for (ep, stats) in &endpoint_stats {
            println!(
                " {:<30} {:>6} {:>5}ms {:>10.1} mol/s",
                ep,
                stats.count,
                stats.avg() as u64,
                stats.throughput_from_latencies()
            );
        }
        println!();
    }

    // Rejected molecules (first 20)
    if !rejected_details.is_empty() {
        println!(
            " Rejected molecules ({} total):",
            rejected_details.len()
        );
        for (hash, mol_type, reason) in rejected_details.iter().take(20) {
            let short_hash = if hash.len() > 12 {
                &hash[..12]
            } else {
                hash
            };
            println!("   hash={short_hash}... type={mol_type} reason=\"{reason}\"");
        }
        if rejected_details.len() > 20 {
            println!("   ... and {} more", rejected_details.len() - 20);
        }
        println!();
    }

    // Error molecules (first 20)
    if !error_details.is_empty() {
        println!(" Error molecules ({} total):", error_details.len());
        for (hash, mol_type, reason, endpoint, http_status) in error_details.iter().take(20) {
            let short_hash = if hash.len() > 12 {
                &hash[..12]
            } else {
                hash
            };
            println!("   hash={short_hash}... type={mol_type} endpoint={endpoint} http={http_status} reason=\"{reason}\"");
        }
        if error_details.len() > 20 {
            println!("   ... and {} more", error_details.len() - 20);
        }
        println!();
    }

    let total_accepted: usize = phase_stats.values().map(|s| s.accepted).sum();
    let total_rejected: usize = phase_stats.values().map(|s| s.rejected).sum();
    let total_errors: usize = phase_stats.values().map(|s| s.errors).sum();
    let total_count: usize = phase_stats.values().map(|s| s.count).sum();
    let overall_throughput = if overall_start_to_end > 0 {
        total_count as f64 * 1000.0 / overall_start_to_end as f64
    } else {
        0.0
    };

    if total_errors > 0 {
        println!(
            " Overall: {} molecules in {:.1}s ({:.1} mol/s) — {} accepted, {} rejected, {} errors",
            total_count,
            overall_start_to_end as f64 / 1000.0,
            overall_throughput,
            total_accepted,
            total_rejected,
            total_errors
        );
    } else {
        println!(
            " Overall: {} molecules in {:.1}s ({:.1} mol/s) — {} accepted, {} rejected",
            total_count,
            overall_start_to_end as f64 / 1000.0,
            overall_throughput,
            total_accepted,
            total_rejected
        );
    }
    println!("═══════════════════════════════════════════════════════════════");

    // Write JSON report
    let report_path = format!(
        "bench-report-{}.json",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );

    let json_report = serde_json::json!({
        "plan": args.plan,
        "endpoints": endpoints,
        "concurrency": args.concurrency,
        "cell_slug": cell_slug,
        "summary": {
            "total_molecules": total_count,
            "accepted": total_accepted,
            "rejected": total_rejected,
            "errors": total_errors,
            "total_time_ms": overall_start_to_end,
            "throughput_mol_per_sec": overall_throughput,
        },
        "phases": phase_stats.iter().map(|(phase, stats)| {
            (phase.to_string(), serde_json::json!({
                "count": stats.count,
                "accepted": stats.accepted,
                "rejected": stats.rejected,
                "errors": stats.errors,
                "total_ms": stats.total_ms,
                "throughput": stats.throughput(),
                "latency": {
                    "min": stats.min(),
                    "avg": stats.avg(),
                    "p50": stats.percentile(50.0),
                    "p95": stats.percentile(95.0),
                    "p99": stats.percentile(99.0),
                    "max": stats.max(),
                }
            }))
        }).collect::<serde_json::Map<String, serde_json::Value>>(),
        "by_type": type_stats.iter().map(|(mol_type, stats)| {
            (mol_type.clone(), serde_json::json!({
                "count": stats.count,
                "accepted": stats.accepted,
                "rejected": stats.rejected,
                "avg_ms": stats.avg(),
                "p50_ms": stats.percentile(50.0),
                "p95_ms": stats.percentile(95.0),
                "p99_ms": stats.percentile(99.0),
                "max_ms": stats.max(),
            }))
        }).collect::<serde_json::Map<String, serde_json::Value>>(),
        "rejected": rejected_details.iter().map(|(hash, mol_type, reason)| {
            serde_json::json!({
                "hash": hash,
                "type": mol_type,
                "reason": reason,
            })
        }).collect::<Vec<serde_json::Value>>(),
        "errors": error_details.iter().map(|(hash, mol_type, reason, endpoint, http_status)| {
            serde_json::json!({
                "hash": hash,
                "type": mol_type,
                "reason": reason,
                "endpoint": endpoint,
                "http_status": http_status,
            })
        }).collect::<Vec<serde_json::Value>>(),
    });

    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&json_report)
            .context("Failed to serialize JSON report")?,
    )
    .with_context(|| format!("Failed to write JSON report to {report_path}"))?;

    println!();
    println!(" Report written to: {report_path}");

    // Write per-molecule latency CSV
    let csv_path = args.csv.unwrap_or_else(|| {
        report_path
            .replace("bench-report-", "bench-latency-")
            .replace(".json", ".csv")
    });
    let mut csv = String::from("dag_index,latency_ms,mol_type,phase,status\n");
    for r in &all_results {
        csv.push_str(&format!(
            "{},{},{},{},{}\n",
            r.dag_index, r.latency_ms, r.mol_type, r.phase, r.validator_status
        ));
    }
    std::fs::write(&csv_path, &csv)
        .with_context(|| format!("Failed to write latency CSV to {csv_path}"))?;
    println!(" Latency CSV written to: {csv_path}");

    // Render latency plot PNG
    let plot_path = args
        .plot
        .unwrap_or_else(|| csv_path.replace(".csv", "-plot.png"));
    match plot::render_latency_plot(&all_results, &plot_path) {
        Ok(()) => println!(" Latency plot written to: {plot_path}"),
        Err(e) => eprintln!(" Warning: failed to render plot: {e}"),
    }

    Ok(())
}
