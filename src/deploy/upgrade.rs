//! `knishio deploy upgrade` — drive the server's upgrade.sh over ssh.
//!
//! The server script owns the safety sequence (pg_dump backup → binary swap
//! with .bak → restart → /readyz gate → AUTO-ROLLBACK on timeout); this
//! command is a thin, preflighted trigger. All three drill paths (upgrade,
//! --rollback, auto-rollback on a broken binary) were validated live on
//! testnet.knish.io.

use anyhow::Result;

use super::ssh::SshHost;
use super::{SUDO_UPGRADE, sudoers_content, user_of};
use crate::output;

pub async fn upgrade(
    host: &str,
    binary: Option<&str>,
    rollback: bool,
    execute: bool,
    readyz_url: Option<&str>,
) -> Result<()> {
    let remote_cmd = if rollback {
        format!("sudo -n {} --rollback", SUDO_UPGRADE)
    } else {
        let bin = binary.unwrap_or("$HOME/.cargo-target/knishio-validator/release/knishio-validator");
        format!("sudo -n {} {}", SUDO_UPGRADE, bin)
    };

    if !execute {
        println!("# knishio deploy upgrade — command (re-run with --execute to run it):");
        println!("ssh {} '{}'", host, remote_cmd);
        return Ok(());
    }

    let h = SshHost(host.to_string());

    // Preflight: the scoped sudoers grant must exist, or the remote sudo -n
    // will fail obscurely. Fall back to artifact mode with the exact fix.
    if !h.has_sudo_grant(SUDO_UPGRADE).await {
        output::error(&format!(
            "{} has no NOPASSWD grant for {} — `deploy bootstrap` installs it. \
             Expected /etc/sudoers.d/knishio-deploy content:",
            host, SUDO_UPGRADE
        ));
        eprintln!("{}", sudoers_content(user_of(host)));
        anyhow::bail!("sudo preflight failed on {}", host);
    }

    output::info(&format!("Running on {}: {}", host, remote_cmd));
    let ok = h.run_streaming(&remote_cmd).await?;
    if !ok {
        anyhow::bail!(
            "remote upgrade.sh exited non-zero — it auto-rolls-back on a failed \
             readiness gate; inspect `journalctl -u knishio-validator` on the server"
        );
    }

    // Belt-and-suspenders: independently confirm readiness from here when a
    // public URL is known (upgrade.sh already gated on the loopback /readyz).
    if let Some(url) = readyz_url {
        let ready = crate::http::wait_for_ready(
            url,
            false,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(3),
        )
        .await;
        if ready {
            output::success(&format!("{}/readyz confirms ready", url.trim_end_matches('/')));
        } else {
            output::warn(&format!(
                "upgrade.sh reported success but {}/readyz is not confirming from here \
                 (edge/network issue?)",
                url.trim_end_matches('/')
            ));
        }
    }
    Ok(())
}
