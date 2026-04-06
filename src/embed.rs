//! Embedding management via `docker exec psql` and HTTP API.
//!
//! Provides four subcommands:
//! - `status` — show embedding coverage and model statistics
//! - `reset`  — clear stale embeddings so the automatic backfill re-embeds them
//! - `search` — run semantic search from the terminal via GraphQL
//! - `ask`    — ask a natural language question about DAG data (RAG)

use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;
use std::io::{self, Write};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::config::Config;
use crate::output;

// ── Constants ──────────────────────────────────────────────────

const MODEL_NAME_MAX_LEN: usize = 100;

// ── Input Validation ───────────────────────────────────────────

fn validate_model_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MODEL_NAME_MAX_LEN {
        anyhow::bail!("Model name must be 1-{} characters", MODEL_NAME_MAX_LEN);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    {
        anyhow::bail!(
            "Model name must contain only alphanumeric characters, dashes, dots, and underscores"
        );
    }
    Ok(())
}

// ── Database Operations ────────────────────────────────────────

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

// ── Helpers ────────────────────────────────────────────────────

/// Detect the active embedding model from the running validator container's env vars.
/// Returns `(model_name, dimensions)` or `None` if the container is not running.
async fn get_active_model(config: &Config) -> Result<Option<(String, usize)>> {
    let out = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            &config.docker.validator_container,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to inspect validator container")?;

    if !out.status.success() {
        return Ok(None);
    }

    let env_text = String::from_utf8_lossy(&out.stdout);
    let mut model_name: Option<String> = None;
    let mut dimensions: usize = 0;

    for line in env_text.lines() {
        if let Some(val) = line.strip_prefix("EMBEDDING_MODEL=") {
            model_name = Some(val.to_string());
        }
        if let Some(val) = line.strip_prefix("EMBEDDING_DIMENSIONS=") {
            dimensions = val.parse().unwrap_or(0);
        }
    }

    Ok(model_name.map(|name| (name, dimensions)))
}

