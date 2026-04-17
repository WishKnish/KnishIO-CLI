//! Docker Model Runner (DMR) helpers — thin wrappers over `docker model …`
//! and the Docker Desktop CLI used by the `knishio dmr …` subcommand group.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::Command;

use crate::detect;
use crate::output;

/// Print current DMR status — client version, server state, TCP endpoint,
/// cached model list. Human-readable; matches the shape of `detect::print_summary`
/// but standalone so operators can use it without the Environment header.
pub async fn status() -> Result<()> {
    let env = detect::detect();

    if env.os != "macos" {
        output::warn("Docker Model Runner is macOS-only; not relevant on this host.");
        return Ok(());
    }

    output::header("Docker Model Runner status");

    if !env.dmr.client_present {
        output::warn("client: not installed");
        output::info("  → install via Docker Desktop 4.62+ (Settings → Beta/AI → enable Model Runner)");
        return Ok(());
    }
    output::success("client: installed");

    if env.dmr.server_running {
        output::success("server: running");
    } else {
        output::warn("server: not running");
        output::info("  → enable in Docker Desktop Settings, then retry");
        return Ok(());
    }

    if env.dmr.tcp_reachable {
        output::success("TCP :12434: reachable (containers can hit model-runner.docker.internal)");
    } else {
        output::warn("TCP :12434: not exposed");
        output::info(
            "  → run: docker desktop enable model-runner --tcp=12434",
        );
    }

    if env.dmr.models.is_empty() {
        output::info("models: none pulled yet");
        output::info("  → docker model pull hf.co/Qwen/Qwen3-Embedding-4B-GGUF");
        output::info("  → docker model pull hf.co/Qwen/Qwen3.5-0.8B-GGUF");
    } else {
        output::info(&format!("models ({}):", env.dmr.models.len()));
        for m in &env.dmr.models {
            println!("    · {}", m);
        }
    }

    Ok(())
}

/// Enable DMR's TCP endpoint (the bit that containers need to reach the host
/// inference server) and optionally offer to pull the two default Qwen
/// models. Docker Desktop itself is enabled through its GUI — this only flips
/// the TCP-exposure toggle, which IS CLI-controllable.
pub async fn enable() -> Result<()> {
    output::header("Enabling Docker Model Runner TCP endpoint");

    let status = Command::new("docker")
        .args(["desktop", "enable", "model-runner", "--tcp=12434"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to run `docker desktop enable model-runner`")?;

    if !status.success() {
        output::error("docker desktop enable model-runner failed");
        output::info(
            "  → ensure Docker Desktop 4.62+ is running and Model Runner is enabled in Settings",
        );
        return Ok(());
    }

    output::success("DMR TCP endpoint enabled on :12434");
    output::info("  → verify: knishio dmr status");
    output::info("  → next:   knishio dmr pull   (fetches default models)");
    Ok(())
}

/// Pull a model into the DMR cache. Accepts the same ref forms the
/// Docker Model Runner CLI understands (`hf.co/<owner>/<repo>`,
/// `ai/<name>`, full OCI refs). If `model` is `None`, pulls the two
/// defaults used by our `docker-compose.dmr.yml` overlay.
pub async fn pull(model: Option<String>) -> Result<()> {
    let refs: Vec<String> = match model {
        Some(m) => vec![m],
        None => vec![
            "hf.co/Qwen/Qwen3-Embedding-4B-GGUF".into(),
            "hf.co/Qwen/Qwen3.5-0.8B-GGUF".into(),
        ],
    };

    for r in &refs {
        output::info(&format!("Pulling {r} …"));
        let status = Command::new("docker")
            .args(["model", "pull", r])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("Failed to run `docker model pull` — is Docker installed?")?;

        if status.success() {
            output::success(&format!("{r} cached"));
        } else {
            output::error(&format!("pull failed for {r}"));
        }
    }

    Ok(())
}
