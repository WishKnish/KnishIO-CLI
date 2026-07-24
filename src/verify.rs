//! `knishio verify` — deployment acceptance gauntlet.
//!
//! Codifies the external verification run that accepted the first public
//! testnet deployment (validator repo docs/audits/TESTNET-DEPLOY-2026-07-23.md,
//! Phase 3): liveness/readiness, GraphQL, WebSocket subscriptions on BOTH
//! routes, unbuffered SSE, edge hardening (HSTS, http→https, /metrics +
//! /config blocked), rate-limit headers, and TLS certificate health.
//!
//! Checks are Pass/Fail/Warn/Skipped — Skipped carries a reason (e.g. edge
//! checks against a direct local validator). Exit code 1 when anything
//! Fails. `--json` renders the full report on stdout for CI; human meta
//! stays on stderr per output.rs conventions.

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::output;

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const SSE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
    Warn,
    Skipped,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeProfile {
    /// Public deployment behind a TLS-terminating reverse proxy — all 13
    /// checks apply.
    Edge,
    /// Direct validator (typically local dev) — edge-hardening checks are
    /// skipped (/metrics being reachable directly is CORRECT there).
    Direct,
}

#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub target: String,
    pub profile: EdgeProfile,
    pub checks: Vec<CheckResult>,
}

// ── Pure evaluators (unit-tested on canned fixtures) ───────────────

fn eval_readyz(status: u16, body: &Value) -> (CheckStatus, String) {
    if status != 200 {
        return (CheckStatus::Fail, format!("HTTP {}", status));
    }
    let m = &body["migrations"];
    let applied = m
        .get("applied")
        .or_else(|| m.get("applied_count"))
        .and_then(Value::as_i64);
    let expected = m
        .get("expected")
        .or_else(|| m.get("expected_count"))
        .and_then(Value::as_i64);
    match (applied, expected) {
        (Some(a), Some(e)) if a == e => {
            (CheckStatus::Pass, format!("ready, migrations {}/{}", a, e))
        }
        (Some(a), Some(e)) => (
            CheckStatus::Fail,
            format!("migrations applied {} != expected {}", a, e),
        ),
        _ => (
            CheckStatus::Warn,
            "200 but no migrations counts in body".to_string(),
        ),
    }
}

fn eval_graphql(body: &Value) -> (CheckStatus, String) {
    match body["data"]["__typename"].as_str() {
        Some(t) => (CheckStatus::Pass, format!("__typename = {}", t)),
        None => (
            CheckStatus::Fail,
            format!("no data.__typename in: {}", truncate(&body.to_string(), 120)),
        ),
    }
}

fn eval_blocked(path: &str, status: u16) -> (CheckStatus, String) {
    if status == 404 || status == 403 {
        (CheckStatus::Pass, format!("{} → {}", path, status))
    } else {
        (
            CheckStatus::Fail,
            format!("{} publicly reachable (HTTP {})", path, status),
        )
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max])
    } else {
        s.to_string()
    }
}

// ── Write-path smoke ────────────────────────────────────────────────