/// Interactive confirmation prompt. Returns false on non-terminal (piped) stdin.
fn confirm(prompt: &str) -> bool {
    print!("{} [y/N] ", prompt);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Build an HTTP client with optional TLS bypass for self-signed certs.
fn http_client(insecure_tls: bool) -> Result<reqwest::Client> {
    http_client_with_timeout(insecure_tls, Duration::from_secs(30))
}

/// Build an HTTP client with custom timeout and optional TLS bypass.
fn http_client_with_timeout(insecure_tls: bool, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

/// Truncate a string for display, appending "..." if longer than max_len.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

// ── Commands ───────────────────────────────────────────────────

/// Show embedding coverage and model statistics.
pub async fn status(config: &Config) -> Result<()> {
    output::header("Embedding Status");

    // Detect active model from validator container
    let active_model = get_active_model(config).await?;
    match &active_model {
        Some((name, dims)) => {
            println!(
                "  Active model: {} ({}d)",
                name.green(),
                if *dims == 0 {
                    "native".to_string()
                } else {
                    dims.to_string()
                }
            );
        }
        None => {
            output::warn("Could not detect active model (is the validator container running?)");
        }
    }

    // Total coverage
    let totals = psql(config, "SELECT COUNT(*), COUNT(embedding) FROM metas").await?;
    let parts: Vec<&str> = totals.split('|').collect();
    let total: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let embedded: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

    if total == 0 {
        output::info("No meta rows in database");
        return Ok(());
    }

    let pct = (embedded as f64 / total as f64) * 100.0;
    println!("  Total metas: {}", total);
    println!("  Embedded:    {} ({:.1}%)", embedded, pct);
    println!("  Missing:     {}", total - embedded);

    // Per-model breakdown with dimensions
    let breakdown = psql(
        config,
        "SELECT COALESCE(embedding_model, '(none)'), COUNT(*), \
         COALESCE(vector_dims(embedding), 0) \
         FROM metas GROUP BY embedding_model, vector_dims(embedding) \
         ORDER BY COUNT(*) DESC",
    )
    .await?;

    if !breakdown.is_empty() {
        output::header("Models");
        println!("{:<30} {:>10} {:>8}", "MODEL", "COUNT", "DIMS");
        println!("{}", "-".repeat(52));

        let mut stale_count: u64 = 0;
        for line in breakdown.lines() {
            let cols: Vec<&str> = line.split('|').collect();
            if cols.len() >= 3 {
                let model = cols[0];
                let count: u64 = cols[1].parse().unwrap_or(0);
                let dims: u64 = cols[2].parse().unwrap_or(0);

                let is_stale = active_model
                    .as_ref()
                    .map(|(name, _)| model != name.as_str() && model != "(none)")
                    .unwrap_or(false);

                if is_stale {
                    stale_count += count;
                    println!(
                        "{:<30} {:>10} {:>8}  {}",
                        model,
                        count,
                        dims,
                        "STALE".yellow()
                    );
                } else if model == "(none)" {
                    println!(
                        "{:<30} {:>10} {:>8}  {}",
                        model,
                        count,
                        "-",
                        "PENDING".blue()
                    );
                } else {
                    println!("{:<30} {:>10} {:>8}", model, count, dims);
                }
            }
        }

        if stale_count > 0 {
            println!();
            output::warn(&format!(
                "{} embeddings from stale models — run `knishio embed reset` to clear them",
                stale_count
            ));
        }
    }

    // Backfill queue
    if let Some((model_name, _)) = &active_model {
        let queue_sql = format!(
            "SELECT COUNT(*) FROM metas WHERE embedding IS NULL OR embedding_model != '{}'",
            model_name.replace('\'', "''")
        );
        let pending: u64 = psql(config, &queue_sql)
            .await?
            .trim()
            .parse()
            .unwrap_or(0);
        if pending > 0 {
            output::info(&format!(
                "Backfill queue: {} metas pending (auto-processes while validator is running)",
                pending
            ));
        } else {
            output::success("All metas embedded with current model");
        }
    }

    Ok(())
}

/// Clear embeddings so the automatic backfill re-embeds them.
pub async fn reset(
    config: &Config,
    model: Option<&str>,
    all: bool,
    skip_confirm: bool,
) -> Result<()> {
    if let Some(name) = model {
        validate_model_name(name)?;
    }

    // Build WHERE clause
    let (where_clause, description) = if all {
        (
            "embedding IS NOT NULL".to_string(),
            "ALL embeddings".to_string(),
        )
    } else if let Some(model_name) = model {
        (
            format!(
                "embedding_model = '{}'",
                model_name.replace('\'', "''")
            ),
            format!("embeddings from model '{}'", model_name),
        )
    } else {
        // Default: clear stale (model != active)
        let active = get_active_model(config).await?;
        match active {
            Some((name, _)) => (
                format!(
                    "embedding_model IS NOT NULL AND embedding_model != '{}'",
                    name.replace('\'', "''")
                ),
                format!("stale embeddings (model != '{}')", name),
            ),
            None => {
                anyhow::bail!(
                    "Cannot detect active model — use --model <name> or --all instead"
                );
            }
        }
    };

    // Count affected rows
    let count_sql = format!("SELECT COUNT(*) FROM metas WHERE {}", where_clause);
    let affected: u64 = psql(config, &count_sql)
        .await?
        .trim()
        .parse()
        .unwrap_or(0);

    if affected == 0 {
        output::info("No embeddings match the criteria — nothing to reset");
        return Ok(());
    }

    // Confirm
    if !skip_confirm {
        output::warn(&format!(
            "This will clear {} for {} meta rows",
            description, affected
        ));
        output::info("The automatic backfill will re-embed them while the validator is running");
        if !confirm("Proceed?") {
            output::info("Aborted");
            return Ok(());
        }
    }

    // Execute
    let update_sql = format!(
        "UPDATE metas \
         SET embedding = NULL, embedding_model = NULL, \
             embedded_at = NULL, content_hash = NULL \
         WHERE {}",
        where_clause
    );
    psql(config, &update_sql).await?;

    output::success(&format!(
        "Cleared {} for {} meta rows",
        description, affected
    ));
    output::info("Run `knishio embed status` to monitor backfill progress");

    Ok(())
}

// ── Search & Ask Types ────────────────────────────────────────

#[derive(Deserialize)]
struct GraphQLResponse {
    data: Option<SearchData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize)]
struct SearchData {
    #[serde(rename = "searchMetasByText")]
    search_metas_by_text: Option<Vec<SearchResult>>,

    #[serde(rename = "askDag")]
    ask_dag: Option<AskDagResult>,
}

#[derive(Deserialize)]
struct AskDagResult {
    answer: String,
    sources: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(rename = "metaType")]
    meta_type: String,
    #[serde(rename = "metaId")]
    meta_id: String,
    key: String,
    value: String,
    similarity: f32,
    #[serde(rename = "molecularHash")]
    molecular_hash: Option<String>,
}

#[derive(Deserialize)]
struct GraphQLError {
    message: String,
}

