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
//! Six subjects, one per subscription the validator implements:
//!   * `embeddings`     — DataBraid embedding-pipeline events (`embeddingChanges`)
//!   * `dag`            — DAG structure events (`dagChanges`)
//!   * `molecules`      — per-bundle molecule status + atoms (`CreateMolecule`)
//!   * `wallet-status`  — per-bundle/token admission + balance (`WalletStatus`)
//!   * `active-user`    — active-session changes for a meta pair (`ActiveUser`)
//!   * `active-wallet`  — wallet changes for a bundle (`ActiveWallet`)
//!
//! All six are in-process broadcast channels on the validator, so they work in
//! every deployment. Delivery is cell-gated server-side (SEC-010 P4-sub).
//!
//! ⚠️ Selection sets MUST use the GraphQL **wire** names, which can differ from
//! the validator's Rust field names via `#[graphql(name = …)]` — e.g. `DagChange`
//! exposes `bundleHash`, not `bundle`. Getting this wrong makes the server reject
//! the whole document and the subscription yields nothing (CLI-5, fixed in 0.2.2).
//! The `schema_contract` tests at the bottom of this file now validate every
//! query below against the validator's SDL (vendored at
//! `tests/validator-schema.graphql`), so that class of bug fails the build
//! instead of shipping.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::config::Config;
use crate::output;

/// One watchable subscription: the CLI subject name, the GraphQL root field, and
/// the document sent on the wire.
pub(crate) struct SubscriptionSpec {
    pub subject: &'static str,
    pub root: &'static str,
    pub query: &'static str,
}

/// Every subscription the CLI can watch — the single source of truth for both
/// dispatch and the `schema_contract` tests. Adding an entry here automatically
/// puts its selection set under contract test, so a wire-name mismatch (CLI-5)
/// cannot reach a release via an unregistered query.
pub(crate) const SUBSCRIPTIONS: &[SubscriptionSpec] = &[
    SubscriptionSpec { subject: "embeddings", root: "embeddingChanges", query: EMBEDDINGS_QUERY },
    SubscriptionSpec { subject: "dag", root: "dagChanges", query: DAG_QUERY },
    SubscriptionSpec { subject: "molecules", root: "CreateMolecule", query: MOLECULES_QUERY },
    SubscriptionSpec { subject: "wallet-status", root: "WalletStatus", query: WALLET_STATUS_QUERY },
    SubscriptionSpec { subject: "active-user", root: "ActiveUser", query: ACTIVE_USER_QUERY },
    SubscriptionSpec { subject: "active-wallet", root: "ActiveWallet", query: ACTIVE_WALLET_QUERY },
];

// Subscription query strings. Field names use the GraphQL camelCase wire form
// (per the `#[graphql(name = …)]` attrs on the server); all fields are asked for
// so the streamed JSON is self-describing.
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

pub(crate) const DAG_QUERY: &str = r#"
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

// WalletStatus (SEC-010 P4-sub): per-bundle/token admission + balance changes.
const WALLET_STATUS_QUERY: &str = r#"
subscription Watch($bundle: String!, $token: String!) {
  WalletStatus(bundle: $bundle, token: $token) {
    bundle
    token
    admission
    balance
  }
}
"#;

// ActiveUser: active-session changes for a metaType/metaId pair.
const ACTIVE_USER_QUERY: &str = r#"
subscription Watch($metaType: String!, $metaId: String!) {
  ActiveUser(metaType: $metaType, metaId: $metaId) {
    bundleHash
    metaType
    metaId
    cellSlug
    jsonData
    lastActive
    createdAt
    updatedAt
  }
}
"#;