/// Full crypto write path: ephemeral identity → U-isotope auth (captures the
/// bundle-bound JWT) → OTS-signed M-isotope createMeta → GraphQL readback.
/// Mirrors the JS-SDK smoke that accepted the testnet deployment. Leaves ONE
/// meta molecule of type KNISHIO_VERIFY_SMOKE on the target (documented).
async fn write_smoke(
    base: &str,
    cell_slug: &str,
    insecure: bool,
) -> Result<(CheckStatus, String)> {
    // Molecule proposal can trigger synchronous embedding on AI-enabled
    // stacks (QA-020) — cold model loads take tens of seconds. Generous
    // timeout, independent of the fast check client.
    let client = &crate::http::client(insecure, Duration::from_secs(90))?;
    use knishio_client::Wallet;

    // Ephemeral identity for this run.
    let nonce: u64 = std::process::id() as u64 ^ std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let secret = format!("{:0>128}", format!("knishio-verify-smoke-{:x}", nonce));
    let auth_wallet = Wallet::create(Some(&secret), None, "AUTH", None, None)
        .map_err(|e| anyhow::anyhow!("wallet create failed: {e}"))?;
    let bundle = auth_wallet
        .bundle
        .clone()
        .ok_or_else(|| anyhow::anyhow!("auth wallet has no bundle"))?;

    // 1. U-isotope auth → token.
    let auth_mol = crate::bench::molecules::make_auth(&secret, &bundle, cell_slug)?;
    let next_pos = crate::bench::molecules::next_position(&auth_mol)?;
    let auth_resp = propose(client, base, &auth_mol, None).await?;
    let token = auth_resp
        .pointer("/data/ProposeMolecule/token")
        .and_then(Value::as_str)
        .map(str::to_string);
    let auth_status = auth_resp
        .pointer("/data/ProposeMolecule/status")
        .and_then(Value::as_str)
        .unwrap_or("error");
    if auth_status != "accepted" || token.is_none() {
        return Ok((
            CheckStatus::Fail,
            format!(
                "auth molecule not accepted: status={} reason={}",
                auth_status,
                auth_resp
                    .pointer("/data/ProposeMolecule/reason")
                    .or_else(|| auth_resp.pointer("/errors/0/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
        ));
    }

    // 2. OTS-signed M-isotope meta write.
    let meta_id = format!("smoke-{:x}", nonce);
    let meta_mol = crate::bench::molecules::make_meta_custom(
        &secret,
        &bundle,
        &next_pos,
        cell_slug,
        "KNISHIO_VERIFY_SMOKE",
        &meta_id,
        vec![knishio_client::MetaItem::new("verifiedBy", "knishio verify --write-smoke")],
    )?;
    let meta_resp = propose(client, base, &meta_mol, token.as_deref()).await?;
    let meta_status = meta_resp
        .pointer("/data/ProposeMolecule/status")
        .and_then(Value::as_str)
        .unwrap_or("error");
    if meta_status != "accepted" {
        return Ok((
            CheckStatus::Fail,
            format!(
                "meta molecule not accepted: status={} reason={}",
                meta_status,
                meta_resp
                    .pointer("/data/ProposeMolecule/reason")
                    .or_else(|| meta_resp.pointer("/errors/0/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
        ));
    }

    // 3. Readback through the query path.
    let q = json!({
        "query": "query M($metaType: String, $metaId: String) { MetaType(metaType: $metaType, metaId: $metaId) { metaType instances { metaId } } }",
        "variables": {"metaType": "KNISHIO_VERIFY_SMOKE", "metaId": meta_id}
    });
    let mut req = client.post(format!("{}/graphql", base)).json(&q);
    if let Some(t) = &token {
        req = req.header("X-Auth-Token", t);
    }
    let rb: Value = req.send().await?.json().await.unwrap_or(Value::Null);
    let found = rb
        .pointer("/data/MetaType/0/instances")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .any(|i| i["metaId"].as_str() == Some(meta_id.as_str()))
        })
        .unwrap_or(false);
    Ok(if found {
        (
            CheckStatus::Pass,
            format!("auth + createMeta accepted + readback ({})", meta_id),
        )
    } else {
        (
            CheckStatus::Warn,
            format!(
                "molecules accepted but readback did not find {} — query shape drift?",
                meta_id
            ),
        )
    })
}

async fn propose(
    client: &reqwest::Client,
    base: &str,
    mol: &knishio_client::Molecule,
    token: Option<&str>,
) -> Result<Value> {
    let q = json!({
        "query": "mutation ProposeMolecule($molecule: MoleculeInput!) { ProposeMolecule(molecule: $molecule) { status molecularHash reason token } }",
        "variables": {"molecule": serde_json::to_value(mol)?}
    });
    let mut req = client.post(format!("{}/graphql", base)).json(&q);
    if let Some(t) = token {
        req = req.header("X-Auth-Token", t);
    }
    Ok(req.send().await?.json().await.unwrap_or(Value::Null))
}

// ── The gauntlet ────────────────────────────────────────────────────

pub async fn run(
    cfg: &Config,
    edge: &str,
    json_output: bool,
    smoke: bool,
    smoke_cell: &str,
    yes: bool,
) -> Result<()> {
    let base = cfg.validator.url.trim_end_matches('/').to_string();
    let insecure = cfg.validator.insecure_tls;

    let profile = match edge {
        "edge" => EdgeProfile::Edge,
        "direct" => EdgeProfile::Direct,
        _ => {
            if crate::target::is_local_url(&base) {
                EdgeProfile::Direct
            } else {
                EdgeProfile::Edge
            }
        }
    };
    let is_https = base.starts_with("https://");
    let edge_skip = |name: &'static str| CheckResult {
        name,
        status: CheckStatus::Skipped,
        detail: "edge-only check (profile: direct)".into(),
        ms: 0,
    };

    output::info(&format!(
        "Running verification gauntlet against {} (profile: {:?})",
        base, profile
    ));

    let client = crate::http::client(insecure, CHECK_TIMEOUT)?;
    let mut checks: Vec<CheckResult> = Vec::new();

    // 1. /healthz liveness
    checks.push(timed("healthz", || async {
        let status = client.get(format!("{}/healthz", base)).send().await?.status();
        Ok(if status.is_success() {
            (CheckStatus::Pass, format!("HTTP {}", status.as_u16()))
        } else {
            (CheckStatus::Fail, format!("HTTP {}", status.as_u16()))
        })
    })
    .await);

    // 2. /readyz readiness + migrations applied == expected (from the body —
    //    the count is compiled into the binary; never hardcode it).
    checks.push(timed("readyz", || async {
        let resp = client.get(format!("{}/readyz", base)).send().await?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(eval_readyz(status, &body))
    })
    .await);

    // 3. /health dashboard + version capture
    checks.push(timed("health", || async {
        let resp = client.get(format!("{}/health", base)).send().await?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let version = body["version"].as_str().unwrap_or("?").to_string();
        Ok(if status == 200 {
            (CheckStatus::Pass, format!("version {}", version))
        } else {
            (CheckStatus::Fail, format!("HTTP {}", status))
        })
    })
    .await);

    // 4. GraphQL round-trip
    checks.push(timed("graphql", || async {
        let resp = client
            .post(format!("{}/graphql", base))
            .json(&json!({"query": "{ __typename }"}))
            .send()
            .await?;
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(eval_graphql(&body))
    })
    .await);

    // 5+6. WebSocket handshake on the canonical route AND the JS-SDK-derived
    //      /ws alias (main.rs routes both; nginx must proxy both).
    for (name, path) in [("ws-graphql", "/graphql/ws"), ("ws-alias", "/ws")] {
        checks.push(
            timed(name, || async {
                let url = crate::ws::to_ws_url(&base, path);
                match crate::ws::connect_and_ack(&url, insecure).await {
                    Ok(mut ws) => {
                        let _ = ws.close(None).await;
                        Ok((CheckStatus::Pass, "connection_ack".to_string()))
                    }
                    Err(e) => Ok((CheckStatus::Fail, format!("{:#}", e))),
                }
            })
            .await,
        );
    }

    // 7. SSE unbuffered streaming: POST /api/ask-stream must return 200 and
    //    yield FIRST BYTES quickly (a buffering proxy sits on the stream).
    //    The graceful AI-off error event is a Pass — it proves the path.
    checks.push(timed("sse-stream", || async {
        let sse_client = crate::http::client(insecure, SSE_TIMEOUT)?;
        let resp = sse_client
            .post(format!("{}/api/ask-stream", base))
            .json(&json!({"question": "ping"}))
            .send()
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Ok((CheckStatus::Fail, format!("HTTP {}", status)));
        }
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        match tokio::time::timeout(SSE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(first))) => {
                let text = String::from_utf8_lossy(&first);
                Ok((
                    CheckStatus::Pass,
                    format!("first bytes streamed: {}", truncate(text.trim(), 60)),
                ))
            }
            Ok(Some(Err(e))) => Ok((CheckStatus::Fail, format!("stream error: {}", e))),
            Ok(None) => Ok((CheckStatus::Fail, "stream ended with no bytes".into())),
            Err(_) => Ok((
                CheckStatus::Fail,
                "no bytes within 15s — proxy buffering?".into(),
            )),
        }
    })
    .await);

    // 8. HSTS at the edge (the validator only emits it with in-binary TLS).
    if profile == EdgeProfile::Edge && is_https {
        checks.push(timed("hsts", || async {
            let resp = client.get(format!("{}/healthz", base)).send().await?;
            Ok(if resp.headers().contains_key("strict-transport-security") {
                (CheckStatus::Pass, "header present".into())
            } else {
                (
                    CheckStatus::Fail,
                    "Strict-Transport-Security missing (add at the proxy)".into(),
                )
            })
        })
        .await);
    } else {
        checks.push(edge_skip("hsts"));
    }

    // 9. http→https redirect (edge). Needs the no-redirect client — the
    //    default one would follow and mask the 301.
    if profile == EdgeProfile::Edge && is_https {
        checks.push(timed("http-redirect", || async {
            let http_base = base.replacen("https://", "http://", 1);
            let nr = crate::http::client_no_redirect(insecure, CHECK_TIMEOUT)?;
            match nr.get(format!("{}/healthz", http_base)).send().await {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    Ok(if (300..400).contains(&s) {
                        (CheckStatus::Pass, format!("HTTP {}", s))
                    } else {
                        (CheckStatus::Fail, format!("expected 3xx, got {}", s))
                    })
                }
                Err(e) => Ok((
                    CheckStatus::Warn,
                    format!("port 80 unreachable ({})", truncate(&e.to_string(), 60)),
                )),
            }
        })
        .await);
    } else {
        checks.push(edge_skip("http-redirect"));
    }

    // 10+11. Operational internals blocked at the edge.
    for (name, path) in [("metrics-blocked", "/metrics"), ("config-blocked", "/config")] {
        if profile == EdgeProfile::Edge {
            checks.push(
                timed(name, || async {
                    let status = client.get(format!("{}{}", base, path)).send().await?.status();
                    Ok(eval_blocked(path, status.as_u16()))
                })
                .await,
            );
        } else {
            checks.push(edge_skip(name));
        }
    }

    // 12. Per-IP rate limiting advertised.
    checks.push(timed("ratelimit-headers", || async {
        let resp = client
            .post(format!("{}/graphql", base))
            .json(&json!({"query": "{ __typename }"}))
            .send()
            .await?;
        let count = resp
            .headers()
            .iter()
            .filter(|(k, _)| k.as_str().starts_with("x-ratelimit"))
            .count();
        Ok(if count > 0 {
            (CheckStatus::Pass, format!("{} x-ratelimit headers", count))
        } else {
            (
                CheckStatus::Warn,
                "no x-ratelimit headers (rate limiting disabled?)".into(),
            )
        })
    })
    .await);

    // 13. TLS certificate health (validity is enforced by the handshake
    //     itself unless --insecure; expiry read from the peer cert).
    if is_https && !insecure {
        checks.push(timed("tls-cert", || async {
            let tls_client = reqwest::Client::builder()
                .timeout(CHECK_TIMEOUT)
                .tls_info(true)
                .build()?;
            let resp = tls_client.get(format!("{}/healthz", base)).send().await?;
            let Some(info) = resp.extensions().get::<reqwest::tls::TlsInfo>() else {
                return Ok((CheckStatus::Warn, "no TLS info exposed".into()));
            };
            let Some(der) = info.peer_certificate() else {
                return Ok((CheckStatus::Warn, "no peer certificate exposed".into()));
            };
            match x509_parser::parse_x509_certificate(der) {
                Ok((_, cert)) => {
                    let not_after = cert.validity().not_after.timestamp();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let days = (not_after - now) / 86_400;
                    let subject = cert.subject().to_string();
                    Ok(if days < 0 {
                        (CheckStatus::Fail, format!("cert EXPIRED ({})", subject))
                    } else if days < 14 {
                        (
                            CheckStatus::Warn,
                            format!("cert expires in {} days ({})", days, subject),
                        )
                    } else {
                        (
                            CheckStatus::Pass,
                            format!("valid, {} days to expiry ({})", days, subject),
                        )
                    })
                }
                Err(e) => Ok((CheckStatus::Warn, format!("cert parse failed: {}", e))),
            }
        })
        .await);
    } else {
        checks.push(CheckResult {
            name: "tls-cert",
            status: CheckStatus::Skipped,
            detail: if insecure {
                "--insecure set; certificate not validated".into()
            } else {
                "target is not https".into()
            },
            ms: 0,
        });
    }

    // 14 (opt-in). Write-path smoke: mutates the ledger (one meta molecule
    // of type KNISHIO_VERIFY_SMOKE) — confirmation-gated for non-local targets.
    if smoke {
        crate::target::confirm_mutation(
            "submit write-smoke molecules (leaves one KNISHIO_VERIFY_SMOKE meta)",
            &base,
            crate::target::is_local_url(&base),
            yes,
        )?;
        let cell = smoke_cell.to_string();
        checks.push(
            timed("write-smoke", || async {
                write_smoke(&base, &cell, insecure).await
            })
            .await,
        );
    }

    // ── Report ──────────────────────────────────────────────────────
    let report = VerifyReport {
        target: base.clone(),
        profile,
        checks,
    };

    let mut pass = 0;
    let mut fail = 0;
    let mut warn = 0;
    let mut skipped = 0;
    for c in &report.checks {
        match c.status {
            CheckStatus::Pass => pass += 1,
            CheckStatus::Fail => fail += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Skipped => skipped += 1,
        }
        if !json_output {
            let (mark, name_col) = match c.status {
                CheckStatus::Pass => ("✓".green().to_string(), c.name.normal()),
                CheckStatus::Fail => ("✗".red().bold().to_string(), c.name.red().bold()),
                CheckStatus::Warn => ("⚠".yellow().to_string(), c.name.yellow()),
                CheckStatus::Skipped => ("–".dimmed().to_string(), c.name.dimmed()),
            };
            println!(
                "{} {:<18} {:<60} {}",
                mark,
                name_col,
                c.detail,
                if c.ms > 0 {
                    format!("{}ms", c.ms).dimmed().to_string()
                } else {
                    String::new()
                }
            );
        }
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    eprintln!();
    let summary = format!(
        "{} passed, {} failed, {} warnings, {} skipped",
        pass, fail, warn, skipped
    );
    if fail > 0 {
        output::error(&format!("Verification FAILED — {}", summary));
        anyhow::bail!("{} verification check(s) failed", fail);
    }
    output::success(&format!("Verification passed — {}", summary));
    Ok(())
}

