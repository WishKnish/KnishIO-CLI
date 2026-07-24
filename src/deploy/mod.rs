//! `knishio deploy` — bare-metal deployment orchestration.
//!
//! Encodes the validator's OPS-010 runbook (deploy/BARE-METAL-DEPLOYMENT.md)
//! as generated, reviewable artifacts — every hard gate here was learned live
//! on testnet.knish.io (docs/audits/TESTNET-DEPLOY-2026-07-23.md). Commands
//! GENERATE by default; `--execute` (ship/upgrade) opts into running the
//! non-root steps over ssh. Root steps always ship as a script the operator
//! reviews and runs (hosting-panel sudo is typically password-gated).

pub mod ship;
pub mod ssh;
pub mod upgrade;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::output;

// ── Shared command grants ───────────────────────────────────────────
//
// SINGLE SOURCE for the sudoers file AND every ssh invocation. sudoers
// matches commands by EXACT string (extra args = denied), so any drift
// between what we grant and what we run breaks --execute.

pub const SUDO_UPGRADE: &str = "/usr/local/bin/upgrade.sh";
pub const SUDO_SYSTEMCTL_RESTART: &str = "/usr/bin/systemctl restart knishio-validator";
pub const SUDO_SYSTEMCTL_STATUS: &str = "/usr/bin/systemctl status knishio-validator";
pub const SUDO_PSQL: &str = "/usr/bin/psql";

/// Render the sudoers drop-in for `deploy_user` from the shared consts.
pub fn sudoers_content(deploy_user: &str) -> String {
    format!(
        "{u} ALL=(root) NOPASSWD: {upgrade}\n\
         {u} ALL=(root) NOPASSWD: {restart}, {status}\n\
         {u} ALL=(postgres) NOPASSWD: {psql}\n",
        u = deploy_user,
        upgrade = SUDO_UPGRADE,
        restart = SUDO_SYSTEMCTL_RESTART,
        status = SUDO_SYSTEMCTL_STATUS,
        psql = SUDO_PSQL,
    )
}

// ── Template rendering ──────────────────────────────────────────────

/// `{{KEY}}` replacement over an `include_str!` template. Errors when any
/// `{{` survives — catches typos and missing variables at generation time
/// instead of on the server.
pub fn render(template: &str, vars: &[(&str, &str)]) -> Result<String> {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    if let Some(pos) = out.find("{{") {
        let tail: String = out[pos..].chars().take(40).collect();
        anyhow::bail!("unrendered template variable near: {}", tail);
    }
    Ok(out)
}

fn write_artifact(dir: &Path, name: &str, content: &str, executable: bool) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create output dir {}", dir.display()))?;
    let path = dir.join(name);
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    output::success(&format!("wrote {}", path.display()));
    Ok(path)
}

/// Split a `user@host` --host value into the user part (deploy user).
pub fn user_of(host: &str) -> &str {
    host.split('@').next().unwrap_or(host)
}

// ── env: /etc/knishio/.env content ──────────────────────────────────

pub struct EnvOpts<'a> {
    pub behind_proxy: bool,
    pub cors: &'a str,
    pub port: u16,
}

pub fn env_content(opts: &EnvOpts) -> Result<String> {
    render(
        include_str!("templates/env.production.tmpl"),
        &[
            ("SERVER_HOST", if opts.behind_proxy { "127.0.0.1" } else { "0.0.0.0" }),
            ("SERVER_PORT", &opts.port.to_string()),
            ("CORS_ORIGINS", opts.cors),
            // TRUSTED_PROXY_IPS is MANDATORY behind a proxy: without it every
            // per-IP rate limit collapses into one global bucket (auth default
            // = 3 req/min for the entire internet). F-5.
            (
                "TRUSTED_PROXY_LINE",
                if opts.behind_proxy {
                    "TRUSTED_PROXY_IPS=127.0.0.1"
                } else {
                    "# TRUSTED_PROXY_IPS= (not behind a proxy)"
                },
            ),
        ],
    )
}

pub async fn env(out_dir: &Path, behind_proxy: bool, cors: &str, port: u16) -> Result<()> {
    let content = env_content(&EnvOpts { behind_proxy, cors, port })?;
    write_artifact(out_dir, "knishio.env", &content, false)?;
    output::info(
        "Review, then install on the server as /etc/knishio/.env (mode 600, owner knishio). \
         The __KNISHIO_DB_PASS__ slot is filled by the bootstrap script, which generates \
         the DB password on-server.",
    );
    Ok(())
}

