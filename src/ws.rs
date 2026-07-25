//! Shared GraphQL-over-WebSocket plumbing (`graphql-transport-ws`).
//!
//! Extracted from `watch.rs` so `verify` can reuse the connect+handshake
//! path: TLS connector construction (with the same insecure-TLS philosophy
//! as the HTTP client), ws URL derivation, and the
//! `connection_init` → `connection_ack` handshake.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ── TLS: insecure verifier for self-signed certs ──────────────────

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

pub fn build_connector(insecure_tls: bool) -> Result<Connector> {
    // rustls 0.23 requires a process-wide crypto provider to be installed
    // before the first ClientConfig is built. `install_default` is
    // idempotent — the only failure mode is "already installed", which is fine.
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

/// Derive a ws(s) URL from the validator's http(s) base URL + path.
pub fn to_ws_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{}", rest)
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{}", rest)
    } else {
        base.to_string()
    };
    format!("{}{}", ws_base, path)
}

/// Connect to `ws_url` with the `graphql-transport-ws` subprotocol and run
/// the `connection_init` → `connection_ack` handshake. Returns the open
/// stream (post-ack) for subscription use, or an error naming what failed.
pub async fn connect_and_ack(ws_url: &str, insecure_tls: bool) -> Result<WsStream> {
    let mut request = ws_url
        .into_client_request()
        .context("failed to build ws request")?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "graphql-transport-ws"
            .parse()
            .expect("static subprotocol string"),
    );

    let connector = Some(build_connector(insecure_tls)?);

    let (mut ws, _resp) =
        connect_async_tls_with_config(request, None, /* disable_nagle */ false, connector)
            .await
            .context("WS connect failed — is the validator running and serving TLS correctly?")?;

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

    let v: serde_json::Value = match &ack {
        Message::Text(t) => serde_json::from_str(t).context("non-JSON ws frame")?,
        other => anyhow::bail!("expected text frame for connection_ack, got: {:?}", other),
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("connection_ack") {
        anyhow::bail!(
            "expected connection_ack, got: {}",
            serde_json::to_string(&v).unwrap_or_default()
        );
    }
    Ok(ws)
}

// ═══════════════════════════════════════════════════════════════
//  Server-message classification + one-shot subscription probe
// ═══════════════════════════════════════════════════════════════

/// A classified `graphql-transport-ws` server message.
///
/// ONE classifier, two consumers — `watch`'s streaming loop and `verify`'s
/// probe. Keeping it single-sourced is deliberate: two hand-written copies of
/// the same protocol logic is exactly how CLI-4 drifted.
pub enum ServerMsg {
    /// A subscription payload for the expected root field.
    Event(serde_json::Value),
    /// The server refused the subscription — GraphQL validation errors (delivered
    /// as `next` with `payload.errors`, then `complete`) or an `error` frame.
    /// This is the CLI-5 signature: no event will ever arrive.
    Rejected(String),
    /// The server ended the subscription (`complete`, or a close frame).
    Completed,
    /// Protocol noise — keepalives, binary frames, unknown types.
    Noise,
}

/// Classify one incoming frame. `root_field` selects the payload to extract.
pub fn classify(msg: &Message, root_field: &str) -> Result<ServerMsg> {
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(t).context("json parse")?,
        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
            return Ok(ServerMsg::Noise)
        }
        Message::Close(_) => return Ok(ServerMsg::Completed),
    };

    Ok(match v.get("type").and_then(|t| t.as_str()) {
        Some("next") => {
            if let Some(event) = v.pointer("/payload/data").and_then(|d| d.get(root_field)) {
                ServerMsg::Event(event.clone())
            } else if let Some(errors) = v.pointer("/payload/errors") {
                ServerMsg::Rejected(errors.to_string())
            } else {
                ServerMsg::Noise
            }
        }
        Some("error") => ServerMsg::Rejected(
            v.get("payload").map(|p| p.to_string()).unwrap_or_default(),
        ),
        Some("complete") => ServerMsg::Completed,
        // "ping"/"connection_ack"/unknown
        _ => ServerMsg::Noise,
    })
}

/// Outcome of a one-shot subscription probe.
pub enum Probe {
    /// An event actually arrived.
    Event,
    /// Subscribed cleanly; nothing emitted inside the window (a healthy idle feed).
    Subscribed,
    /// The server refused the document — the subscription is dead (CLI-5 class).
    Rejected(String),
}

/// Open a subscription, then classify what the server does with it.
///
/// This is `verify`'s runtime net for the CLI-5 bug class: a handshake-only check
/// (`connection_init` → `ack`) reports a green socket even when the server
/// rejects the query document, so the subscription silently yields nothing.
/// Distinguishing "rejected" from "subscribed but idle" requires actually
/// subscribing — an idle DAG legitimately emits nothing, so silence is a PASS
/// while errors / an immediate `complete` are a FAIL.
pub async fn subscribe_and_probe(
    ws_url: &str,
    insecure_tls: bool,
    query: &str,
    variables: serde_json::Value,
    root_field: &str,
    window: std::time::Duration,
) -> Result<Probe> {
    use futures_util::SinkExt;

    let mut ws = connect_and_ack(ws_url, insecure_tls).await?;
    ws.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "id": "probe-1",
            "type": "subscribe",
            "payload": { "query": query, "variables": variables },
        }))
        .expect("static json"),
    ))
    .await
    .context("failed to send subscribe")?;

    let deadline = tokio::time::Instant::now() + window;
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break Probe::Subscribed;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            // Window elapsed with no decisive message: subscribed and idle.
            Err(_) => break Probe::Subscribed,
            Ok(None) => {
                break Probe::Rejected("server closed the connection before any event".into())
            }
            Ok(Some(Err(e))) => break Probe::Rejected(format!("ws error: {e}")),
            Ok(Some(Ok(m))) => match classify(&m, root_field)? {
                ServerMsg::Event(_) => break Probe::Event,
                ServerMsg::Rejected(r) => break Probe::Rejected(r),
                ServerMsg::Completed => {
                    break Probe::Rejected(
                        "server completed the subscription immediately without sending any event"
                            .into(),
                    )
                }
                ServerMsg::Noise => continue,
            },
        }
    };

    let _ = ws.close(None).await;
    Ok(outcome)
}