/// Run one named check with timing; transport-level errors become Fail.
async fn timed<F, Fut>(name: &'static str, f: F) -> CheckResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(CheckStatus, String)>>,
{
    let start = Instant::now();
    let (status, detail) = match f().await {
        Ok(r) => r,
        Err(e) => (CheckStatus::Fail, format!("{:#}", e)),
    };
    CheckResult {
        name,
        status,
        detail,
        ms: start.elapsed().as_millis() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readyz_pass_on_match() {
        let body = serde_json::json!({"migrations": {"applied": 50, "expected": 50}});
        let (s, d) = eval_readyz(200, &body);
        assert_eq!(s, CheckStatus::Pass);
        assert!(d.contains("50/50"));
    }

    #[test]
    fn readyz_fail_on_mismatch() {
        let body = serde_json::json!({"migrations": {"applied": 46, "expected": 50}});
        let (s, _) = eval_readyz(200, &body);
        assert_eq!(s, CheckStatus::Fail);
    }

    #[test]
    fn readyz_fail_on_503() {
        let (s, _) = eval_readyz(503, &serde_json::Value::Null);
        assert_eq!(s, CheckStatus::Fail);
    }

    #[test]
    fn readyz_accepts_alt_spellings() {
        let body =
            serde_json::json!({"migrations": {"applied_count": 50, "expected_count": 50}});
        let (s, _) = eval_readyz(200, &body);
        assert_eq!(s, CheckStatus::Pass);
    }

    #[test]
    fn graphql_pass_and_fail() {
        let ok = serde_json::json!({"data": {"__typename": "QueryRoot"}});
        assert_eq!(eval_graphql(&ok).0, CheckStatus::Pass);
        let bad = serde_json::json!({"errors": [{"message": "X-Auth-Token header is required!"}]});
        assert_eq!(eval_graphql(&bad).0, CheckStatus::Fail);
    }

    #[test]
    fn blocked_semantics() {
        assert_eq!(eval_blocked("/metrics", 404).0, CheckStatus::Pass);
        assert_eq!(eval_blocked("/metrics", 403).0, CheckStatus::Pass);
        assert_eq!(eval_blocked("/metrics", 200).0, CheckStatus::Fail);
    }
}
