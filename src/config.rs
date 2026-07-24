//! Configuration loading with layered resolution:
//! config file → env vars → CLI flags (highest priority).

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::detect::Accel;
use crate::output;

const CONFIG_FILENAME: &str = "knishio.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub validator: ValidatorConfig,
    pub docker: DockerConfig,
    pub database: DatabaseConfig,
    /// Optional: force this accel, skipping auto-detection. Mostly for CI /
    /// reproducible rigs.  Accepts the same names as the CLI `--accel` flag.
    pub default_accel: Option<String>,
    /// Remote-deployment defaults for the `deploy` family and `--host`.
    pub deploy: DeployConfig,
    /// Provenance of `validator.url` — which layer won precedence. Feeds the
    /// TARGET banner so operators see WHY a URL is in effect. Not part of the
    /// file format.
    #[serde(skip)]
    pub url_source: crate::target::UrlSource,
}

/// `[deploy]` — defaults for remote-deployment commands. All optional; flags
/// override. Replaces the retired `[docker] compose_file` as the home for
/// deployment-shaped configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeployConfig {
    /// Default `--host` (user@host) for ssh-transport commands.
    pub host: Option<String>,
    /// Default `--domain` for edge/bootstrap generation.
    pub domain: Option<String>,
    /// Default target arch for ship/package ("amd64" | "arm64").
    pub arch: Option<String>,
    /// Default remote staging directory for `deploy ship`.
    pub staging_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ValidatorConfig {
    pub url: String,
    pub insecure_tls: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// DEPRECATED (CLI-1): was never consulted for dispatch — accel profiles
    /// drive file selection. Kept for deserialization back-compat only; a
    /// non-default value triggers a warning pointing at `profile`.
    pub compose_file: String,
    /// Stack profile: "dev" (docker-compose.standalone.yml base — dev JWT
    /// secret, permissive CORS, rate limiting off) or "production"
    /// (docker-compose.production.yml base — KNISHIO_ENV=production, _FILE
    /// secrets, TLS on, rate limiting on). Overridable per-run with the
    /// global `--profile` flag. Fixes CLI-1: production.yml was unreachable.
    pub profile: String,
    pub postgres_container: String,
    pub validator_container: String,
    /// Per-accel overlay chains. Keys match `Accel::config_key()`.
    /// Empty/missing keys fall back to baked defaults (see `default_accel_map`).
    pub accel: HashMap<String, AccelProfile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AccelProfile {
    /// Compose filenames (in layering order) for `docker compose -f a -f b …`.
    pub files: Vec<String>,
    /// When true, the validator runs natively on the host rather than in a
    /// container. `start` still brings up whatever's in `files` (typically
    /// just Postgres) and then emits a native-run hint block.
    pub native_validator: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub user: String,
    pub name: String,
}

// ── Defaults ────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            validator: ValidatorConfig::default(),
            docker: DockerConfig::default(),
            database: DatabaseConfig::default(),
            default_accel: None,
            deploy: DeployConfig::default(),
            url_source: crate::target::UrlSource::Default,
        }
    }
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            url: "https://localhost:8080".into(),
            insecure_tls: false,
        }
    }
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            compose_file: "docker-compose.standalone.yml".into(),
            profile: "dev".into(),
            postgres_container: "knishio-postgres".into(),
            validator_container: "knishio-validator".into(),
            accel: default_accel_map(),
        }
    }
}


impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            user: "knishio".into(),
            name: "knishio".into(),
        }
    }
}

/// Baked-in defaults for every accel profile. Written out verbatim in
/// `knishio.toml`'s template so operators can see + override them.
fn default_accel_map() -> HashMap<String, AccelProfile> {
    let mut m = HashMap::new();
    m.insert(
        "cpu".into(),
        AccelProfile {
            files: vec!["docker-compose.standalone.yml".into()],
            native_validator: false,
        },
    );
    m.insert(
        "cuda".into(),
        AccelProfile {
            files: vec![
                "docker-compose.standalone.yml".into(),
                "docker-compose.cuda.yml".into(),
            ],
            native_validator: false,
        },
    );
    m.insert(
        "dmr".into(),
        AccelProfile {
            files: vec![
                "docker-compose.standalone.yml".into(),
                "docker-compose.dmr.yml".into(),
            ],
            native_validator: false,
        },
    );
    m.insert(
        "metal-native".into(),
        AccelProfile {
            files: vec!["docker-compose.metal.yml".into()],
            native_validator: true,
        },
    );
    // R24 C.2: rocm + vulkan removed. The compose files
    // (docker-compose.rocm.yml, docker-compose.vulkan.yml) don't exist in
    // the validator repo — pre-R24 `knishio start --accel rocm` failed with
    // a cryptic "compose file not found" error. The AccelFlag::Rocm/Vulkan
    // and Accel::Rocm/Vulkan variants remain (clap parses them) but
    // `cfg.accel_files()` returns an empty list, triggering the
    // resolve_accel_and_files fallback path with a clear "no configured
    // compose files; falling back to cpu" message. Restore these entries
    // once docker-compose.rocm.yml + docker-compose.vulkan.yml ship.
    m
}

// ── Loading ─────────────────────────────────────────────────

