//! Target model: state honestly where a command's effects land, and gate
//! mutations aimed at anything that isn't the local machine.
//!
//! Born from the 2026-07-23 testnet deployment goose chase (CLI-2 in the
//! validator repo's docs/audits/TESTNET-DEPLOY-2026-07-23.md): an operator
//! passed `--url https://testnet.knish.io` to `cell create`, but cell admin
//! runs over `docker exec psql` — the mutation landed on the local dev stack
//! while appearing to target production. Two rules prevent a repeat:
//!
//! 1. Every command prints a `TARGET:` banner naming its true transport —
//!    the HTTP URL (with the source that won precedence), the docker
//!    container, or the ssh host. Never the URL for a non-HTTP command.
//! 2. Mutating commands aimed at a non-local target require confirmation
//!    (or `--yes`); non-interactive runs fail fast instead of hanging.

use anyhow::Result;
use colored::Colorize;
use std::io::{BufRead, IsTerminal, Write};

/// The compiled-in default validator URL (also the clap default before 0.2.0).
pub const DEFAULT_URL: &str = "https://localhost:8080";

/// Where the effective validator URL came from, by precedence:
/// `--url` flag > `KNISHIO_URL` env > `knishio.toml` > compiled default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UrlSource {
    Flag,
    Env,
    ConfigFile,
    #[default]
    Default,
}

impl UrlSource {
    pub fn label(&self) -> &'static str {
        match self {
            UrlSource::Flag => "--url flag",
            UrlSource::Env => "KNISHIO_URL env",
            UrlSource::ConfigFile => "knishio.toml",
            UrlSource::Default => "default",
        }
    }
}

/// True when the URL's host is unambiguously this machine.
pub fn is_local_url(url: &str) -> bool {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("ws://")
        .trim_start_matches("wss://");
    // Strip path, then port. IPv6 literals keep their brackets for the match.
    let host = host.split('/').next().unwrap_or(host);
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| &h[1..]).unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
}

/// Print the target banner for an HTTP-transport command.
pub fn banner_http(url: &str, source: UrlSource) {
    eprintln!(
        "{} {}  {}",
        "→ TARGET:".bold(),
        url.cyan().bold(),
        format!("({})", source.label()).dimmed()
    );
}

/// Print the target banner for a non-HTTP transport (docker exec, compose,
/// ssh). `transport` examples: `docker://knishio-postgres (docker exec psql)`,
/// `docker compose (standalone)`, `ssh://forge@host (sudo -u postgres psql)`.
pub fn banner_transport(transport: &str) {
    eprintln!("{} {}", "→ TARGET:".bold(), transport.cyan().bold());
}

/// Gate a mutating action aimed at a non-local target.
///
/// - `--yes` (or a local target) passes silently.
/// - Interactive TTY: y/N prompt naming the target.
/// - Non-interactive without `--yes`: hard error (never hangs CI/scripts).
pub fn confirm_mutation(action: &str, target_desc: &str, is_local: bool, yes: bool) -> Result<()> {
    if is_local || yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to {} against non-local target {} without confirmation \
             (non-interactive session). Re-run with --yes to proceed.",
            action,
            target_desc
        );
    }
    eprint!(
        "{} {} against {} — proceed? [y/N] ",
        "⚠".yellow().bold(),
        action,
        target_desc.cyan().bold()
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        Ok(())
    } else {
        anyhow::bail!("aborted by operator (answer was not 'y')")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_local_cases() {
        assert!(is_local_url("https://localhost:8080"));
        assert!(is_local_url("http://127.0.0.1"));
        assert!(is_local_url("https://[::1]:8080"));
        assert!(is_local_url("https://0.0.0.0:8080/graphql"));
        assert!(!is_local_url("https://testnet.knish.io"));
        assert!(!is_local_url("https://testnet.knish.io:8080/graphql"));
        assert!(!is_local_url("http://10.0.0.5:8080"));
    }

    #[test]
    fn confirm_passes_local_and_yes_without_prompt() {
        confirm_mutation("submit molecules", "https://remote", true, false).unwrap();
        confirm_mutation("submit molecules", "https://remote", false, true).unwrap();
    }
}