// ActiveWallet: wallet changes for a bundle. Returns the full `Wallet` type;
// we select scalars plus shallow nesting and deliberately OMIT `metas` /
// `tokenUnits` / `tradeRates` so each streamed line stays jq-friendly.
const ACTIVE_WALLET_QUERY: &str = r#"
subscription Watch($bundle: String!) {
  ActiveWallet(bundle: $bundle) {
    address
    position
    isShadow
    bundleHash
    tokenSlug
    balance
    batchId
    amount
    type
    createdAt
    updatedAt
    token {
      slug
      name
      fungibility
    }
    walletBundle {
      bundleHash
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

/// Public entry: `knishio watch wallet-status --bundle <hash> --token <slug>`.
pub async fn wallet_status(cfg: &Config, bundle: String, token: String) -> Result<()> {
    let variables = json!({
        "bundle": bundle,
        "token": token,
    });
    run_subscription(cfg, WALLET_STATUS_QUERY, variables, "WalletStatus").await
}

/// Public entry: `knishio watch active-user --meta-type <t> --meta-id <i>`.
pub async fn active_user(cfg: &Config, meta_type: String, meta_id: String) -> Result<()> {
    let variables = json!({
        "metaType": meta_type,
        "metaId": meta_id,
    });
    run_subscription(cfg, ACTIVE_USER_QUERY, variables, "ActiveUser").await
}

/// Public entry: `knishio watch active-wallet --bundle <hash>`.
pub async fn active_wallet(cfg: &Config, bundle: String) -> Result<()> {
    let variables = json!({
        "bundle": bundle,
    });
    run_subscription(cfg, ACTIVE_WALLET_QUERY, variables, "ActiveWallet").await
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
///
/// Classification itself lives in `crate::ws::classify` — shared with `verify`'s
/// subscription probe so the two can't drift. This function adds only the
/// operator-facing output and the streaming loop's control flow.
fn handle_message(msg: Message, root_field: &str) -> Result<Flow> {
    Ok(match crate::ws::classify(&msg, root_field)? {
        crate::ws::ServerMsg::Event(event) => {
            // JSON-per-line on stdout — jq/tool-friendly.
            println!("{}", serde_json::to_string(&event).unwrap_or_default());
            Flow::Streamed
        }
        crate::ws::ServerMsg::Rejected(reason) => {
            output::error(&format!("server error: {}", reason));
            Flow::Ended { reason: Some(reason) }
        }
        crate::ws::ServerMsg::Completed => {
            output::info("Server signalled subscription complete.");
            Flow::Ended { reason: None }
        }
        crate::ws::ServerMsg::Noise => Flow::Continue,
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

// ═══════════════════════════════════════════════════════════════
//  Schema contract: every subscription query vs the validator SDL
// ═══════════════════════════════════════════════════════════════
//
// This is the build-time net for the CLI-5 bug class. A selection set that names
// a field the server doesn't expose (e.g. `bundle` where the wire name is
// `bundleHash`) makes the server reject the entire document, so the subscription
// silently yields nothing. Validating against the SDL catches it because the SDL
// *is* the wire contract — `#[graphql(name = …)]` renames are already applied.

#[cfg(test)]
mod schema_contract {
    use super::{SubscriptionSpec, SUBSCRIPTIONS};
    use std::collections::HashMap;

    /// One node of a parsed GraphQL selection set.
    #[derive(Debug)]
    struct Sel {
        name: String,
        children: Vec<Sel>,
    }

    /// Remove `"""…"""` doc blocks — they may contain braces and prose that
    /// would otherwise parse as fields.
    fn strip_docs(src: &str) -> String {
        let mut s = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("\"\"\"") {
            s.push_str(&rest[..i]);
            rest = &rest[i + 3..];
            match rest.find("\"\"\"") {
                Some(j) => rest = &rest[j + 3..],
                None => { rest = ""; break; }
            }
        }
        s.push_str(rest);
        s
    }

    /// Remove `(…)` groups, depth-aware. Load-bearing on BOTH sides: query
    /// arguments (`dagChanges(cellSlug: $x)`) and SDL field arguments
    /// (`dagChanges(cellSlug: String): DagChange!` — whose first colon is INSIDE
    /// the parens, so splitting on it without this yields the argument's type).
    fn strip_parens(src: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in src.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// Strip doc blocks and argument groups, then tokenize into identifiers and braces.
    fn tokenize(src: &str) -> Vec<String> {
        let out = strip_parens(&strip_docs(src));
        out.replace('{', " { ")
            .replace('}', " } ")
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    /// Parse a subscription document into (root field name, its selection set).
    fn parse_query(query: &str) -> (String, Vec<Sel>) {
        let toks = tokenize(query);
        // Skip the operation header up to its opening brace.
        let body = toks.iter().position(|t| t == "{").expect("query has a body");
        let mut i = body + 1;
        let root = toks[i].clone();
        i += 1;
        assert_eq!(toks[i], "{", "root field must have a selection set");
        i += 1;
        let (sels, _) = parse_sels(&toks, i);
        (root, sels)
    }

    /// Parse selections until the matching close brace; returns (selections, index-after).
    fn parse_sels(toks: &[String], mut i: usize) -> (Vec<Sel>, usize) {
        let mut out = Vec::new();
        while i < toks.len() {
            match toks[i].as_str() {
                "}" => return (out, i + 1),
                "{" => panic!("unexpected brace at token {i}"),
                name => {
                    let name = name.to_string();
                    i += 1;
                    if i < toks.len() && toks[i] == "{" {
                        let (children, next) = parse_sels(toks, i + 1);
                        out.push(Sel { name, children });
                        i = next;
                    } else {
                        out.push(Sel { name, children: Vec::new() });
                    }
                }
            }
        }
        (out, i)
    }

    /// SDL `type NAME { … }` blocks → field name → bare type name.
    fn sdl_types(sdl: &str) -> HashMap<String, HashMap<String, String>> {
        // Drop doc blocks first so their prose can't look like fields.
        let docs_free = strip_docs(sdl);

        let mut types = HashMap::new();
        let mut cur: Option<(String, HashMap<String, String>)> = None;
        for line in docs_free.lines() {
            // Strip argument groups so `f(a: X): Y` splits on the RIGHT colon.
            let stripped = strip_parens(line);
            let t = stripped.trim();
            if let Some(rest) = t.strip_prefix("type ") {
                let name = rest.split_whitespace().next().unwrap_or("").to_string();
                cur = Some((name, HashMap::new()));
                continue;
            }
            if t == "}" {
                if let Some((n, f)) = cur.take() {
                    types.insert(n, f);
                }
                continue;
            }
            if let Some((name, fields)) = cur.as_mut() {
                let _ = name;
                // `field: Type`, `field(args…): Type`
                if let Some((lhs, rhs)) = t.split_once(':') {
                    let fname = lhs.split('(').next().unwrap_or("").trim();
                    if !fname.is_empty() && fname.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        let ftype = rhs
                            .trim()
                            .trim_start_matches('[')
                            .split(|c: char| c == '!' || c == ']' || c == ' ')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        fields.insert(fname.to_string(), ftype);
                    }
                }
            }
        }
        types
    }

    fn vendored_sdl() -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/validator-schema.graphql");
        std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("vendored SDL missing at {}: {e}", p.display()))
    }

    /// Recursively assert every selected field exists on `type_name`.
    fn check(
        types: &HashMap<String, HashMap<String, String>>,
        type_name: &str,
        sels: &[Sel],
        path: &str,
        errs: &mut Vec<String>,
    ) {
        let Some(fields) = types.get(type_name) else {
            errs.push(format!("{path}: SDL has no type `{type_name}`"));
            return;
        };
        for s in sels {
            match fields.get(&s.name) {
                None => errs.push(format!(
                    "{path}.{}: field not on type `{type_name}` — wire-name mismatch? \
                     (available: {})",
                    s.name,
                    {
                        let mut v: Vec<&str> = fields.keys().map(String::as_str).collect();
                        v.sort();
                        v.join(", ")
                    }
                )),
                Some(child_type) if !s.children.is_empty() => check(
                    types,
                    child_type,
                    &s.children,
                    &format!("{path}.{}", s.name),
                    errs,
                ),
                Some(_) => {}
            }
        }
    }

    /// THE CLI-5 GUARD: every registered subscription's selection set must match
    /// the validator's SDL exactly, including nested selections.
    #[test]
    fn every_subscription_query_matches_validator_sdl() {
        let sdl = vendored_sdl();
        let types = sdl_types(&sdl);
        let sub_root = types
            .get("SubscriptionRoot")
            .expect("SDL defines SubscriptionRoot");

        let mut errs = Vec::new();
        for SubscriptionSpec { subject, root, query } in SUBSCRIPTIONS {
            let (parsed_root, sels) = parse_query(query);
            if &parsed_root != root {
                errs.push(format!(
                    "{subject}: registry says root `{root}` but the query selects `{parsed_root}`"
                ));
                continue;
            }
            match sub_root.get(*root) {
                None => errs.push(format!(
                    "{subject}: `{root}` is not a field of SubscriptionRoot"
                )),
                Some(ret) => check(&types, ret, &sels, subject, &mut errs),
            }
        }
        assert!(errs.is_empty(), "schema contract violations:\n  - {}", errs.join("\n  - "));
    }

    /// Sanity: the registry covers every subscription the validator exposes, so a
    /// newly added server subscription surfaces here instead of going unnoticed.
    #[test]
    fn registry_covers_every_validator_subscription() {
        let sdl = vendored_sdl();
        let types = sdl_types(&sdl);
        let sub_root = types.get("SubscriptionRoot").expect("SubscriptionRoot");
        let registered: Vec<&str> = SUBSCRIPTIONS.iter().map(|s| s.root).collect();
        let missing: Vec<&String> = sub_root
            .keys()
            .filter(|k| !registered.contains(&k.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "validator exposes subscriptions the CLI cannot watch: {missing:?} \
             (add them to SUBSCRIPTIONS + WatchSubject, or document the gap)"
        );
    }

    /// Monorepo-only: the vendored SDL must not drift from the validator's
    /// committed baseline. Skips cleanly outside the monorepo (e.g. crates.io CI
    /// checkouts), where the contract test above still runs against the vendor.
    #[test]
    fn vendored_sdl_matches_validator_repo() {
        let sibling = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../servers/knishio-validator-rust/tests/schema.graphql");
        if !sibling.exists() {
            eprintln!("skipping drift check: not in the monorepo ({})", sibling.display());
            return;
        }
        let theirs = std::fs::read_to_string(&sibling).expect("read validator SDL");
        assert_eq!(
            vendored_sdl(),
            theirs,
            "tests/validator-schema.graphql has drifted from the validator's \
             committed SDL — re-copy it: cp {} tests/validator-schema.graphql",
            sibling.display()
        );
    }
}
