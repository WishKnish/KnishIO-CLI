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
    bundleHash
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

    // Events actually delivered — distinguishes "idle but healthy" from
    // "subscription was rejected and will never deliver anything".
    let mut streamed: usize = 0;

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
                    Some(Ok(m)) => match handle_message(m, root_field) {
                        Ok(Flow::Streamed) => streamed += 1,
                        Ok(Flow::Continue) => {}
                        // The server ended the subscription. If it never
                        // delivered an event, this is a FAILURE — previously the
                        // loop kept waiting on a socket that would never produce
                        // anything, so a rejected subscription looked like a
                        // healthy idle stream until an external timeout killed it.
                        Ok(Flow::Ended { reason }) => {
                            let _ = ws.close(None).await;
                            return match (streamed, reason) {
                                (0, Some(r)) => Err(anyhow::anyhow!(
                                    "subscription to `{root_field}` was rejected by the \
                                     server and streamed no events: {r}"
                                )),
                                (0, None) => Err(anyhow::anyhow!(
                                    "server completed the `{root_field}` subscription \
                                     immediately without sending any event"
                                )),
                                (n, _) => {
                                    output::info(&format!(
                                        "Subscription ended after {n} event(s)."
                                    ));
                                    Ok(())
                                }
                            };
                        }
                        Err(e) => output::warn(&format!("skipping malformed message: {e}")),
                    },
                    Some(Err(e)) => {
                        output::error(&format!("WS error: {e}"));
                        return Err(anyhow::anyhow!(e));
                    }
                    None => {
                        if streamed == 0 {
                            return Err(anyhow::anyhow!(
                                "server closed the connection before sending any \
                                 `{root_field}` event"
                            ));
                        }
                        output::warn("Server closed the connection.");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// What a received message means for the streaming loop.
enum Flow {
    /// Protocol noise / keepalive — keep waiting.
    Continue,
    /// A real event was printed to stdout.
    Streamed,
    /// The server ended the subscription. `reason` is `Some` when the server
    /// reported an error, which is fatal — no events will ever arrive.
    Ended { reason: Option<String> },
}

/// Turn one incoming WS message into a stdout JSON line, or a no-op
/// for protocol control frames.
fn handle_message(msg: Message, root_field: &str) -> Result<Flow> {
    let v = match msg {
        Message::Text(t) => serde_json::from_str::<Value>(&t).context("json parse")?,
        Message::Binary(_) => return Ok(Flow::Continue), // GraphQL-WS uses text frames
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(Flow::Continue),
        Message::Close(_) => {
            output::warn("Server sent close frame.");
            return Ok(Flow::Ended { reason: None });
        }
    };

    Ok(match v.get("type").and_then(|t| t.as_str()) {
        Some("next") => {
            // payload.data.<root_field> — our streamable event.
            if let Some(event) = v
                .pointer("/payload/data")
                .and_then(|d| d.get(root_field))
            {
                // JSON-per-line on stdout — jq/tool-friendly.
                println!("{}", serde_json::to_string(event).unwrap_or_default());
                Flow::Streamed
            } else if let Some(errors) = v.pointer("/payload/errors") {
                // async-graphql delivers query-validation failures here, then
                // `complete` — fatal for this subscription.
                output::error(&format!("server error: {}", errors));
                Flow::Ended {
                    reason: Some(errors.to_string()),
                }
            } else {
                Flow::Continue
            }
        }
        Some("error") => {
            let payload = v.get("payload").map(|p| p.to_string()).unwrap_or_default();
            output::error(&format!("subscription error: {}", payload));
            Flow::Ended {
                reason: Some(payload),
            }
        }
        Some("complete") => {
            output::info("Server signalled subscription complete.");
            Flow::Ended { reason: None }
        }
        Some("ping") => {
            // graphql-transport-ws keepalive — no response required
            // (server usually just sends periodically).
            Flow::Continue
        }
        _ => {
            // Unknown message type — ignore, don't clutter output.
            Flow::Continue
        }
    })
}



#[cfg(test)]
mod tests {
    use super::*;

    fn text(json: &str) -> Message {
        Message::Text(json.to_string())
    }

    /// Guards CLI-5 half 1: the DAG subscription must select the field name the
    /// validator actually exposes (`bundleHash` via #[graphql(name=…)]), not the
    /// Rust-side `bundle`. Selecting `bundle` made GraphQL reject the whole
    /// document, so `watch dag` could never emit a single event.
    #[test]
    fn dag_query_uses_wire_field_names() {
        assert!(DAG_QUERY.contains("bundleHash"), "must select bundleHash");
        // No bare `bundle` selection (the bug). Check as a standalone line.
        assert!(
            !DAG_QUERY.lines().any(|l| l.trim() == "bundle"),
            "bare `bundle` is not a DagChange wire field: {DAG_QUERY}"
        );
    }

    /// Guards CLI-5 half 2: a rejected subscription must END the loop with a
    /// reason (→ non-zero exit), not be logged and waited on forever.
    #[test]
    fn query_errors_end_the_subscription() {
        let msg = text(
            r#"{"type":"next","payload":{"errors":[{"message":"Unknown field \"bundle\""}]}}"#,
        );
        match handle_message(msg, "dagChanges").unwrap() {
            Flow::Ended { reason: Some(r) } => assert!(r.contains("Unknown field")),
            _ => panic!("query errors must end the subscription with a reason"),
        }
    }

    #[test]
    fn protocol_frames_and_events_flow_correctly() {
        // `complete` ends without a reason (normal teardown / immediate completion).
        assert!(matches!(
            handle_message(text(r#"{"type":"complete"}"#), "dagChanges").unwrap(),
            Flow::Ended { reason: None }
        ));
        // keepalive is noise
        assert!(matches!(
            handle_message(text(r#"{"type":"ping"}"#), "dagChanges").unwrap(),
            Flow::Continue
        ));
        // a real event counts as streamed
        assert!(matches!(
            handle_message(
                text(r#"{"type":"next","payload":{"data":{"dagChanges":{"eventType":"MOLECULE_ACCEPTED"}}}}"#),
                "dagChanges"
            )
            .unwrap(),
            Flow::Streamed
        ));
        // explicit protocol error ends it
        assert!(matches!(
            handle_message(text(r#"{"type":"error","payload":[{"message":"boom"}]}"#), "dagChanges").unwrap(),
            Flow::Ended { reason: Some(_) }
        ));
    }
}