/// Run semantic search from the terminal via GraphQL.
pub async fn search(
    config: &Config,
    query: &str,
    limit: i32,
    threshold: f64,
    meta_type: Option<&str>,
) -> Result<()> {
    let graphql_query = r#"query SearchMetasByText($query: String!, $metaType: String, $similarityThreshold: Float, $limit: Int!) {
  searchMetasByText(query: $query, metaType: $metaType, similarityThreshold: $similarityThreshold, limit: $limit) {
    metaType
    metaId
    key
    value
    similarity
    molecularHash
  }
}"#;

    let variables = serde_json::json!({
        "query": query,
        "metaType": meta_type,
        "similarityThreshold": threshold,
        "limit": limit.min(100),
    });

    let body = serde_json::json!({
        "query": graphql_query,
        "variables": variables,
    });

    let url = format!("{}/graphql", config.validator.url);
    let client = http_client(config.validator.insecure_tls)?;

    let resp = client.post(&url).json(&body).send().await.map_err(|e| {
        let err_str = format!("{:?}", e).to_lowercase();
        if err_str.contains("certificate")
            || err_str.contains("tls")
            || err_str.contains("handshake")
        {
            anyhow::anyhow!(
                "TLS error: {}\n\
                 Hint: set insecure_tls = true in knishio.toml or KNISHIO_INSECURE_TLS=true",
                e
            )
        } else {
            anyhow::anyhow!("Connection failed: {} — is the validator running?", e)
        }
    })?;

    let resp_body: GraphQLResponse = resp
        .json()
        .await
        .context("Failed to parse GraphQL response")?;

    // Handle GraphQL errors
    if let Some(errors) = resp_body.errors {
        let msg = &errors[0].message;
        if msg.contains("Embedding service not available") || msg.contains("not enabled") {
            output::error("Embedding service is not enabled on the validator");
            output::info(
                "Set EMBEDDING_ENABLED=true in docker-compose.standalone.yml and restart",
            );
            return Ok(());
        }
        if msg.contains("different vector dimensions") {
            output::error(&format!("Dimension mismatch: {}", msg));
            output::info("Run `knishio embed reset` to clear stale embeddings, then retry");
            return Ok(());
        }
        anyhow::bail!("GraphQL error: {}", msg);
    }

    let results = resp_body
        .data
        .and_then(|d| d.search_metas_by_text)
        .unwrap_or_default();

    if results.is_empty() {
        output::info(&format!(
            "No results for \"{}\" (threshold: {:.2})",
            query, threshold
        ));
        return Ok(());
    }

    // Display results
    output::header(&format!(
        "Search: \"{}\" ({} results)",
        query,
        results.len()
    ));
    println!(
        "{:<6} {:<16} {:<20} {:<12} {:<40} {}",
        "SCORE", "TYPE", "ID", "KEY", "VALUE", "MOLECULE"
    );
    println!("{}", "-".repeat(100));

    for r in &results {
        let value_display = truncate(&r.value, 40);
        let mol_display = r
            .molecular_hash
            .as_deref()
            .map(|h| truncate(h, 12))
            .unwrap_or_else(|| "-".to_string());

        let score_str = format!("{:.3}", r.similarity);
        let score_colored = if r.similarity >= 0.9 {
            score_str.green()
        } else if r.similarity >= 0.8 {
            score_str.yellow()
        } else {
            score_str.normal()
        };

        println!(
            "{:<6} {:<16} {:<20} {:<12} {:<40} {}",
            score_colored,
            truncate(&r.meta_type, 16),
            truncate(&r.meta_id, 20),
            truncate(&r.key, 12),
            value_display,
            mol_display,
        );
    }

    Ok(())
}

/// Ask a natural language question about DAG data via streaming SSE endpoint.
///
/// Streams tokens in real-time with a spinner, falling back to GraphQL if
/// the SSE endpoint is unavailable (older validator versions).
pub async fn ask(
    config: &Config,
    question: &str,
    max_results: i32,
    threshold: f64,
    meta_type: Option<&str>,
) -> Result<()> {
    output::info(&format!("Asking: \"{}\"", question));

    // Try SSE streaming first
    let stream_url = format!("{}/api/ask-stream", config.validator.url);
    let client = http_client_with_timeout(config.validator.insecure_tls, Duration::from_secs(300))?;

    let sse_body = serde_json::json!({
        "question": question,
        "metaType": meta_type,
        "similarityThreshold": threshold,
        "maxResults": max_results.min(50),
    });

    let resp = client.post(&stream_url).json(&sse_body).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            ask_streaming(r, question).await
        }
        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
            // SSE endpoint not available (older validator) — fall back to GraphQL
            output::info("Streaming not available, using GraphQL fallback...");
            ask_graphql(config, question, max_results, threshold, meta_type).await
        }
        Ok(r) => {
            anyhow::bail!("ask-stream returned HTTP {}", r.status());
        }
        Err(e) => {
            let err_str = format!("{:?}", e).to_lowercase();
            if err_str.contains("certificate")
                || err_str.contains("tls")
                || err_str.contains("handshake")
            {
                anyhow::bail!(
                    "TLS error: {}\n\
                     Hint: set insecure_tls = true in knishio.toml or KNISHIO_INSECURE_TLS=true",
                    e
                );
            }
            // Connection refused / timeout — try GraphQL fallback
            output::info("Streaming endpoint unreachable, using GraphQL fallback...");
            ask_graphql(config, question, max_results, threshold, meta_type).await
        }
    }
}

