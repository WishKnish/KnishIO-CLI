//! `knishio deploy ship` — move a release tarball to the server with
//! SHAKE256 chain-of-custody verified on BOTH ends.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::ssh::SshHost;
use crate::output;

/// Compute the tarball's inner binary hash locally is not possible without
/// unpacking; the custody chain instead verifies the SHIPPED FILE hash
/// (local sha of the tarball vs remote sha), and the bootstrap re-verifies
/// the inner binary against SHAKE256SUMS at install time.
fn local_shake256(path: &Path) -> Result<String> {
    use sha3::digest::{ExtendableOutput, Update, XofReader};
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = sha3::Shake256::default();
    hasher.update(&bytes);
    let mut out = [0u8; 32];
    hasher.finalize_xof().read(&mut out);
    Ok(hex::encode(out))
}

/// The remote SHAKE256 command, used by BOTH artifact mode (printed) and
/// `--execute` (run) so the two can never drift.
///
/// `os.path.expanduser` is load-bearing: the default `--dest` is
/// `~/knishio-staging`, and a bare `~` inside a Python string literal is NOT
/// expanded (tilde expansion is a shell feature) — `open("~/…")` raises
/// FileNotFoundError, which used to surface as a bogus "chain-of-custody
/// FAILED" on an upload that actually succeeded.
fn remote_hash_cmd(remote_path: &str) -> String {
    format!(
        "python3 -c 'import hashlib,os;print(hashlib.shake_256(open(os.path.expanduser(\"{}\"),\"rb\").read()).hexdigest(32))'",
        remote_path
    )
}

/// Locate the newest tarball in the validator repo's dist/ for `arch`.
fn find_tarball(validator_dir: &Path, arch: &str) -> Result<PathBuf> {
    let dist = validator_dir.join("dist");
    let needle = format!("-linux-{}.tar.gz", arch);
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&dist)
        .with_context(|| format!("no dist/ at {} — run `knishio package` first", dist.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(&needle))
        .collect();
    candidates.sort();
    candidates
        .pop()
        .with_context(|| format!("no *-linux-{}.tar.gz in {}", arch, dist.display()))
}

pub async fn ship(
    validator_dir: &Path,
    host: &str,
    arch: &str,
    dest_dir: &str,
    tarball: Option<PathBuf>,
    execute: bool,
) -> Result<()> {
    let tarball = match tarball {
        Some(t) => t,
        None => find_tarball(validator_dir, arch)?,
    };
    let fname = tarball
        .file_name()
        .and_then(|n| n.to_str())
        .context("tarball has no file name")?
        .to_string();
    let local_hash = local_shake256(&tarball)?;
    let remote_path = format!("{}/{}", dest_dir.trim_end_matches('/'), fname);

    output::info(&format!(
        "Tarball: {} (SHAKE256 {})",
        tarball.display(),
        &local_hash[..16]
    ));

    if !execute {
        println!("# knishio deploy ship — commands (re-run with --execute to run them):");
        println!("ssh {} 'mkdir -p {}'", host, dest_dir);
        println!("scp {} {}:{}", tarball.display(), host, remote_path);
        println!("ssh {} \"{}\"", host, remote_hash_cmd(&remote_path).replace('"', "\\\""));
        println!("# expect: {}", local_hash);
        return Ok(());
    }

    let h = SshHost(host.to_string());
    let mkdir = h.run(&format!("mkdir -p {}", dest_dir)).await?;
    if !mkdir.status.success() {
        anyhow::bail!(
            "mkdir -p {} failed on {}: {}",
            dest_dir,
            host,
            String::from_utf8_lossy(&mkdir.stderr)
        );
    }
    h.scp(&tarball, &remote_path).await?;

    let remote = h.run(&remote_hash_cmd(&remote_path)).await?;
    let remote_hash = String::from_utf8_lossy(&remote.stdout).trim().to_string();
    // An empty hash means the remote command itself failed (missing python3,
    // unreadable path) — surface THAT rather than a bogus mismatch report.
    if remote_hash.is_empty() {
        anyhow::bail!(
            "could not compute the remote SHAKE256 on {} — the upload may have \
             succeeded; verification did not run:\n{}",
            host,
            String::from_utf8_lossy(&remote.stderr).trim()
        );
    }
    if remote_hash != local_hash {
        anyhow::bail!(
            "chain-of-custody FAILED: local {} != remote {} — re-ship",
            local_hash,
            remote_hash
        );
    }
    output::success(&format!(
        "shipped {} → {}:{} (SHAKE256 verified both ends)",
        fname, host, remote_path
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards CLI-4: the default `--dest` is `~/knishio-staging`, and Python's
    /// `open()` does not expand `~`. Without expanduser, `deploy ship --execute`
    /// fails verification on its own default path.
    #[test]
    fn remote_hash_cmd_expands_tilde() {
        let cmd = remote_hash_cmd("~/knishio-staging/x.tar.gz");
        assert!(cmd.contains("os.path.expanduser"), "cmd: {cmd}");
        assert!(cmd.contains("import hashlib,os"), "os must be imported: {cmd}");
        assert!(cmd.contains("hexdigest(32)"), "SHAKE256 32-byte XOF: {cmd}");
        // The path must not be passed bare to open() — that's the bug.
        assert!(!cmd.contains("open(\"~"), "tilde passed bare to open(): {cmd}");
    }
}