// ── bootstrap: runbook §1–§7 as one idempotent root script ──────────

pub async fn bootstrap(
    out_dir: &Path,
    deploy_user: &str,
    behind_proxy: bool,
    cors: &str,
    port: u16,
) -> Result<()> {
    let env_body = env_content(&EnvOpts { behind_proxy, cors, port })?;
    // The env body nests inside the bootstrap heredoc; its __KNISHIO_DB_PASS__
    // sentinel is substituted by the script at run time (sed) with the
    // password generated on-server.
    let script = render(
        include_str!("templates/bootstrap.sh.tmpl"),
        &[
            ("PG_MAJOR", "16"),
            ("DEPLOY_USER", deploy_user),
            ("SERVER_PORT", &port.to_string()),
            ("ENV_CONTENT", env_body.trim_end()),
            ("SUDOERS_CONTENT", sudoers_content(deploy_user).trim_end()),
        ],
    )?;
    write_artifact(out_dir, "knishio-bootstrap.sh", &script, true)?;
    output::info(&format!(
        "Review it, ship it with the tarball, then run ONCE as root on the server:\n  \
         sudo bash knishio-bootstrap.sh /home/{}/knishio-validator-<ver>-linux-<arch>.tar.gz",
        deploy_user
    ));
    Ok(())
}

// ── edge: nginx reverse-proxy vhost ─────────────────────────────────

pub async fn edge(
    out_dir: &Path,
    domain: &str,
    flavor: &str,
    upstream_port: u16,
    forge_server_id: &str,
) -> Result<()> {
    let (tmpl, name): (&str, String) = match flavor {
        "forge" => (
            include_str!("templates/nginx-forge.conf.tmpl"),
            format!("nginx-{}.forge.conf", domain),
        ),
        _ => (
            include_str!("templates/nginx-generic.conf.tmpl"),
            format!("nginx-{}.conf", domain),
        ),
    };
    let content = render(
        tmpl,
        &[
            ("DOMAIN", domain),
            ("UPSTREAM_PORT", &upstream_port.to_string()),
            ("FORGE_SERVER_ID", forge_server_id),
            ("SSL_CERT_PATH", "/etc/nginx/ssl/REPLACE/server.crt"),
            ("SSL_KEY_PATH", "/etc/nginx/ssl/REPLACE/server.key"),
        ],
    )?;
    write_artifact(out_dir, &name, &content, false)?;
    if flavor == "forge" {
        output::info(
            "Paste into Forge → site → Edit Nginx Configuration. FIRST copy the \
             existing file's `# FORGE SSL` cert paths over the REPLACE placeholders \
             and check the forge-conf server id matches. Do NOT edit Forge's \
             'General site config' — this file deliberately stops including it.",
        );
    } else {
        output::info(
            "Install under /etc/nginx/sites-available/, fill the ssl_certificate \
             paths (e.g. certbot), symlink into sites-enabled, `nginx -t`, reload.",
        );
    }
    Ok(())
}

// ── forge: the CD script pair ───────────────────────────────────────

