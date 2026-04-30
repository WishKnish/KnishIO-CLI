//! Docker Compose control — start, stop, destroy, rebuild, logs, status, psql.
//!
//! Every public function accepts `files: &[PathBuf]` so an accel profile can
//! assemble overlay chains like `docker compose -f base.yml -f cuda.yml …`.
//! Callers resolve the chain via `crate::paths::find_compose_files`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

use crate::config::Config;
use crate::output;

/// Run `docker compose -f <file1> [-f <file2>…] <args...>`, inheriting
/// stdout/stderr. Auto-detects `.env.production` when a `docker-compose.production.yml`
/// layer is present in the chain.
/// Resolve a user-supplied generation-model short alias to a full
/// DMR model reference. Unknown strings pass through unchanged so
/// operators can supply arbitrary `huggingface.co/...` or `ai/...`
/// refs directly.
pub fn resolve_gen_model(name: &str) -> String {
    match name {
        "gemma" | "gemma-4-e4b" => "huggingface.co/unsloth/gemma-4-e4b-it-gguf:latest".into(),
        "qwen3.5" | "qwen3.5-0.8b" => "huggingface.co/unsloth/qwen3.5-0.8b-gguf:latest".into(),
        "qwen3-0.6b" => "huggingface.co/unsloth/qwen3-0.6b-gguf:latest".into(),
        other => other.to_string(),
    }
}

async fn compose(files: &[PathBuf], args: &[&str], env: &[(&str, &str)]) -> Result<bool> {
    if files.is_empty() {
        return Err(anyhow::anyhow!(
            "compose() called with no compose files — this is a bug"
        ));
    }

    let mut cmd = Command::new("docker");
    cmd.arg("compose");
    for f in files {
        cmd.arg("-f").arg(f);
    }

    // Inject any env overrides (e.g. GENERATION_MODEL from --gen-model).
    // Docker compose inherits the subprocess env and expands `${VAR:-…}`
    // against it, so setting these here shadows whatever the shell had.
    for (k, v) in env {
        cmd.env(k, v);
    }

    // R22 A.2: auto-detect .env.production whenever it exists in the validator
    // dir, regardless of which compose layer is active. R19 Phase F made
    // .env.production the canonical operator surface for env vars; it should
    // drive `${VAR:-default}` interpolation in dmr.yml / standalone.yml /
    // production.yml uniformly. Pre-fix this only fired when "production" was
    // in the file path — defeating operator-set EMBEDDING_MODEL on the
    // dmr-only stack.
    if let Some(dir) = files[0].parent() {
        let env_production = dir.join(".env.production");
        if env_production.exists() {
            cmd.arg("--env-file").arg(&env_production);
        }
    }

    let status = cmd
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to run docker compose — is Docker installed?")?;
    Ok(status.success())
}

pub async fn start(
    files: &[PathBuf],
    build: bool,
    detach: bool,
    env: &[(&str, &str)],
) -> Result<()> {
    output::info("Starting KnishIO validator stack...");
    let mut args = vec!["up"];
    if build {
        args.push("--build");
    }
    if detach {
        args.push("-d");
    }
    if compose(files, &args, env).await? {
        if detach {
            output::success("Stack is running");
        }
    } else {
        output::error("docker compose up failed");
    }
    Ok(())
}

pub async fn stop(files: &[PathBuf]) -> Result<()> {
    output::info("Stopping KnishIO validator stack...");
    if compose(files, &["stop"], &[]).await? {
        output::success("Stack stopped");
    } else {
        output::error("docker compose stop failed");
    }
    Ok(())
}

pub async fn destroy(files: &[PathBuf], volumes: bool) -> Result<()> {
    output::warn("Destroying KnishIO validator stack...");
    let mut args = vec!["down"];
    if volumes {
        args.push("-v");
        output::warn("Volumes will be removed (all data lost)");
    }
    if compose(files, &args, &[]).await? {
        output::success("Stack destroyed");
    } else {
        output::error("docker compose down failed");
    }
    Ok(())
}

pub async fn rebuild(files: &[PathBuf], env: &[(&str, &str)]) -> Result<()> {
    output::info("Rebuilding KnishIO validator (no cache)...");
    compose(files, &["build", "--no-cache"], env).await?;
    output::info("Starting rebuilt stack...");
    if compose(files, &["up", "-d"], env).await? {
        output::success("Rebuilt and running");
    } else {
        output::error("Failed to start after rebuild");
    }
    Ok(())
}

pub async fn logs(files: &[PathBuf], follow: bool, tail: Option<usize>) -> Result<()> {
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
    compose(files, &args, &[]).await?;
    Ok(())
}

pub async fn status(files: &[PathBuf]) -> Result<()> {
    compose(files, &["ps"], &[]).await?;
    Ok(())
}

/// Open an interactive psql session or run a single SQL command.
/// Doesn't take a compose file list — it goes straight to `docker exec`.
pub async fn psql(cfg: &Config, sql_command: Option<&str>) -> Result<()> {
    let container = &cfg.docker.postgres_container;
    let db_user = &cfg.database.user;
    let db_name = &cfg.database.name;

    let mut args = vec!["exec"];

    if sql_command.is_none() {
        // Interactive mode needs -it
        args.push("-it");
    }

    args.extend_from_slice(&[container.as_str(), "psql", "-U", db_user, "-d", db_name]);

    if let Some(cmd) = sql_command {
        args.push("-c");
        args.push(cmd);
    }

    let status = Command::new("docker")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("Failed to run psql — is the postgres container running?")?;

    if !status.success() {
        output::error("psql session ended with error");
    }
    Ok(())
}

/// Print the Metal-native "next step" block for operators who chose that
/// fallback path. Emitted after `start --accel metal-native` when DMR is
/// unavailable. The hint points at the cargo invocation that spawns a native
/// Metal-accelerated validator binary against the containerised Postgres.
pub fn print_metal_native_hint(_cwd: &Path, cfg: &Config) {
    output::header("Next step (Metal-native path):");
    println!(
        "  The validator cannot run inside a Linux container with Metal GPU\n\
         access. Postgres is up via this stack; finish the setup natively.\n\
         The `metal` feature cascades to `llama-cpp`, so the provider env\n\
         var below is enabled by the rebuild:\n\n\
         \x20 cd servers/knishio-validator-rust\n\
         \x20 cargo build --release --features metal\n\
         \x20 export DATABASE_URL=\"{}\"\n\
         \x20 export EMBEDDING_ENABLED=true\n\
         \x20 export EMBEDDING_PROVIDER=llama-cpp\n\
         \x20 export EMBEDDING_MODEL_PATH=./models/Qwen3-Embedding-4B-Q8_0.gguf\n\
         \x20 export EMBEDDING_GPU_LAYERS=999\n\
         \x20 ./target/release/knishio-validator\n\n\
         Or, to skip this extra step, enable Docker Model Runner:\n\
         \x20 docker desktop enable model-runner --tcp=12434\n\
         \x20 docker model pull hf.co/Qwen/Qwen3-Embedding-4B-GGUF\n\
         \x20 knishio start --accel dmr",
        db_url_for_native(cfg)
    );
}

fn db_url_for_native(cfg: &Config) -> String {
    format!(
        "postgres://{}:{}@localhost:5432/{}",
        cfg.database.user, cfg.database.user, cfg.database.name
    )
}
