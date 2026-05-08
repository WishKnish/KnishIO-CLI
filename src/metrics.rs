//! `knishio metrics` — pretty-print the validator's Prometheus scrape.
//!
//! Fetches `/metrics` (text-exposition format) and groups samples by
//! subsystem prefix. Histograms collapse to count / sum / avg — operators
//! who want per-bucket counts can still pipe `--raw` into their own
//! Prometheus-parsing tool.
//!
//! Subsystem mapping is a best-effort grouping of `knishio_<subsys>_*`
//! prefixes. Unknown families land in a catch-all "Other" bucket so new
//! validator-side metrics show up without CLI changes.

use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::Config;

/// Entry point for `knishio metrics`.
pub async fn metrics(cfg: &Config, filter: Option<&str>, raw: bool) -> Result<()> {
    let url = format!(
        "{}/metrics",
        cfg.validator.url.trim_end_matches('/')
    );

    let client = build_client(cfg.validator.insecure_tls)?;
    let resp = client.get(&url).send().await.map_err(friendly_net_error)?;
    let http_status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if http_status != 200 {
        anyhow::bail!("/metrics returned HTTP {}: {}", http_status, body);
    }

    if raw {
        print!("{}", body);
        return Ok(());
    }

    render(&body, filter, &cfg.validator.url);
    Ok(())
}

// ── Parsing ─────────────────────────────────────────────────────────

/// One flattened sample line from the Prometheus text format.
#[derive(Debug, Clone)]
struct Sample {
    /// The full sample name — `knishio_embedding_inference_seconds_bucket`,
    /// `knishio_cache_requests_total`, etc. Includes the `_bucket`/`_count`
    /// /`_sum` suffix for histogram-family lines.
    name: String,
    /// Label content between the braces, without the braces — empty when
    /// the sample has no labels. Preserved verbatim for display.
    labels: String,
    value: f64,
}

/// Parse the Prometheus text exposition body into a flat list of samples.
/// Skips `#`-prefixed HELP/TYPE lines and blank lines.
fn parse_samples(body: &str) -> Vec<Sample> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `name{labels} value` or `name value`. The last whitespace-
        // separated field is the value; everything before is the
        // label-carrying name.
        let Some((name_part, value_str)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let Ok(value) = value_str.parse::<f64>() else {
            continue;
        };
        let (name, labels) = match name_part.split_once('{') {
            Some((n, rest)) => {
                // rest ends with a trailing `}`.
                let labels = rest.strip_suffix('}').unwrap_or(rest).to_string();
                (n.to_string(), labels)
            }
            None => (name_part.to_string(), String::new()),
        };
        out.push(Sample { name, labels, value });
    }
    out
}

// ── Grouping ────────────────────────────────────────────────────────

/// Map a sample name to a display subsystem. Best-effort; unknown
/// `knishio_*` names fall through to "Other".
fn subsystem_of(name: &str) -> &'static str {
    let stripped = name.strip_prefix("knishio_").unwrap_or(name);
    match stripped.split('_').next().unwrap_or("") {
        "embedding" => "AI / Embedding",
        "generation" => "AI / Generation",
        "model" => "AI / Model Load",
        "cache" => "Cache",
        "db" => "Database",
        "graphql" => "HTTP / GraphQL",
        "molecule" | "molecules" => "Molecule Processing",
        "auth" => "Auth",
        "p2p" => "P2P",
        _ => "Other",
    }
}

