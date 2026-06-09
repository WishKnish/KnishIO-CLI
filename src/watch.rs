//! `knishio watch <subject>` — live GraphQL subscription streaming.
//!
//! Connects to the validator's `/graphql/ws` endpoint using the modern
//! `graphql-transport-ws` subprotocol (as implemented by
//! `async-graphql-axum 7.x`). Streams subscription `next` events to
//! stdout as JSON-per-line — jq-friendly by design so operators can
//! pipe:
//!
//!     knishio watch embeddings | jq -r '.metaType + " " + .metaId'
//!
//! Ctrl-C sends a graceful `complete` + closes the socket cleanly so
//! the server-side subscription isn't left dangling.
//!
//! Two subjects:
//!   * `embeddings` — DataBraid embedding-pipeline events
//!     (`embeddingChanges` subscription, in-process broadcast).
//!   * `dag` — DAG structure events (`dagChanges`, in-process broadcast).
//!
//! These are the validator's only subscriptions. (Molecule/wallet change
//! feeds previously planned over Supabase Realtime were removed in the
//! 2026-05-29 Supabase scrub; if reintroduced, build them over Postgres
//! LISTEN/NOTIFY and they'll surface here the same way.)

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use crate::config::Config;
use crate::output;

// ── TLS: insecure verifier for self-signed certs ──────────────────
//
// The validator serves TLS 1.3 via rustls server-side. We match with
// rustls client-side. When operators set insecure_tls = true, we
// install a verifier that accepts anything — same philosophy as
// reqwest's `danger_accept_invalid_certs`, just expressed in rustls
// 0.23's typed form.

#[derive(Debug)]
struct AcceptAnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
            ED448,
        ]
    }
}

fn build_connector(insecure_tls: bool) -> Result<Connector> {
    // rustls 0.23 requires a process-wide crypto provider to be
    // installed before the first ClientConfig is built. `install_default`
    // is idempotent — subsequent calls return Err silently. We don't
    // care about the return value; the only failure mode is "someone
    // else already installed a different provider", which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = if insecure_tls {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(Connector::Rustls(Arc::new(config)))
}

// Subscription query strings. Field lists mirror
// `src/graphql/subscriptions.rs` struct members at the time of writing;
// all fields are asked for so the streamed JSON is self-describing.
// Field names use the GraphQL camelCase wire form (per `#[graphql(name
// = "metaType")]` attrs on the server).
const EMBEDDINGS_QUERY: &str = r#"
subscription Watch($metaType: String, $metaId: String) {
  embeddingChanges(metaType: $metaType, metaId: $metaId) {
    metaType
    metaId
    key
    state
    model
    embeddedAt
    molecularHash
  }
}
"#;

const DAG_QUERY: &str = r#"
subscription Watch($cellSlug: String) {
  dagChanges(cellSlug: $cellSlug) {
    eventType
    molecularHash
    status
    height
    cellSlug
    bondHash
    createdAt
    bundle
    bondType
  }
}
"#;

// CreateMolecule (WP-016): per-bundle molecule-status feed — the full molecule
// (status + atoms) as it is accepted. Field names are the camelCase wire form
// from `src/graphql/types/{subscriptions,molecule}.rs`; `bundle` is required.
const MOLECULES_QUERY: &str = r#"
subscription Watch($bundle: String!) {
  CreateMolecule(bundle: $bundle) {
    molecularHash
    status
    cellSlug
    bundleHash
    counterparty
    height
    depth
    createdAt
    reason
    atoms {
      isotope
      position
      tokenSlug
      value
      index
      metaType
      metaId
    }
  }
}
"#;

/// Public entry: `knishio watch embeddings`.
pub async fn embeddings(
    cfg: &Config,
    meta_type: Option<String>,
    meta_id: Option<String>,
) -> Result<()> {
    let variables = json!({
        "metaType": meta_type,
        "metaId": meta_id,
    });
    run_subscription(cfg, EMBEDDINGS_QUERY, variables, "embeddingChanges").await
}

/// Public entry: `knishio watch dag`.
pub async fn dag(cfg: &Config, cell_slug: Option<String>) -> Result<()> {
    let variables = json!({
        "cellSlug": cell_slug,
    });
    run_subscription(cfg, DAG_QUERY, variables, "dagChanges").await
}

/// Public entry: `knishio watch molecules --bundle <hash>`.
pub async fn molecules(cfg: &Config, bundle: String) -> Result<()> {
    let variables = json!({
        "bundle": bundle,
    });
    run_subscription(cfg, MOLECULES_QUERY, variables, "CreateMolecule").await
}

// ── Subscription driver ─────────────────────────────────────────────