pub async fn forge(out_dir: &Path, deploy_user: &str, lock_committed: bool) -> Result<()> {
    let deploy_script = render(
        include_str!("templates/forge-deploy.txt.tmpl"),
        &[("DEPLOY_USER", deploy_user)],
    )?;
    // Forge template scripts break on multi-line/UTF-8/comment content —
    // validated live. Enforce the invariants at generation time too.
    debug_assert!(deploy_script.is_ascii());
    write_artifact(out_dir, "forge-deploy-script.txt", &deploy_script, false)?;

    let lock_line = if lock_committed {
        "# Cargo.lock is committed — cargo --locked uses it directly."
    } else {
        // F-12: with the lock uncommitted, a fresh clone re-resolves deps and
        // can break (live example: newest pgvector pulled sqlx 0.9 beside 0.8).
        "cp \"$HOME/knishio-Cargo.lock.pinned\" Cargo.lock"
    };
    let build_script = render(
        include_str!("templates/knishio-deploy-build.sh.tmpl"),
        &[("LOCK_RESTORE_LINE", lock_line)],
    )?;
    write_artifact(out_dir, "knishio-deploy-build.sh", &build_script, true)?;

    let lock_note = if lock_committed {
        "Cargo.lock is committed — no pinned-lock step needed.".to_string()
    } else {
        format!(
            "Stash a known-good lock at /home/{}/knishio-Cargo.lock.pinned \
             (F-12 interim until Cargo.lock is committed).",
            deploy_user
        )
    };
    output::info(&format!(
        "1. Copy knishio-deploy-build.sh to /home/{}/ on the server.\n  \
         2. Paste forge-deploy-script.txt into Forge → site → Deployments.\n  \
         3. {}",
        deploy_user, lock_note
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_and_rejects_leftovers() {
        assert_eq!(render("a {{X}} b", &[("X", "1")]).unwrap(), "a 1 b");
        assert!(render("a {{X}} {{Y}}", &[("X", "1")]).is_err());
    }

    #[test]
    fn sudoers_lines_match_shared_consts() {
        let s = sudoers_content("forge");
        assert!(s.contains(SUDO_UPGRADE));
        assert!(s.contains(SUDO_SYSTEMCTL_RESTART));
        assert!(s.contains(SUDO_SYSTEMCTL_STATUS));
        assert!(s.contains(SUDO_PSQL));
        assert!(s.lines().all(|l| l.starts_with("forge ")));
    }

    #[test]
    fn env_behind_proxy_invariants() {
        let e = env_content(&EnvOpts { behind_proxy: true, cors: "*", port: 8080 }).unwrap();
        assert!(e.contains("SERVER_HOST=127.0.0.1"));
        assert!(e.contains("TRUSTED_PROXY_IPS=127.0.0.1"), "F-5: mandatory behind proxy");
        assert!(e.contains("VALIDATOR_KEM_SECRET_FILE="), "F-10");
        assert!(e.contains("KNISHIO_ENV=production"));
        assert!(!e.contains("base64"), "hex-only DB password (DSN parse bug)");
        let open = env_content(&EnvOpts { behind_proxy: false, cors: "*", port: 8080 }).unwrap();
        assert!(open.contains("SERVER_HOST=0.0.0.0"));
    }

    #[test]
    fn bootstrap_hard_gates_present() {
        let env_body = env_content(&EnvOpts { behind_proxy: true, cors: "*", port: 8080 }).unwrap();
        let s = render(
            include_str!("templates/bootstrap.sh.tmpl"),
            &[
                ("PG_MAJOR", "16"),
                ("DEPLOY_USER", "forge"),
                ("SERVER_PORT", "8080"),
                ("ENV_CONTENT", env_body.trim_end()),
                ("SUDOERS_CONTENT", sudoers_content("forge").trim_end()),
            ],
        )
        .unwrap();
        // F-13: uuid-ossp BEFORE vector, both before enable --now
        let uuid = s.find(r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp""#).expect("uuid-ossp");
        let vector = s.find("CREATE EXTENSION IF NOT EXISTS vector").expect("vector");
        let enable = s.find("systemctl enable --now knishio-validator").expect("enable");
        assert!(uuid < enable && vector < enable);
        // F-1: DIRECTIVE-ONLY docker check
        assert!(s.contains(r"^(Requires|After)=.*docker\.service"));
        // Forge sudoers lesson: scoped visudo, never global
        assert!(s.contains("visudo -cf"));
        assert!(!s.contains("visudo -c\n"));
        // hex-only password
        assert!(s.contains("openssl rand -hex"));
        assert!(!s.contains("rand -base64"));
        // F-3: never hardcode the migration count
        assert!(s.contains("m.get('applied') == m.get('expected')"));
    }

    #[test]
    fn forge_script_is_minimal_ascii() {
        let s = render(
            include_str!("templates/forge-deploy.txt.tmpl"),
            &[("DEPLOY_USER", "forge")],
        )
        .unwrap();
        assert!(s.is_ascii(), "Forge template expansion breaks on non-ASCII");
        assert!(!s.contains('#'), "comments break Forge template expansion");
        assert!(s.lines().filter(|l| !l.trim().is_empty()).count() <= 6);
        assert!(s.contains("|| exit 1"), "activation must be gated on the build");
    }
}
