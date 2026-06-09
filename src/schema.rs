//! `knishio schema export` — export the validator's GraphQL schema.
//!
//! `--format sdl` (default) GETs the validator's read-only `/schema` endpoint
//! (the canonical SDL from `schema_sdl()`). `--format json` POSTs the standard
//! GraphQL introspection query to `/graphql` (introspection is public) and dumps
//! the pretty-printed result — useful for client codegen tooling.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::output;

fn http_client(insecure_tls: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("Failed to build HTTP client")
}

pub async fn export(config: &Config, format: &str, output_path: Option<PathBuf>) -> Result<()> {
    let content = match format.to_lowercase().as_str() {
        "sdl" => fetch_sdl(config).await?,
        "json" => fetch_introspection_json(config).await?,
        other => anyhow::bail!("unknown --format '{other}': use 'sdl' or 'json'"),
    };

    match output_path {
        Some(path) => {
            std::fs::write(&path, &content)
                .with_context(|| format!("failed to write {}", path.display()))?;
            output::success(&format!("Schema ({format}) written to {}", path.display()));
        }
        None => {
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

async fn fetch_sdl(config: &Config) -> Result<String> {
    let url = format!("{}/schema", config.validator.url);
    http_client(config.validator.insecure_tls)?
        .get(&url)
        .send()
        .await
        .context("Failed to GET /schema — is the validator running?")?
        .error_for_status()
        .context("/schema returned a non-success status")?
        .text()
        .await
        .context("/schema response was not text")
}

async fn fetch_introspection_json(config: &Config) -> Result<String> {
    let url = format!("{}/graphql", config.validator.url);
    let body = serde_json::json!({ "query": INTROSPECTION_QUERY });
    let resp: serde_json::Value = http_client(config.validator.insecure_tls)?
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to POST introspection to /graphql")?
        .error_for_status()
        .context("/graphql returned a non-success status")?
        .json()
        .await
        .context("introspection response was not valid JSON")?;
    // The validator caps GraphQL query complexity (DoS guard), and the standard
    // introspection query can exceed it ("Query is too complex."). Fail loudly with
    // the GraphQL error rather than dumping an `{"data":null,"errors":[…]}` blob that
    // looks like success — and point at the canonical SDL path.
    if let Some(errors) = resp
        .get("errors")
        .and_then(|e| e.as_array())
        .filter(|a| !a.is_empty())
    {
        let msgs: Vec<String> = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()).map(String::from))
            .collect();
        anyhow::bail!(
            "introspection query rejected by the validator: {}\n\
             (the validator caps GraphQL query complexity; use `--format sdl` — the default — \
             which reads the canonical SDL from GET /schema instead)",
            msgs.join("; ")
        );
    }
    serde_json::to_string_pretty(&resp).context("failed to format introspection JSON")
}

/// The canonical GraphQL introspection query (graphql-js standard).
const INTROSPECTION_QUERY: &str = r#"
query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives { name description locations args { ...InputValue } }
  }
}
fragment FullType on __Type {
  kind name description
  fields(includeDeprecated: true) {
    name description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
  possibleTypes { ...TypeRef }
}
fragment InputValue on __InputValue {
  name description type { ...TypeRef } defaultValue
}
fragment TypeRef on __Type {
  kind name
  ofType { kind name ofType { kind name ofType { kind name
    ofType { kind name ofType { kind name ofType { kind name
      ofType { kind name } } } } } } }
}
"#;
