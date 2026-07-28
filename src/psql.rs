//! psql transport: run SQL against the validator's Postgres through one of three
//! mechanisms — the LOCAL docker stack (`docker exec psql`, the historical
//! behavior), a LOCAL bare-metal Postgres (`sudo -n -u postgres psql`, CLI-7), or a
//! REMOTE host over ssh (`ssh <host> sudo -n -u postgres psql`).
//!
//! This is the structural CLI-2 fix (validator repo
//! docs/audits/TESTNET-DEPLOY-2026-07-23.md): cell/audit administration is
//! database-side, not HTTP — so `--url` never could aim it at a remote
//! deployment, and silently acted on the local stack instead. The transport
//! makes the destination explicit: `--host user@host` for a remote server,
//! `--local` to explicitly choose this machine when the validator URL points
//! elsewhere.
//!
//! CLI-7 added the bare-metal variant because "local" had been hardcoded to mean
//! "docker": on a runbook host (PGDG Postgres, no Docker) every cell/audit command
//! dead-ended on "is the stack running?" for a stack that cannot exist there.
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
    /// `sudo -n -u postgres psql -d <db> …` on THIS machine — a bare-metal
    /// deployment (CLI-7).
    ///
    /// The runbook's hosts run PGDG PostgreSQL directly with no Docker at all, so
    /// `DockerExec` — previously the only "local" option — could never work there:
    /// every invocation dead-ended on "is the stack running?" for a stack that does
    /// not exist. This is the same command [`Ssh`] runs remotely, minus the hop,
    /// and exactly what the `(postgres) NOPASSWD: /usr/bin/psql` grant that
    /// `deploy bootstrap` already installs permits.
    LocalPsql { db: String },
}

/// Which local psql mechanism is usable, as observed on this machine.
///
/// Split out so [`select_local`] stays pure and every branch is unit-testable without
/// a Docker daemon or a Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCapabilities {
    /// The configured postgres container exists (running *or* stopped).
    pub container_exists: bool,
    /// …and is currently running.
    pub container_running: bool,
    /// `sudo -n -u postgres psql` connects on this host.
    pub local_psql_ok: bool,
}

