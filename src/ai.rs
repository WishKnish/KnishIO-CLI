//! Validator `/ai/status` wrapper.
//!
//! Introspection front-end for the DataBraid AI pipeline state — embedding
//! / generation provider info, backfill queue depth, query-embedding cache
//! hit ratio, compile-time acceleration label. Hits the validator over HTTP
//! using the same TLS-bypass pattern as `src/health.rs`.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;

use crate::config::Config;

// ── Response shape (mirrors src/ai_status.rs on the validator) ─────

#[derive(Debug, Deserialize)]
struct AiStatusResponse {
    embedding: EmbeddingStatus,
    generation: GenerationStatus,
    backfill: BackfillStatus,
    cache: CacheStatus,
    acceleration: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddingStatus {
    enabled: bool,
    provider: Option<String>,
    model: Option<String>,
    dimensions: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GenerationStatus {
    enabled: bool,
    provider: Option<String>,
    model: Option<String>,
    /// Sampling / context params currently in effect. Optional so this
    /// CLI stays compatible with older validator builds that don't
    /// expose the field.
    #[serde(default)]
    sampling: Option<SamplingParams>,
    /// Recent inference activity summary (latency + success/error
    /// counts). Optional: absent when generation is disabled or no
    /// calls have completed since startup.
    #[serde(default)]
    recent: Option<RecentInferenceStats>,
}

#[derive(Debug, Deserialize)]
struct SamplingParams {
    temperature: f32,
    top_p: f32,
    frequency_penalty: f32,
    presence_penalty: f32,
    max_tokens: u32,
    n_ctx: u32,
}

#[derive(Debug, Deserialize)]
struct RecentInferenceStats {
    window_secs: u64,
    count: usize,
    success_count: usize,
    error_count: usize,
    avg_latency_secs: f64,
    min_latency_secs: f64,
    max_latency_secs: f64,
}

#[derive(Debug, Deserialize)]
struct BackfillStatus {
    pending: Option<i64>,
    total_embedded: Option<i64>,
    total_metas: Option<i64>,
    active_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CacheStatus {
    hits: u64,
    misses: u64,
    entries: u64,
    hit_ratio: f64,
}

// ── Entry point ─────────────────────────────────────────────────────

/// `knishio ai status` — fetch `/ai/status` and pretty-print.
pub async fn status(cfg: &Config) -> Result<()> {
    let url = format!("{}/ai/status", cfg.validator.url.trim_end_matches('/'));

    let client = build_client(cfg.validator.insecure_tls)?;
    let resp = client.get(&url).send().await.map_err(friendly_net_error)?;
    let http_status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if http_status != 200 {
        anyhow::bail!("/ai/status returned HTTP {}: {}", http_status, body);
    }

    let parsed: AiStatusResponse = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse /ai/status response: {}", body))?;

    // R24 B: surface R20 Phase A nudge counters from the /metrics endpoint.
    // Best-effort: if /metrics fails or doesn't expose the counters yet, we
    // simply skip the section. Doesn't fail the overall status command.
    let nudges = fetch_nudge_metrics(&client, &cfg.validator.url).await;

    render(&parsed, &cfg.validator.url, &nudges);
    Ok(())
}

/// R24 B: minimal Prometheus text-format counter parse.
///
/// Each line is either a `# HELP …`/`# TYPE …` comment or `metric_name value`
/// (or `metric_name{labels} value`). We only need exact-name matches for
/// counters with no labels.
fn parse_prom_counter(body: &str, name: &str) -> Option<u64> {
    body.lines()
        .find(|line| line.starts_with(name) && !line.starts_with('#'))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|val| val.parse::<f64>().ok())
        .map(|val| val as u64)
}

#[derive(Debug, Default)]
struct NudgeMetrics {
    nudges_received: Option<u64>,
    fast_drain_wakes: Option<u64>,
}

async fn fetch_nudge_metrics(client: &reqwest::Client, base_url: &str) -> NudgeMetrics {
    let url = format!("{}/metrics", base_url.trim_end_matches('/'));
    let body = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
        _ => return NudgeMetrics::default(),
    };
    NudgeMetrics {
        nudges_received: parse_prom_counter(&body, "knishio_embedding_nudges_received_total"),
        fast_drain_wakes: parse_prom_counter(&body, "knishio_embedding_fast_drain_wakes_total"),
    }
}

// ── Rendering ───────────────────────────────────────────────────────

fn render(s: &AiStatusResponse, base_url: &str, nudges: &NudgeMetrics) {
    println!();
    println!(
        "{} {}",
        "AI Pipeline Status".bold().underline(),
        format!("({})", base_url).dimmed()
    );
    println!();

    // Embedding
    section("Embedding");
    match (
        &s.embedding.enabled,
        &s.embedding.provider,
        &s.embedding.model,
        &s.embedding.dimensions,
    ) {
        (true, provider, model, dim) => {
            row_check("enabled", provider.as_deref().unwrap_or(""), true);
            row("model", model.as_deref().unwrap_or("—"));
            row(
                "dimensions",
                dim.map(|d| d.to_string())
                    .unwrap_or_else(|| "unknown".into())
                    .as_str(),
            );
        }
        (false, _, _, _) => {
            row_check("disabled", "EMBEDDING_ENABLED=false", false);
        }
    }
    println!();

    // Generation
    section("Generation");
    match (&s.generation.enabled, &s.generation.provider, &s.generation.model) {
        (true, provider, model) => {
            row_check("enabled", provider.as_deref().unwrap_or(""), true);
            row("model", model.as_deref().unwrap_or("—"));
            if let Some(sp) = &s.generation.sampling {
                row(
                    "sampling",
                    &format!(
                        "temp={:.2} top_p={:.2} freq={:.2} pres={:.2}",
                        sp.temperature, sp.top_p, sp.frequency_penalty, sp.presence_penalty
                    ),
                );
                row(
                    "tokens",
                    &format!("max={} n_ctx={}", sp.max_tokens, sp.n_ctx),
                );
            }
            if let Some(r) = &s.generation.recent {
                let err_label = if r.error_count == 0 {
                    format!("{} errors", r.error_count).green().to_string()
                } else {
                    format!("{} errors", r.error_count).yellow().to_string()
                };
                row(
                    "recent",
                    &format!(
                        "{} calls over {}s · {} · avg {:.1}s · min {:.1}s · max {:.1}s",
                        r.count,
                        r.window_secs,
                        err_label,
                        r.avg_latency_secs,
                        r.min_latency_secs,
                        r.max_latency_secs
                    ),
                );
            }
        }
        (false, _, _) => {
            row_check("disabled", "GENERATION_ENABLED=false", false);
        }
    }
    println!();

    // Backfill
    section("Backfill");
    match (
        s.backfill.pending,
        s.backfill.total_embedded,
        s.backfill.total_metas,
        &s.backfill.active_model,
    ) {
        (Some(pending), Some(embedded), Some(total), Some(model)) => {
            if total == 0 {
                row_info("rows", "no metas yet");
            } else {
                let ratio = (embedded as f64) / (total as f64) * 100.0;
                let pending_label = if pending == 0 {
                    format!("{} / {}", fmt_int(embedded), fmt_int(total))
                        .green()
                        .to_string()
                } else {
                    format!(
                        "{} pending · {} / {}  ({:.1} %)",
                        fmt_int(pending),
                        fmt_int(embedded),
                        fmt_int(total),
                        ratio
                    )
                    .yellow()
                    .to_string()
                };
                let icon = if pending == 0 { "✓".green() } else { "⚠".yellow() };
                println!("  {} {:<14} {}", icon.bold(), "coverage", pending_label);
            }
            row("active model", model);
        }
        _ => {
            row_info("backfill", "unavailable (embedding service disabled?)");
        }
    }
    // R24 B: surface R20 Phase A nudge counters when available.
    if let Some(n) = nudges.nudges_received {
        row("nudges", &format!("{} received", fmt_int(n as i64)));
    }
    if let Some(w) = nudges.fast_drain_wakes {
        row("fast-drain", &format!("{} wakes from idle", fmt_int(w as i64)));
    }
    println!();

    // Cache
    section("Query-embedding cache");
    let total_reqs = s.cache.hits + s.cache.misses;
    if total_reqs == 0 {
        row_info("requests", "cold — no queries yet");
    } else {
        row(
            "hit ratio",
            &format!(
                "{:.1} %  ({} hit / {} miss)",
                s.cache.hit_ratio * 100.0,
                s.cache.hits,
                s.cache.misses
            ),
        );
    }
    row("entries", &s.cache.entries.to_string());
    println!();

    // Acceleration
    section("Acceleration");
    let accel_label = match s.acceleration.as_str() {
        "cpu" => s.acceleration.dimmed().to_string(),
        other => other.green().bold().to_string(),
    };
    println!("  {:<16} {}", "compile-time", accel_label);
    println!();
}

fn section(title: &str) {
    println!("{}", title.bold().cyan());
}

fn row(label: &str, value: &str) {
    println!("  {:<16} {}", label.dimmed(), value);
}

fn row_check(label: &str, value: &str, ok: bool) {
    let icon = if ok {
        "✓".green().bold()
    } else {
        "✗".red().bold()
    };
    println!("  {} {:<14} {}", icon, label, value.dimmed());
}

fn row_info(label: &str, value: &str) {
    println!("  {} {:<14} {}", "ℹ".blue(), label, value.dimmed());
}

// ── Helpers ─────────────────────────────────────────────────────────

fn build_client(insecure_tls: bool) -> Result<reqwest::Client> {
    crate::http::client(insecure_tls, crate::http::DEFAULT_TIMEOUT)
}

fn friendly_net_error(e: reqwest::Error) -> anyhow::Error {
    let s = format!("{:?}", e).to_lowercase();
    if s.contains("certificate") || s.contains("tls") || s.contains("handshake") {
        anyhow::anyhow!(
            "TLS error hitting /ai/status: {}\n\
             Hint: pass --insecure, set insecure_tls = true in knishio.toml, or export KNISHIO_INSECURE_TLS=true \
             for self-signed dev certs",
            e
        )
    } else {
        anyhow::anyhow!(
            "Failed to reach /ai/status: {} — is the validator running? (try: knishio ready)",
            e
        )
    }
}

/// Simple thousands-separator for i64 values up to a billion-ish.
fn fmt_int(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