impl Config {
    /// Load config from file (if found), then apply env var overrides.
    pub fn load(search_start: &Path) -> Self {
        let mut config = match find_config_file(search_start) {
            Some(path) => match Self::from_file(&path) {
                Ok(mut cfg) => {
                    output::info(&format!("Config loaded from {}", path.display()));
                    if cfg.validator.url != crate::target::DEFAULT_URL {
                        cfg.url_source = crate::target::UrlSource::ConfigFile;
                    }
                    cfg
                }
                Err(e) => {
                    output::warn(&format!("Failed to parse {}: {}", path.display(), e));
                    Config::default()
                }
            },
            None => Config::default(),
        };

        // Ensure baked defaults are present for any accel profile the config
        // file didn't override explicitly.
        for (k, v) in default_accel_map() {
            config.docker.accel.entry(k).or_insert(v);
        }

        config.apply_env_overrides();
        config
    }

    fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("KNISHIO_URL") {
            self.validator.url = val;
            self.url_source = crate::target::UrlSource::Env;
        }
        if let Ok(val) = std::env::var("KNISHIO_PG_CONTAINER") {
            self.docker.postgres_container = val;
        }
        if let Ok(val) = std::env::var("KNISHIO_VALIDATOR_CONTAINER") {
            self.docker.validator_container = val;
        }
        if let Ok(val) = std::env::var("KNISHIO_DB_USER") {
            self.database.user = val;
        }
        if let Ok(val) = std::env::var("KNISHIO_DB_NAME") {
            self.database.name = val;
        }
        if let Ok(val) = std::env::var("KNISHIO_INSECURE_TLS") {
            self.validator.insecure_tls =
                val.eq_ignore_ascii_case("true") || val == "1";
        }
        if let Ok(val) = std::env::var("KNISHIO_ACCEL") {
            self.default_accel = Some(val);
        }
    }

    /// Apply an explicit `--url` flag (highest precedence). `None` = flag not
    /// passed. Replaces the old `with_url_override` compare-against-default
    /// heuristic, which could neither label the source honestly nor let an
    /// explicit `--url https://localhost:8080` beat a config-file URL.
    pub fn apply_url_flag(&mut self, flag: Option<&str>) {
        if let Some(url) = flag {
            self.validator.url = url.to_string();
            self.url_source = crate::target::UrlSource::Flag;
        }
    }

    /// Apply an explicit `--profile` flag over `[docker] profile`, validating
    /// the value. Also warns about the deprecated, never-honored
    /// `compose_file` field when a config file set it to a non-default.
    pub fn apply_profile_flag(&mut self, flag: Option<&str>) -> Result<()> {
        if let Some(p) = flag {
            self.docker.profile = p.to_string();
        }
        match self.docker.profile.as_str() {
            "dev" | "production" => {}
            other => anyhow::bail!(
                "unknown stack profile `{}` — expected \"dev\" or \"production\"",
                other
            ),
        }
        if self.docker.compose_file != "docker-compose.standalone.yml" {
            output::warn(
                "[docker] compose_file is deprecated and was never honored for stack \
                 selection — use `profile = \"production\"` (or --profile) instead.",
            );
        }
        Ok(())
    }

    /// Map the accel overlay chain through the active profile: "production"
    /// swaps the standalone base for docker-compose.production.yml. Overlay
    /// files (cuda/dmr/…) are unaffected.
    pub fn profiled_files(&self, files: &[String]) -> Vec<String> {
        if self.docker.profile != "production" {
            return files.to_vec();
        }
        let mut swapped = false;
        let out: Vec<String> = files
            .iter()
            .map(|f| {
                if f == "docker-compose.standalone.yml" {
                    swapped = true;
                    "docker-compose.production.yml".to_string()
                } else {
                    f.clone()
                }
            })
            .collect();
        if !swapped {
            output::warn(
                "profile \"production\" set, but the resolved stack has no \
                 docker-compose.standalone.yml base to swap (native/metal chains \
                 run the validator outside compose) — files unchanged.",
            );
        }
        out
    }

    /// Look up the overlay file list for the given accel, falling back to CPU
    /// (with a warning emitted by the caller) if the profile has no `files`.
    pub fn accel_files(&self, accel: Accel) -> &[String] {
        self.docker
            .accel
            .get(accel.config_key())
            .map(|p| p.files.as_slice())
            .unwrap_or(&[])
    }

    /// Whether this accel wants the validator to run natively on the host.
    pub fn accel_is_native(&self, accel: Accel) -> bool {
        self.docker
            .accel
            .get(accel.config_key())
            .map(|p| p.native_validator)
            .unwrap_or(false)
    }
}

/// Walk up from `start` looking for knishio.toml.
fn find_config_file(start: &Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.exists() {
            return Some(candidate);
        }

        let candidate = dir.join("knishio-validator-rust").join(CONFIG_FILENAME);
        if candidate.exists() {
            return Some(candidate);
        }

        let candidate = dir
            .join("servers")
            .join("knishio-validator-rust")
            .join(CONFIG_FILENAME);
        if candidate.exists() {
            return Some(candidate);
        }

        if !dir.pop() {
            break;
        }
    }
    None
}