/// Choose the local transport, or `None` when neither mechanism is available.
///
/// Ordering is deliberate and the container-**exists** test is load-bearing:
///
/// * If the container exists at all — even stopped — prefer Docker. On a developer
///   laptop with a stopped stack, "start the stack" is the correct diagnosis, and
///   falling through to some *other* local Postgres would be far worse than an
///   error: it would silently administer the wrong database.
/// * Only when there is no container to speak of (or Docker is not installed) do we
///   consider bare metal.
pub fn select_local(caps: LocalCapabilities, container: &str, db: &str, user: &str) -> Option<PsqlTransport> {
    if caps.container_exists {
        // Running or stopped, Docker owns this host's Postgres. A stopped container
        // surfaces the existing "is the stack running?" error at exec time.
        let _ = caps.container_running;
        return Some(PsqlTransport::DockerExec {
            container: container.to_string(),
            user: user.to_string(),
            db: db.to_string(),
        });
    }
    if caps.local_psql_ok {
        return Some(PsqlTransport::LocalPsql { db: db.to_string() });
    }
    None
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
                 target this machine's database (a docker stack or a bare-metal \
                 PostgreSQL, whichever is present).",
                cfg.validator.url
            );
        }
        let caps = Self::probe_local_caps(&cfg.docker.postgres_container, &cfg.database.name);
        select_local(
            caps,
            &cfg.docker.postgres_container,
            &cfg.database.name,
            &cfg.database.user,
        )
        .ok_or_else(|| {
            // Name BOTH attempted paths. The old message assumed Docker unconditionally
            // and told bare-metal operators to start a stack that cannot exist (CLI-7).
            anyhow::anyhow!(
                "no local Postgres available for database administration. Tried:\n  \
                 • docker container `{}` — not found\n  \
                 • `sudo -n -u postgres psql -d {}` — unavailable\n\
                 For a Docker stack run `knishio start`. On a bare-metal host, the \
                 sudoers grant comes from `knishio deploy bootstrap`. For a remote \
                 server pass --host <user@host>.",
                cfg.docker.postgres_container,
                cfg.database.name
            )
        })
    }

    /// Observe which local psql mechanisms work on this machine.
    ///
    /// Cheap in the common case: developer machines have the container, so the
    /// `sudo` probe is skipped entirely.
    fn probe_local_caps(container: &str, db: &str) -> LocalCapabilities {
        let inspect = |args: &[&str]| -> Option<String> {
            let out = std::process::Command::new("docker").args(args).output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        // `ps -a` includes stopped containers — the distinction that keeps a laptop
        // with a stopped stack from silently falling through to another database.
        let all = inspect(&["ps", "-a", "--filter", &format!("name=^{container}$"), "--format", "{{.Names}}"]);
        let container_exists = all.as_deref().is_some_and(|s| !s.is_empty());
        let container_running = if container_exists {
            inspect(&["ps", "--filter", &format!("name=^{container}$"), "--format", "{{.Names}}"])
                .as_deref()
                .is_some_and(|s| !s.is_empty())
        } else {
            false
        };

        let local_psql_ok = if container_exists {
            false // not consulted; skip the probe
        } else {
            std::process::Command::new("sudo")
                .args(["-n", "-u", "postgres", "psql", "-d", db, "-tAc", "select 1"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        LocalCapabilities { container_exists, container_running, local_psql_ok }
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
            PsqlTransport::LocalPsql { .. } => {
                "local://postgres (sudo -u postgres psql — bare metal)".to_string()
            }
        }
    }

    /// True when effects land on this machine.
    ///
    /// `LocalPsql` counts: the database is on this host. ⚠️ On a deployed node that
    /// means "local" is *production*, so a mutating `cell`/`audit` command there runs
    /// without the confirmation prompt (settled deliberately 2026-07-28 — read-only
    /// commands never prompted anyway). `--yes` remains available for scripts.
    pub fn is_local(&self) -> bool {
        matches!(self, PsqlTransport::DockerExec { .. } | PsqlTransport::LocalPsql { .. })
    }

    /// Run SQL (via stdin) and return trimmed stdout. Errors are scrubbed to
    /// operator-friendly messages; `missing_hint` customizes the
    /// "relation does not exist" case (audit uses its own).
    pub async fn exec_with_hint(&self, sql: &str, missing_hint: &str) -> Result<String> {
        let (program, args) = self.psql_stdin_argv(self.db_name());
        let mut cmd = Command::new(&program);
        cmd.args(&args);

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
                PsqlTransport::LocalPsql { .. } => {
                    "Failed to spawn sudo/psql — is a local PostgreSQL client installed?"
                }
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
            } else if stderr.contains("is not running") {
                // `docker exec` against a stopped container. Now that stopped-vs-absent
                // decides which transport is chosen (CLI-7), this case deserves an
                // actionable message rather than the generic fallthrough it used to hit.
                anyhow::bail!(
                    "The postgres container exists but is stopped — run `knishio start` \
                     (or `docker start` it)."
                );
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

    /// The database this transport administers.
    pub fn db_name(&self) -> &str {
        match self {
            PsqlTransport::DockerExec { db, .. }
            | PsqlTransport::Ssh { db, .. }
            | PsqlTransport::LocalPsql { db } => db,
        }
    }

    /// The stdin-fed `psql` command line for an arbitrary database, as (program, args).
    ///
    /// Single source for the exec path and for restore's dump pipe, so the two cannot
    /// drift the way the banner drifted from the transport in CLI-7b.
    pub fn psql_stdin_argv(&self, db: &str) -> (String, Vec<String>) {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            PsqlTransport::DockerExec { container, user, .. } => (
                "docker".into(),
                owned(&[
                    "exec", "-i", container, "psql", "-U", user, "-d", db, "-q", "-t", "-A", "-f",
                    "-",
                ]),
            ),
            PsqlTransport::LocalPsql { .. } => (
                "sudo".into(),
                owned(&[
                    "-n", "-u", "postgres", "psql", "-d", db, "-q", "-t", "-A", "-f", "-",
                ]),
            ),
            PsqlTransport::Ssh { host, .. } => (
                std::env::var("KNISHIO_SSH_BIN").unwrap_or_else(|_| "ssh".to_string()),
                owned(&[
                    "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, "sudo", "-n", "-u",
                    "postgres", "psql", "-d", db, "-q", "-t", "-A", "-f", "-",
                ]),
            ),
        }
    }

    /// The `pg_dump` command line for this transport, as (program, args).
    ///
    /// Pure so the three shapes are unit-assertable without spawning anything. Note the
    /// bare-metal and ssh arms take **no `-U`**: they already run *as* the `postgres`
    /// role via sudo, exactly as their `psql` counterparts do. Only the Docker arm needs
    /// `-U`, because there the client runs inside the container as root.
    pub fn pg_dump_argv(&self) -> (String, Vec<String>) {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            PsqlTransport::DockerExec { container, user, db } => (
                "docker".into(),
                owned(&[
                    "exec", container, "pg_dump", "-U", user, "-d", db, "--no-owner", "--no-acl",
                ]),
            ),
            PsqlTransport::LocalPsql { db } => (
                "sudo".into(),
                owned(&[
                    "-n", "-u", "postgres", "pg_dump", "-d", db, "--no-owner", "--no-acl",
                ]),
            ),
            PsqlTransport::Ssh { host, db } => (
                std::env::var("KNISHIO_SSH_BIN").unwrap_or_else(|_| "ssh".to_string()),
                owned(&[
                    "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, "sudo", "-n", "-u",
                    "postgres", "pg_dump", "-d", db, "--no-owner", "--no-acl",
                ]),
            ),
        }
    }

    /// Stream a `pg_dump` of the database to bytes.
    pub async fn pg_dump(&self) -> Result<Vec<u8>> {
        let (program, args) = self.pg_dump_argv();
        let out = Command::new(&program)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("Failed to run {program} for pg_dump"))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("a password is required") || stderr.contains("sudo:") {
                anyhow::bail!(
                    "pg_dump refused: the sudoers grant for `sudo -u postgres pg_dump` is \
                     missing on this host. It is installed by `knishio deploy bootstrap` \
                     (0.2.7+); hosts provisioned earlier only granted psql."
                );
            }
            anyhow::bail!("pg_dump failed: {}", stderr.trim());
        }
        Ok(out.stdout)
    }

    /// Run SQL against an ARBITRARY database on this transport.
    ///
    /// Restore needs the `postgres` maintenance database to drop/recreate the target, which
    /// [`exec`] cannot express since it is pinned to the configured database.
    pub async fn exec_on_db(&self, db: &str, sql: &str) -> Result<String> {
        self.with_db(db).exec(sql).await
    }

    /// This transport retargeted at another database. Pure, so the retargeting itself is
    /// unit-testable — restore depends on it to drop the database it is not connected to.
    pub fn with_db(&self, db: &str) -> Self {
        let mut t = self.clone();
        match &mut t {
            PsqlTransport::DockerExec { db: d, .. }
            | PsqlTransport::Ssh { db: d, .. }
            | PsqlTransport::LocalPsql { db: d } => *d = db.to_string(),
        }
        t
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

    fn caps(exists: bool, running: bool, psql_ok: bool) -> LocalCapabilities {
        LocalCapabilities { container_exists: exists, container_running: running, local_psql_ok: psql_ok }
    }

    /// The full local-selection matrix (CLI-7). Pure function, so no daemon or DB needed.
    #[test]
    fn select_local_matrix() {
        // Container running → Docker, as always.
        let t = select_local(caps(true, true, false), "pg", "knishio", "knishio").unwrap();
        assert!(matches!(t, PsqlTransport::DockerExec { .. }));
        assert!(t.describe().contains("docker://pg"));

        // THE REGRESSION GUARD: container exists but is STOPPED, and a local Postgres
        // happens to be reachable. Must still pick Docker — falling through would
        // silently administer a DIFFERENT database on a developer laptop, and the
        // "is the stack running?" error is the correct diagnosis there.
        let t = select_local(caps(true, false, true), "pg", "knishio", "knishio").unwrap();
        assert!(
            matches!(t, PsqlTransport::DockerExec { .. }),
            "a stopped container must NOT fall through to bare metal"
        );

        // No container at all + local psql works → bare metal (the testnet box).
        let t = select_local(caps(false, false, true), "pg", "knishio", "knishio").unwrap();
        assert!(matches!(t, PsqlTransport::LocalPsql { .. }));
        assert!(t.describe().contains("bare metal"), "banner must not claim docker: {}", t.describe());
        assert!(!t.describe().contains("docker"));

        // Neither → None, so the caller can name both attempted paths.
        assert!(select_local(caps(false, false, false), "pg", "knishio", "knishio").is_none());
    }

    /// The exact pg_dump command line per transport (CLI-8). Pure, so this asserts the
    /// real argv without a daemon, a database, or a network.
    #[test]
    fn pg_dump_argv_per_transport() {
        let docker = PsqlTransport::DockerExec {
            container: "pg".into(),
            user: "knishio".into(),
            db: "knishio".into(),
        };
        let (p, a) = docker.pg_dump_argv();
        assert_eq!(p, "docker");
        assert_eq!(
            a,
            vec!["exec", "pg", "pg_dump", "-U", "knishio", "-d", "knishio", "--no-owner", "--no-acl"]
        );

        // Bare metal runs AS postgres via sudo, so it must NOT pass -U (that would try to
        // connect as a role that may not exist on a server).
        let bare = PsqlTransport::LocalPsql { db: "knishio".into() };
        let (p, a) = bare.pg_dump_argv();
        assert_eq!(p, "sudo");
        assert_eq!(
            a,
            vec!["-n", "-u", "postgres", "pg_dump", "-d", "knishio", "--no-owner", "--no-acl"]
        );
        assert!(!a.contains(&"-U".to_string()), "bare metal must not pass -U");

        let ssh = PsqlTransport::Ssh { host: "forge@h".into(), db: "knishio".into() };
        let (_, a) = ssh.pg_dump_argv();
        assert!(a.contains(&"forge@h".to_string()));
        assert!(a.contains(&"pg_dump".to_string()));
        assert!(a.contains(&"BatchMode=yes".to_string()), "ssh must never prompt");
        assert!(!a.contains(&"-U".to_string()));
    }

    /// `exec_on_db` must retarget the database (restore drops the configured one) while
    /// leaving the transport's other fields alone.
    #[test]
    fn psql_stdin_argv_retargets_the_database() {
        let docker = PsqlTransport::DockerExec {
            container: "pg".into(),
            user: "knishio".into(),
            db: "knishio".into(),
        };
        let (_, a) = docker.psql_stdin_argv("postgres");
        assert!(a.windows(2).any(|w| w == ["-d", "postgres"]), "argv: {a:?}");
        // stdin-fed, so no nested shell quoting anywhere.
        assert_eq!(a.last().map(String::as_str), Some("-"));

        let bare = PsqlTransport::LocalPsql { db: "knishio".into() };
        let (p, a) = bare.psql_stdin_argv("postgres");
        assert_eq!(p, "sudo");
        assert!(a.windows(2).any(|w| w == ["-d", "postgres"]), "argv: {a:?}");
        assert_eq!(bare.db_name(), "knishio", "db_name must reflect the transport, not the override");

        // `exec_on_db` retargets via `with_db`. If this silently returned the original
        // transport, restore would issue DROP DATABASE while connected to the very database
        // it is dropping — the whole reason the maintenance DB is used.
        for t in [
            PsqlTransport::DockerExec { container: "pg".into(), user: "u".into(), db: "knishio".into() },
            PsqlTransport::LocalPsql { db: "knishio".into() },
            PsqlTransport::Ssh { host: "h".into(), db: "knishio".into() },
        ] {
            assert_eq!(t.with_db("postgres").db_name(), "postgres", "with_db must retarget: {t:?}");
            assert_eq!(t.db_name(), "knishio", "with_db must not mutate the original");
        }
    }

    /// Bare-metal counts as local, so mutations are not gated behind a prompt
    /// (deliberate 2026-07-28) — and the banner still says which mechanism is in play.
    #[test]
    fn local_psql_is_local_and_honestly_described() {
        let t = PsqlTransport::LocalPsql { db: "knishio".into() };
        assert!(t.is_local());
        assert!(!PsqlTransport::Ssh { host: "h".into(), db: "d".into() }.is_local());
    }

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