/// Collapse a set of samples sharing the same base name into a single
/// rendered line. Histograms (base with `_bucket`/`_count`/`_sum`
/// sibling samples) render as `count=N sum=Ts avg=Ts`; scalars render
/// the value directly.
fn family_base(name: &str) -> String {
    for suffix in ["_bucket", "_count", "_sum"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    name.to_string()
}

/// Is this family a histogram? We treat any base that has a `_bucket`
/// sample as a histogram.
fn is_histogram(samples: &[&Sample]) -> bool {
    samples.iter().any(|s| s.name.ends_with("_bucket"))
}

// ── Rendering ───────────────────────────────────────────────────────

fn render(body: &str, filter: Option<&str>, base_url: &str) {
    let samples = parse_samples(body);

    // Group: subsystem → family_base → Vec<&Sample>.
    let mut grouped: BTreeMap<&'static str, BTreeMap<String, Vec<&Sample>>> =
        BTreeMap::new();
    for s in &samples {
        if let Some(f) = filter {
            if !s.name.to_lowercase().contains(&f.to_lowercase()) {
                continue;
            }
        }
        let base = family_base(&s.name);
        grouped
            .entry(subsystem_of(&s.name))
            .or_default()
            .entry(base)
            .or_default()
            .push(s);
    }

    if grouped.is_empty() {
        println!("{}", "(no metrics matched the filter)".dimmed());
        return;
    }

    println!();
    println!("{} ({})", "Validator metrics".bold(), base_url);
    println!();

    for (subsystem, families) in &grouped {
        println!("{}", subsystem.bold().cyan());
        for (base, samples) in families {
            render_family(base, samples);
        }
        println!();
    }
}

fn render_family(base: &str, samples: &[&Sample]) {
    if is_histogram(samples) {
        // Histogram: show count / sum / avg; skip the buckets.
        let count = samples
            .iter()
            .find(|s| s.name.ends_with("_count") && s.labels.is_empty())
            .map(|s| s.value as u64);
        let sum = samples
            .iter()
            .find(|s| s.name.ends_with("_sum") && s.labels.is_empty())
            .map(|s| s.value);

        // With labels: group by label set for histograms that are
        // partitioned (e.g. by provider or query_type).
        let mut partitioned: BTreeMap<&str, (Option<u64>, Option<f64>)> = BTreeMap::new();
        for s in samples {
            if s.labels.is_empty() {
                continue;
            }
            let entry = partitioned.entry(s.labels.as_str()).or_insert((None, None));
            if s.name.ends_with("_count") {
                entry.0 = Some(s.value as u64);
            } else if s.name.ends_with("_sum") {
                entry.1 = Some(s.value);
            }
        }

        if partitioned.is_empty() {
            // Unpartitioned histogram.
            let summary = format_hist_summary(count, sum);
            println!("  {:<50} {}", base, summary);
        } else {
            println!("  {}", base);
            for (labels, (c, s)) in &partitioned {
                let summary = format_hist_summary(*c, *s);
                println!("    {{{:<40}}} {}", labels, summary);
            }
        }
    } else {
        // Counter or gauge — one sample per label-set.
        for s in samples {
            let display_name = if s.labels.is_empty() {
                s.name.clone()
            } else {
                format!("{}{{{}}}", s.name, s.labels)
            };
            println!(
                "  {:<50} {}",
                display_name,
                format_value(s.value).dimmed()
            );
        }
    }
}

fn format_hist_summary(count: Option<u64>, sum: Option<f64>) -> String {
    match (count, sum) {
        (Some(c), Some(s)) if c > 0 => {
            let avg = s / c as f64;
            format!(
                "count={} sum={:.3}s avg={:.3}s",
                c, s, avg
            )
            .dimmed()
            .to_string()
        }
        (Some(c), Some(s)) => {
            format!("count={} sum={:.3}s", c, s).dimmed().to_string()
        }
        _ => "(no data)".dimmed().to_string(),
    }
}

fn format_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

// ── HTTP plumbing (mirrors health.rs TLS-bypass pattern) ────────────

fn build_client(insecure_tls: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

fn friendly_net_error(e: reqwest::Error) -> anyhow::Error {
    let err_str = format!("{:?}", e).to_lowercase();
    if err_str.contains("certificate")
        || err_str.contains("ssl")
        || err_str.contains("tls")
        || err_str.contains("verify")
        || err_str.contains("handshake")
    {
        anyhow::anyhow!(
            "TLS certificate error: {}\n\
             Hint: pass --insecure, set insecure_tls = true in knishio.toml, or export KNISHIO_INSECURE_TLS=true for self-signed certs",
            e
        )
    } else {
        anyhow::anyhow!("Connection failed: {} — is the validator running?", e)
    }
}
