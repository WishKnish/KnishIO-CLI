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
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::config::Config;
use crate::output;


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
    let ws_url = crate::ws::to_ws_url(&cfg.validator.url, "/graphql/ws");
    output::info(&format!("Connecting to {} …", ws_url));

    // Connect + graphql-transport-ws handshake (shared with `verify`).
    let mut ws = crate::ws::connect_and_ack(&ws_url, cfg.validator.insecure_tls).await?;

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