async fn run_subscription(
    cfg: &Config,
    query: &str,
    variables: Value,
    root_field: &str,
) -> Result<()> {
    let ws_url = to_ws_url(&cfg.validator.url);
    output::info(&format!("Connecting to {} …", ws_url));

    // Build a request with the required subprotocol header. Clients
    // that skip this header get rejected by async-graphql-axum's
    // upgrade handler.
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .context("failed to build ws request")?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "graphql-transport-ws"
            .parse()
            .expect("static subprotocol string"),
    );

    let connector = Some(build_connector(cfg.validator.insecure_tls)?);

    let (mut ws, _resp) =
        connect_async_tls_with_config(request, None, /* disable_nagle */ false, connector)
            .await
            .context("WS connect failed — is the validator running and serving TLS correctly?")?;

    // Handshake: send connection_init, expect connection_ack.
    ws.send(Message::Text(
        serde_json::to_string(&json!({"type": "connection_init", "payload": {}}))
            .expect("static json"),
    ))
    .await
    .context("failed to send connection_init")?;

    let ack = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .context("timed out waiting for connection_ack (5s)")?
        .ok_or_else(|| anyhow::anyhow!("WS closed before sending connection_ack"))?
        .context("error reading connection_ack")?;
    match parse_text(&ack)? {
        v if v.get("type").and_then(|t| t.as_str()) == Some("connection_ack") => {}
        other => {
            anyhow::bail!(
                "expected connection_ack, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    // Subscribe.
    let sub_id = "sub-1".to_string();
    let sub_msg = json!({
        "id": sub_id,
        "type": "subscribe",
        "payload": {
            "query": query,
            "variables": variables,
        },
    });
    ws.send(Message::Text(
        serde_json::to_string(&sub_msg).expect("static json"),
    ))
    .await
    .context("failed to send subscribe")?;

    output::info(&format!(
        "Subscribed to {}; streaming events (Ctrl-C to stop)…",
        root_field
    ));

    // Consume messages until Ctrl-C or server closes.
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);

    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                // Send `complete` for graceful teardown, then close.
                let _ = ws
                    .send(Message::Text(
                        serde_json::to_string(&json!({"id": sub_id, "type": "complete"}))
                            .expect("static json"),
                    ))
                    .await;
                let _ = ws.close(None).await;
                output::info("\nSubscription closed.");
                return Ok(());
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(m)) => {
                        if let Err(e) = handle_message(m, root_field) {
                            output::warn(&format!("skipping malformed message: {e}"));
                        }
                    }
                    Some(Err(e)) => {
                        output::error(&format!("WS error: {e}"));
                        return Err(anyhow::anyhow!(e));
                    }
                    None => {
                        output::warn("Server closed the connection.");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Turn one incoming WS message into a stdout JSON line, or a no-op
/// for protocol control frames.
fn handle_message(msg: Message, root_field: &str) -> Result<()> {
    let v = match msg {
        Message::Text(t) => serde_json::from_str::<Value>(&t).context("json parse")?,
        Message::Binary(_) => return Ok(()), // GraphQL-WS uses text frames
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(()),
        Message::Close(_) => {
            output::warn("Server sent close frame.");
            return Ok(());
        }
    };

    match v.get("type").and_then(|t| t.as_str()) {
        Some("next") => {
            // payload.data.<root_field> — our streamable event.
            if let Some(event) = v
                .pointer("/payload/data")
                .and_then(|d| d.get(root_field))
            {
                // JSON-per-line on stdout — jq/tool-friendly.
                println!("{}", serde_json::to_string(event).unwrap_or_default());
            } else if let Some(errors) = v.pointer("/payload/errors") {
                output::warn(&format!("server error: {}", errors));
            }
        }
        Some("error") => {
            output::error(&format!(
                "subscription error: {}",
                v.get("payload").map(|p| p.to_string()).unwrap_or_default()
            ));
        }
        Some("complete") => {
            output::info("Server signalled subscription complete.");
        }
        Some("ping") => {
            // graphql-transport-ws keepalive — no response required
            // (server usually just sends periodically).
        }
        _ => {
            // Unknown message type — ignore, don't clutter output.
        }
    }
    Ok(())
}

fn parse_text(msg: &Message) -> Result<Value> {
    match msg {
        Message::Text(t) => serde_json::from_str::<Value>(t).context("json parse"),
        _ => Err(anyhow::anyhow!("expected text frame, got {:?}", msg)),
    }
}

/// Transform `https://host:port` → `wss://host:port/graphql/ws`
/// (or `http://` → `ws://`).
fn to_ws_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let with_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{}", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{}", rest)
    } else {
        // Assume wss:// if no scheme — matches the rest of the CLI's
        // "validator defaults to TLS" stance.
        format!("wss://{}", trimmed)
    };
    format!("{}/graphql/ws", with_scheme)
}
