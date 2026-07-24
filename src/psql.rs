//! psql transport: run SQL against the validator's Postgres either through
//! the LOCAL docker stack (`docker exec psql`, the historical behavior) or a
//! REMOTE host over ssh (`ssh <host> sudo -n -u postgres psql`).
//!
//! This is the structural CLI-2 fix (validator repo
//! docs/audits/TESTNET-DEPLOY-2026-07-23.md): cell/audit administration is
//! database-side, not HTTP — so `--url` never could aim it at a remote
//! deployment, and silently acted on the local stack instead. The transport
//! makes the destination explicit: `--host user@host` for a remote server,
//! `--local` to explicitly choose the local docker stack when the validator
//! URL points elsewhere.
//!
//! SQL is fed via STDIN (`psql -f -`) on both paths — that sidesteps nested
//! shell-quoting entirely for the ssh case and keeps one code path.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Config;

/// Default hint used for "relation does not exist"-class errors.
const DEFAULT_MISSING_HINT: &str =
    "Database operation failed — run `knishio db` to check consistency";

#[derive(Debug, Clone)]
pub enum PsqlTransport {
    /// `docker exec -i <container> psql -U <user> -d <db> …` on the local daemon.
    DockerExec {
        container: String,
        user: String,
        db: String,
    },
    /// `ssh <host> sudo -n -u postgres psql -d <db> …`. Requires the scoped
    /// sudoers grant installed by `knishio deploy bootstrap`.
    Ssh { host: String, db: String },
}

impl PsqlTransport {
    /// Pick the transport from flags + config.
    ///
    /// CLI-2 guard: when the validator URL is non-local and the operator gave
    /// neither `--host` nor `--local`, refuse — the URL *looks* like the
    /// target but psql-side admin cannot travel over it.
    pub fn resolve(cfg: &Config, host: Option<&str>, local: bool) -> Result<Self> {
        if let Some(h) = host {
            return Ok(PsqlTransport::Ssh {
                host: h.to_string(),
                db: cfg.database.name.clone(),
            });
        }
        if !local && !crate::target::is_local_url(&cfg.validator.url) {
            anyhow::bail!(
                "this command administers the DATABASE, not the HTTP API — the \
                 validator URL ({}) cannot be its target. Pass --host <user@host> \
                 to run psql on the server over ssh, or --local to explicitly \
                 target the local docker stack.",
                cfg.validator.url
            );
        }
        Ok(PsqlTransport::DockerExec {
            container: cfg.docker.postgres_container.clone(),
            user: cfg.database.user.clone(),
            db: cfg.database.name.clone(),
        })
    }

    /// Human-readable destination for the TARGET banner.
    pub fn describe(&self) -> String {
        match self {
            PsqlTransport::DockerExec { container, .. } => {
                format!("docker://{} (docker exec psql — local stack)", container)
            }
            PsqlTransport::Ssh { host, .. } => {
                format!("ssh://{} (sudo -u postgres psql)", host)
            }
        }
    }

    /// True when effects land on this machine.
    pub fn is_local(&self) -> bool {
        matches!(self, PsqlTransport::DockerExec { .. })
    }

    /// Run SQL (via stdin) and return trimmed stdout. Errors are scrubbed to
    /// operator-friendly messages; `missing_hint` customizes the
    /// "relation does not exist" case (audit uses its own).
    pub async fn exec_with_hint(&self, sql: &str, missing_hint: &str) -> Result<String> {
        let mut cmd = match self {
            PsqlTransport::DockerExec { container, user, db } => {
                let mut c = Command::new("docker");
                c.args([
                    "exec", "-i", container, "psql", "-U", user, "-d", db, "-q", "-t", "-A",
                    "-f", "-",
                ]);
                c
            }
            PsqlTransport::Ssh { host, db } => {
                let ssh_bin =
                    std::env::var("KNISHIO_SSH_BIN").unwrap_or_else(|_| "ssh".to_string());
                let mut c = Command::new(ssh_bin);
                c.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    host,
                    "sudo",
                    "-n",
                    "-u",
                    "postgres",
                    "psql",
                    "-d",
                    db,
                    "-q",
                    "-t",
                    "-A",
                    "-f",
                    "-",
                ]);
                c
            }
        };

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(match self {
                PsqlTransport::DockerExec { .. } => {
                    "Failed to exec into postgres container — is the stack running?"
                }
                PsqlTransport::Ssh { .. } => "Failed to spawn ssh — is it installed?",
            })?;

        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(sql.as_bytes())
            .await
            .context("Failed to write SQL to psql stdin")?;
        // Close stdin so psql sees EOF.
        drop(child.stdin.take());

        let out = child
            .wait_with_output()
            .await
            .context("Failed to collect psql output")?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Scrub: don't expose raw SQL errors with schema details.
            if stderr.contains("does not exist") {
                anyhow::bail!("{}", missing_hint);
            } else if stderr.contains("connection refused") || stderr.contains("could not connect")
            {
                anyhow::bail!("Cannot connect to database — is the stack running?");
            } else if stderr.contains("Permission denied")
                || stderr.contains("a password is required")
                || stderr.contains("sudo:")
            {
                anyhow::bail!(
                    "Remote psql refused: the scoped sudoers grant for `sudo -u postgres psql` \
                     is missing on the host (installed by `knishio deploy bootstrap`)."
                );
            } else {
                anyhow::bail!("Database operation failed (run with RUST_LOG=debug for details)");
            }
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// [`exec_with_hint`] with the default missing-relation hint.
    pub async fn exec(&self, sql: &str) -> Result<String> {
        self.exec_with_hint(sql, DEFAULT_MISSING_HINT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn resolve_matrix() {
        let mut cfg = Config::default();

        // Local URL, no flags → DockerExec.
        let t = PsqlTransport::resolve(&cfg, None, false).unwrap();
        assert!(t.is_local());

        // Explicit --host → Ssh regardless of URL.
        let t = PsqlTransport::resolve(&cfg, Some("forge@testnet.knish.io"), false).unwrap();
        assert!(!t.is_local());
        assert!(t.describe().contains("ssh://forge@testnet.knish.io"));

        // Remote URL, no flags → the CLI-2 hard error.
        cfg.validator.url = "https://testnet.knish.io".into();
        let err = PsqlTransport::resolve(&cfg, None, false).unwrap_err();
        assert!(err.to_string().contains("--host"), "err: {err}");
        assert!(err.to_string().contains("--local"), "err: {err}");

        // Remote URL + explicit --local → DockerExec (operator opted in).
        let t = PsqlTransport::resolve(&cfg, None, true).unwrap();
        assert!(t.is_local());
    }
}
