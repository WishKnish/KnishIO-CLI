//! `knishio package` — wrap the validator's local-packaging Makefile.
//!
//! Thin shell-out to `make` inside `servers/knishio-validator-rust/`.
//! Makefile is the authoritative source; CLI just saves the operator
//! from `cd`-ing into the validator dir. Symmetric with how `knishio
//! rebuild` abstracts `docker compose build`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

use crate::{output, paths};

/// Map an arch name to the Makefile's linux packaging target.
/// `package-linux` is arm64 (native buildx on Apple Silicon);
/// `package-linux-amd64` runs the builder stage under emulation
/// (BUILDER_PLATFORM override) with a file(1) arch gate.
pub fn linux_make_target(arch: &str) -> Result<&'static str> {
    match arch {
        "arm64" | "aarch64" => Ok("package-linux"),
        "amd64" | "x86_64" => Ok("package-linux-amd64"),
        other => anyhow::bail!(
            "unknown arch `{}` — expected arm64 or amd64",
            other
        ),
    }
}

/// Locate the directory that contains the validator's Makefile by
/// anchoring on `docker-compose.standalone.yml` — the same marker the
/// `docker` subcommands use for compose-file discovery. Returns the
/// parent directory of that file.
pub fn find_validator_dir(cwd: &Path) -> Result<PathBuf> {
    let compose = paths::find_compose_file(cwd, "docker-compose.standalone.yml")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find validator Makefile. Run `knishio package` \
                 from the monorepo root or from servers/knishio-validator-rust/."
            )
        })?;
    Ok(compose
        .parent()
        .expect("compose file always has a parent")
        .to_path_buf())
}

/// Run `make -C <dir> <target>` with stdout/stderr inherited so the
/// operator sees cargo + docker buildx progress live. Non-zero exit is
/// surfaced as an error; spawn failures (e.g. `make` not installed) get
/// a clear hint via the error context.
async fn run_make(dir: &Path, target: &str) -> Result<()> {
    let status = Command::new("make")
        .arg("-C")
        .arg(dir)
        .arg(target)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| {
            format!(
                "failed to invoke `make -C {} {}` — is `make` installed? \
                 On macOS: `xcode-select --install`. On Debian/Ubuntu: \
                 `apt install build-essential`.",
                dir.display(),
                target
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "`make -C {} {}` exited non-zero ({})",
            dir.display(),
            target,
            status
        );
    }
    Ok(())
}

pub async fn package_mac(cwd: &Path) -> Result<()> {
    let dir = find_validator_dir(cwd)?;
    output::info(&format!(
        "Packaging macOS arm64 via {}/Makefile",
        dir.display()
    ));
    run_make(&dir, "package-mac").await
}

pub async fn package_linux(cwd: &Path, arch: &str) -> Result<()> {
    let dir = find_validator_dir(cwd)?;
    let target = linux_make_target(arch)?;
    output::info(&format!(
        "Packaging Linux {} via {}/Makefile ({})",
        arch,
        dir.display(),
        target
    ));
    run_make(&dir, target).await
}

pub async fn package_all(cwd: &Path) -> Result<()> {
    let dir = find_validator_dir(cwd)?;
    output::info(&format!(
        "Packaging macOS arm64 + Linux arm64 via {}/Makefile",
        dir.display()
    ));
    run_make(&dir, "package").await
}

pub async fn clean(cwd: &Path) -> Result<()> {
    let dir = find_validator_dir(cwd)?;
    output::info(&format!("Cleaning {}/dist/", dir.display()));
    run_make(&dir, "clean").await
}
