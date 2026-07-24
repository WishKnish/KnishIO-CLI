//! Shared HTTP client construction + readiness polling.
//!
//! Every module that talks to the validator over HTTP builds its client here
//! so the TLS posture (`insecure_tls` → `danger_accept_invalid_certs`) and
//! timeout policy live in exactly one place.

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::time::sleep;

/// Default request timeout for one-shot queries.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Shorter timeout for status/introspection endpoints.
pub const SHORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build an HTTP client with optional TLS verification skip.
pub fn client(insecure_tls: bool, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

/// Like [`client`], but never follows redirects. Needed by checks that assert
/// on a redirect itself (e.g. verify's http→https 301 check) — the default
/// policy would follow it and mask the status.
pub fn client_no_redirect(insecure_tls: bool, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

/// Poll `{base_url}/readyz` until it returns HTTP 200 or `timeout` elapses.
/// Returns `true` on ready. The validator returns 503 while migrations are
/// pending, so a plain success-status check is the correct readiness gate.
pub async fn wait_for_ready(
    base_url: &str,
    insecure_tls: bool,
    timeout: Duration,
    interval: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let url = format!("{}/readyz", base_url.trim_end_matches('/'));
    while tokio::time::Instant::now() < deadline {
        if let Ok(c) = client(insecure_tls, SHORT_TIMEOUT) {
            if let Ok(resp) = c.get(&url).send().await {
                if resp.status().is_success() {
                    return true;
                }
            }
        }
        sleep(interval).await;
    }
    false
}
