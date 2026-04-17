//! Host environment detection for accel profile auto-selection.
//!
//! The public API is:
//! - [`detect`] — synchronous probe returning the resolved [`Environment`]
//! - [`print_summary`] — colored multi-line Environment block
//! - [`Accel`] — the resolved recommendation; feeds into `config::accel_files`
//!
//! Detection is intentionally fast (< ~200ms total): all probes use short
//! timeouts and run serially. Every `knishio` docker command re-detects on
//! each invocation so the output is always current.

use colored::Colorize;
use std::fmt;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The resolved hardware-acceleration profile for the current host.
///
/// Variants map 1:1 to the `[docker.accel.<name>]` tables in `knishio.toml`.
/// `auto` is a CLI-level sentinel meaning "call `detect()` and use its
/// result"; by the time we hit the docker layer, a concrete variant has
/// been chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Portable CPU-only stack.
    Cpu,
    /// NVIDIA GPU, containerised validator + nvidia-container-toolkit.
    Cuda,
    /// Apple Silicon via Docker Model Runner (llama.cpp-latest-metal host daemon).
    Dmr,
    /// Apple Silicon fallback: validator runs natively; Postgres in Docker only.
    MetalNative,
    /// AMD GPU via ROCm (overlay not yet shipped).
    Rocm,
    /// Cross-vendor GPU via Vulkan (overlay not yet shipped).
    Vulkan,
}

impl Accel {
    /// The toml key under `[docker.accel.<key>]`.
    pub fn config_key(self) -> &'static str {
        match self {
            Accel::Cpu => "cpu",
            Accel::Cuda => "cuda",
            Accel::Dmr => "dmr",
            Accel::MetalNative => "metal-native",
            Accel::Rocm => "rocm",
            Accel::Vulkan => "vulkan",
        }
    }

}

impl fmt::Display for Accel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.config_key())
    }
}