/// Stream tokens from the SSE endpoint with a spinner.
async fn ask_streaming(resp: reqwest::Response, question: &str) -> Result<()> {
    use futures_util::StreamExt;
    use indicatif::{ProgressBar, ProgressStyle};

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template(" {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Connecting...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut streaming_tokens = false;
    let mut full_answer = String::new();
    let mut sources: Vec<AskDagSource> = vec![];

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Stream read error")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Parse SSE events (split on double newline)
        while let Some(boundary) = buffer.find("\n\n") {
            let block = buffer[..boundary].trim().to_string();
            buffer = buffer[boundary + 2..].to_string();

            if block.is_empty() {
                continue;
            }

            let mut event_type = String::new();
            let mut data_str = String::new();

            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_type = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_str = rest.trim().to_string();
                }
            }

            if data_str.is_empty() {
                continue;
            }

            match event_type.as_str() {
                "status" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&data_str) {
                        let phase = payload["phase"].as_str().unwrap_or("working");
                        let msg = match phase {
                            "embedding" => "Embedding question...".to_string(),
                            "searching" => {
                                if let Some(count) = payload["count"].as_u64() {
                                    format!("Searching DAG ({} records)...", count)
                                } else {
                                    "Searching DAG...".to_string()
                                }
                            }
                            "generating" => "Generating answer...".to_string(),
                            other => format!("{}...", other),
                        };
                        spinner.set_message(msg);
                    }
                }
                "token" => {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&data_str) {
                        if let Some(text) = payload["text"].as_str() {
                            if !streaming_tokens {
                                // First token — stop spinner and print header
                                spinner.finish_and_clear();
                                streaming_tokens = true;
                                println!();
                                output::header(&format!("Question: \"{}\"", question));
                                println!();
                            }
                            full_answer.push_str(text);
                            print!("{}", text);
                            io::stdout().flush().ok();
                        }
                    }
                }
                "done" => {
                    if !streaming_tokens {
                        spinner.finish_and_clear();
                    }
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&data_str) {
                        // If we never streamed tokens, show the full answer
                        if !streaming_tokens {
                            if let Some(answer) = payload["answer"].as_str() {
                                println!();
                                output::header(&format!("Question: \"{}\"", question));
                                println!();
                                println!("{}", answer);
                            }
                        } else {
                            println!(); // Final newline after streamed tokens
                        }
                        // Parse sources
                        if let Some(src_arr) = payload["sources"].as_array() {
                            for src in src_arr {
                                sources.push(AskDagSource {
                                    meta_type: src["metaType"].as_str().unwrap_or("").to_string(),
                                    meta_id: src["metaId"].as_str().unwrap_or("").to_string(),
                                    key: src["key"].as_str().unwrap_or("").to_string(),
                                    value: src["value"].as_str().unwrap_or("").to_string(),
                                    similarity: src["similarity"].as_f64().unwrap_or(0.0) as f32,
                                    molecular_hash: src["molecularHash"].as_str().map(|s| s.to_string()),
                                });
                            }
                        }
                    }
                }
                "error" => {
                    spinner.finish_and_clear();
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&data_str) {
                        let msg = payload["message"].as_str().unwrap_or("Unknown error");
                        output::error(msg);
                        if msg.contains("Embedding service") {
                            output::info("Set EMBEDDING_ENABLED=true in docker-compose.standalone.yml and restart");
                        } else if msg.contains("Generation service") {
                            output::info("Set GENERATION_ENABLED=true in docker-compose.standalone.yml and restart");
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // Display sources table
    display_sources(&sources);
    Ok(())
}

/// Fallback: non-streaming GraphQL askDag query.
async fn ask_graphql(
    config: &Config,
    question: &str,
    max_results: i32,
    threshold: f64,
    meta_type: Option<&str>,
) -> Result<()> {
    use indicatif::{ProgressBar, ProgressStyle};

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template(" {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message("Searching DAG and generating answer...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    let graphql_query = r#"query AskDag($question: String!, $metaType: String, $similarityThreshold: Float, $maxResults: Int!) {
  askDag(question: $question, metaType: $metaType, similarityThreshold: $similarityThreshold, maxResults: $maxResults) {
    answer
    sources {
      metaType
      metaId
      key
      value
      similarity
      molecularHash
    }
  }
}"#;

    let variables = serde_json::json!({
        "question": question,
        "metaType": meta_type,
        "similarityThreshold": threshold,
        "maxResults": max_results.min(50),
    });

    let body = serde_json::json!({
        "query": graphql_query,
        "variables": variables,
    });

    let url = format!("{}/graphql", config.validator.url);
    let client = http_client_with_timeout(config.validator.insecure_tls, Duration::from_secs(120))?;

    let resp = client.post(&url).json(&body).send().await.map_err(|e| {
        let err_str = format!("{:?}", e).to_lowercase();
        if err_str.contains("certificate")
            || err_str.contains("tls")
            || err_str.contains("handshake")
        {
            anyhow::anyhow!(
                "TLS error: {}\n\
                 Hint: set insecure_tls = true in knishio.toml or KNISHIO_INSECURE_TLS=true",
                e
            )
        } else {
            anyhow::anyhow!("Connection failed: {} — is the validator running?", e)
        }
    })?;

    spinner.finish_and_clear();

    let resp_body: GraphQLResponse = resp
        .json()
        .await
        .context("Failed to parse GraphQL response")?;

    if let Some(errors) = resp_body.errors {
        let msg = &errors[0].message;
        if msg.contains("Embedding service not available") || msg.contains("not enabled") {
            output::error("Embedding service is not enabled on the validator");
            output::info("Set EMBEDDING_ENABLED=true in docker-compose.standalone.yml and restart");
            return Ok(());
        }
        if msg.contains("Generation service not available") {
            output::error("Generation service is not enabled on the validator");
            output::info("Set GENERATION_ENABLED=true in docker-compose.standalone.yml and restart");
            return Ok(());
        }
        anyhow::bail!("GraphQL error: {}", msg);
    }

    let result = resp_body
        .data
        .and_then(|d| d.ask_dag)
        .ok_or_else(|| anyhow::anyhow!("No askDag data in response"))?;

    println!();
    output::header(&format!("Question: \"{}\"", question));
    println!();
    println!("{}", result.answer);

    let sources: Vec<AskDagSource> = result
        .sources
        .iter()
        .map(|r| AskDagSource {
            meta_type: r.meta_type.clone(),
            meta_id: r.meta_id.clone(),
            key: r.key.clone(),
            value: r.value.clone(),
            similarity: r.similarity,
            molecular_hash: r.molecular_hash.clone(),
        })
        .collect();

    display_sources(&sources);
    Ok(())
}

/// Display sources table (shared between streaming and fallback paths).
fn display_sources(sources: &[AskDagSource]) {
    if sources.is_empty() {
        return;
    }

    println!();
    output::header(&format!("Sources ({} records)", sources.len()));
    println!(
        "{:<6} {:<16} {:<20} {:<12} {:<40} {}",
        "SCORE", "TYPE", "ID", "KEY", "VALUE", "MOLECULE"
    );
    println!("{}", "-".repeat(100));

    for r in sources {
        let value_display = truncate(&r.value, 40);
        let mol_display = r
            .molecular_hash
            .as_deref()
            .map(|h| truncate(h, 12))
            .unwrap_or_else(|| "-".to_string());

        let score_str = format!("{:.3}", r.similarity);
        let score_colored = if r.similarity >= 0.9 {
            score_str.green()
        } else if r.similarity >= 0.8 {
            score_str.yellow()
        } else {
            score_str.normal()
        };

        println!(
            "{:<6} {:<16} {:<20} {:<12} {:<40} {}",
            score_colored,
            truncate(&r.meta_type, 16),
            truncate(&r.meta_id, 20),
            truncate(&r.key, 12),
            value_display,
            mol_display,
        );
    }
}

/// Source record for display (shared between SSE and GraphQL paths).
struct AskDagSource {
    meta_type: String,
    meta_id: String,
    key: String,
    value: String,
    similarity: f32,
    molecular_hash: Option<String>,
}
