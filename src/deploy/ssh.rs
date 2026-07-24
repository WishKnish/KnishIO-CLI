//! ssh/scp execution layer: shell out to the system binaries (agent, keys,
//! known_hosts, ProxyJump and ~/.ssh/config come for free — a russh
//! dependency would reimplement all of that for zero capability gain here).
//!
//! Test seam: `KNISHIO_SSH_BIN` / `KNISHIO_SCP_BIN` point tests at a stub
//! that records argv+stdin and returns canned output.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Output, Stdio};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct SshHost(pub String);

fn ssh_bin() -> String {
    std::env::var("KNISHIO_SSH_BIN").unwrap_or_else(|_| "ssh".into())
}

fn scp_bin() -> String {
    std::env::var("KNISHIO_SCP_BIN").unwrap_or_else(|_| "scp".into())
}

const SSH_OPTS: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

impl SshHost {
    /// Run a remote command, capturing output.
    pub async fn run(&self, cmd: &str) -> Result<Output> {
        Command::new(ssh_bin())
            .args(SSH_OPTS)
            .arg(&self.0)
            .arg(cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to spawn ssh — is it installed?")
    }

    /// Run a remote command, streaming output to the operator's terminal.
    pub async fn run_streaming(&self, cmd: &str) -> Result<bool> {
        let status = Command::new(ssh_bin())
            .args(SSH_OPTS)
            .arg(&self.0)
            .arg(cmd)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("failed to spawn ssh — is it installed?")?;
        Ok(status.success())
    }

    /// Copy a local file to `remote_path` on the host.
    pub async fn scp(&self, local: &Path, remote_path: &str) -> Result<()> {
        let status = Command::new(scp_bin())
            .args(SSH_OPTS)
            .arg(local)
            .arg(format!("{}:{}", self.0, remote_path))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("failed to spawn scp — is it installed?")?;
        if !status.success() {
            anyhow::bail!("scp to {}:{} failed", self.0, remote_path);
        }
        Ok(())
    }

    /// `sudo -n -l <cmd>` preflight: is the scoped NOPASSWD grant in place?
    pub async fn has_sudo_grant(&self, cmd: &str) -> bool {
        match self.run(&format!("sudo -n -l {}", cmd)).await {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }
}