/// Snapshot of the host environment, populated by [`detect`].
#[derive(Debug, Clone)]
pub struct Environment {
    pub os: &'static str,
    pub arch: &'static str,
    pub cpu_brand: String,
    pub memory_gb: Option<u64>,
    pub docker: DockerStatus,
    pub gpu: Option<GpuInfo>,
    pub dmr: DmrStatus,
    pub accel: Accel,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DockerStatus {
    pub present: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DmrStatus {
    pub client_present: bool,
    pub server_running: bool,
    pub tcp_reachable: bool,
    /// Cached model IDs visible at `/engines/v1/models`. Populated on a best-
    /// effort basis; empty when the endpoint isn't reachable.
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Apple,
    Nvidia,
    Amd,
}

// ── Public entry points ────────────────────────────────────────

/// Probe the host and return its resolved accel profile + metadata.
pub fn detect() -> Environment {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cpu_brand = probe_cpu_brand(os).unwrap_or_else(|| "unknown".to_string());
    let memory_gb = probe_memory_gb(os);
    let docker = probe_docker();
    let dmr = if os == "macos" {
        probe_dmr()
    } else {
        DmrStatus {
            client_present: false,
            server_running: false,
            tcp_reachable: false,
            models: Vec::new(),
        }
    };
    let nvidia_present = probe_binary_ok("nvidia-smi", &["-L"]);
    let rocm_present = probe_binary_ok("rocminfo", &[]);
    let gpu = pick_gpu(os, &cpu_brand, nvidia_present, rocm_present);

    let mut reasons = Vec::new();
    let accel = resolve_accel(
        os,
        arch,
        &docker,
        &dmr,
        nvidia_present,
        rocm_present,
        &mut reasons,
    );

    Environment {
        os,
        arch,
        cpu_brand,
        memory_gb,
        docker,
        gpu,
        dmr,
        accel,
        reasons,
    }
}

/// Colorised multi-line Environment block written to stdout.
pub fn print_summary(env: &Environment) {
    crate::output::header("Environment");

    let info = |label: &str, value: String| {
        println!("{} {:8} {}", "ℹ".blue().bold(), label, value);
    };

    info("Host:", format!("{} ({})", env.os, env.arch));

    let cpu_line = match env.memory_gb {
        Some(gb) => format!("{} · {} GB RAM", env.cpu_brand, gb),
        None => env.cpu_brand.clone(),
    };
    info("CPU:", cpu_line);

    match &env.gpu {
        Some(gpu) => info("GPU:", format!("{} ({:?})", gpu.name, gpu.vendor)),
        None => info("GPU:", "none detected".into()),
    }

    let docker_line = if env.docker.present {
        env.docker
            .version
            .clone()
            .unwrap_or_else(|| "running".into())
    } else {
        "not available".into()
    };
    info("Docker:", docker_line);

    if env.os == "macos" {
        let dmr_line = match (&env.dmr.client_present, &env.dmr.tcp_reachable) {
            (true, true) => format!(
                "running, TCP :12434 reachable, {} cached model(s)",
                env.dmr.models.len()
            ),
            (true, false) if env.dmr.server_running => {
                "running but TCP not exposed (run: docker desktop enable model-runner --tcp=12434)"
                    .to_string()
            }
            (true, false) => "client installed, server not running".into(),
            (false, _) => "not installed".into(),
        };
        info("DMR:", dmr_line);
    }

    let arrow = "→".bold();
    let accel_reason = if env.reasons.is_empty() {
        String::new()
    } else {
        format!("  ({})", env.reasons.join("; "))
    };
    println!(
        "{} {:8} {}{}",
        arrow,
        "Accel:",
        env.accel.to_string().bold().green(),
        accel_reason
    );
}

// ── Resolution logic ───────────────────────────────────────────

fn resolve_accel(
    os: &str,
    arch: &str,
    docker: &DockerStatus,
    dmr: &DmrStatus,
    nvidia_present: bool,
    rocm_present: bool,
    reasons: &mut Vec<String>,
) -> Accel {
    if !docker.present {
        reasons.push("docker not available — falling back to cpu profile anyway".into());
        return Accel::Cpu;
    }

    if os == "macos" && arch == "aarch64" {
        if dmr.tcp_reachable {
            reasons.push("Apple Silicon + DMR TCP reachable".into());
            return Accel::Dmr;
        }
        reasons.push("Apple Silicon; DMR not reachable — using metal-native fallback".into());
        return Accel::MetalNative;
    }

    if nvidia_present {
        reasons.push("nvidia-smi present".into());
        return Accel::Cuda;
    }

    if rocm_present {
        reasons.push("rocminfo present".into());
        return Accel::Rocm;
    }

    reasons.push("no accelerator detected".into());
    Accel::Cpu
}

// ── Probes ────────────────────────────────────────────────────

fn probe_cpu_brand(os: &str) -> Option<String> {
    match os {
        "macos" => run_capture("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "linux" => {
            let content = std::fs::read_to_string("/proc/cpuinfo").ok()?;
            content
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        }
        _ => None,
    }
}

fn probe_memory_gb(os: &str) -> Option<u64> {
    match os {
        "macos" => {
            let bytes: u64 = run_capture("sysctl", &["-n", "hw.memsize"])?
                .trim()
                .parse()
                .ok()?;
            Some(bytes / 1024 / 1024 / 1024)
        }
        "linux" => {
            let content = std::fs::read_to_string("/proc/meminfo").ok()?;
            let line = content.lines().find(|l| l.starts_with("MemTotal:"))?;
            let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            Some(kb / 1024 / 1024)
        }
        _ => None,
    }
}

fn probe_docker() -> DockerStatus {
    let version = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty());

    DockerStatus {
        present: version.is_some(),
        version,
    }
}

/// macOS-only DMR probe: checks `docker model version` (client present),
/// `docker model status` (server running), and finally a TCP probe of the
/// OAI-compatible endpoint (only populated when the user has explicitly
/// enabled TCP via `docker desktop enable model-runner --tcp=12434`).
fn probe_dmr() -> DmrStatus {
    let client_present = Command::new("docker")
        .args(["model", "version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
        .unwrap_or(false);

    if !client_present {
        return DmrStatus {
            client_present: false,
            server_running: false,
            tcp_reachable: false,
            models: Vec::new(),
        };
    }

    let server_running = Command::new("docker")
        .args(["model", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.to_lowercase().contains("is running"))
        .unwrap_or(false);

    // Short, blocking-ish TCP probe. reqwest would be async-only and we're in
    // a sync detect path; use std::net::TcpStream with a short timeout.
    let tcp_reachable = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:12434".parse().expect("static socket addr"),
        Duration::from_millis(500),
    )
    .is_ok();

    // Best-effort model list; ignored on error. Uses curl to avoid pulling an
    // HTTP client dep into the sync detection path.
    let models = if tcp_reachable {
        Command::new("curl")
            .args([
                "-s",
                "-m",
                "2",
                "http://localhost:12434/engines/v1/models",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| parse_model_ids(&s))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    DmrStatus {
        client_present,
        server_running,
        tcp_reachable,
        models,
    }
}

/// Minimal JSON parse of `{"object":"list","data":[{"id":"…"}, …]}` without
/// pulling `serde_json` just for this. Scans for `"id":"<value>"` tokens.
fn parse_model_ids(json: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(idx) = rest.find("\"id\":\"") {
        rest = &rest[idx + 6..];
        let end = rest.find('"')?;
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    Some(out)
}

/// Returns true iff the binary exists on PATH and, when called with `args`,
/// exits 0.
fn probe_binary_ok(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pick_gpu(
    os: &str,
    cpu_brand: &str,
    nvidia_present: bool,
    rocm_present: bool,
) -> Option<GpuInfo> {
    // Apple Silicon: CPU brand doubles as GPU identity (unified SoC).
    if os == "macos" && cpu_brand.contains("Apple") {
        return Some(GpuInfo {
            vendor: GpuVendor::Apple,
            name: cpu_brand.to_string(),
        });
    }
    if nvidia_present {
        // Best-effort: parse `nvidia-smi -L` first line
        let name = run_capture("nvidia-smi", &["-L"])
            .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            .unwrap_or_else(|| "NVIDIA GPU".into());
        return Some(GpuInfo {
            vendor: GpuVendor::Nvidia,
            name,
        });
    }
    if rocm_present {
        return Some(GpuInfo {
            vendor: GpuVendor::Amd,
            name: "AMD GPU (rocminfo)".into(),
        });
    }
    None
}

fn run_capture(bin: &str, args: &[&str]) -> Option<String> {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
}
