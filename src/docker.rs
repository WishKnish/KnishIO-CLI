//! Docker Compose control — start, stop, destroy, rebuild, logs, status.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::output;

/// Run `docker compose -f <file> <args...>`, inheriting stdout/stderr.
async fn compose(compose_file: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(compose_file)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to run docker compose — is Docker installed?")?;
    Ok(status.success())
}

pub async fn start(compose_file: &Path, build: bool, detach: bool) -> Result<()> {
    output::info("Starting KnishIO validator stack...");
    let mut args = vec!["up"];
    if build {
        args.push("--build");
    }
    if detach {
        args.push("-d");
    }
    if compose(compose_file, &args).await? {
        if detach {
            output::success("Stack is running");
        }
    } else {
        output::error("docker compose up failed");
    }
    Ok(())
}

pub async fn stop(compose_file: &Path) -> Result<()> {
    output::info("Stopping KnishIO validator stack...");
    if compose(compose_file, &["stop"]).await? {
        output::success("Stack stopped");
    } else {
        output::error("docker compose stop failed");
    }
    Ok(())
}

pub async fn destroy(compose_file: &Path, volumes: bool) -> Result<()> {
    output::warn("Destroying KnishIO validator stack...");
    let mut args = vec!["down"];
    if volumes {
        args.push("-v");
        output::warn("Volumes will be removed (all data lost)");
    }
    if compose(compose_file, &args).await? {
        output::success("Stack destroyed");
    } else {
        output::error("docker compose down failed");
    }
    Ok(())
}

pub async fn rebuild(compose_file: &Path) -> Result<()> {
    output::info("Rebuilding KnishIO validator (no cache)...");
    compose(compose_file, &["build", "--no-cache"]).await?;
    output::info("Starting rebuilt stack...");
    if compose(compose_file, &["up", "-d"]).await? {
        output::success("Rebuilt and running");
    } else {
        output::error("Failed to start after rebuild");
    }
    Ok(())
}

pub async fn logs(compose_file: &Path, follow: bool, tail: Option<usize>) -> Result<()> {
    let mut args = vec!["logs"];
    if follow {
        args.push("--follow");
    }
    let tail_str;
    if let Some(n) = tail {
        tail_str = format!("{}", n);
        args.push("--tail");
        args.push(&tail_str);
    }
    compose(compose_file, &args).await?;
    Ok(())
}

pub async fn status(compose_file: &Path) -> Result<()> {
    compose(compose_file, &["ps"]).await?;
    Ok(())
}
