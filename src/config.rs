//! Configuration loading with layered resolution:
//! config file → env vars → CLI flags (highest priority).

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

use crate::output;

const CONFIG_FILENAME: &str = "knishio.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub validator: ValidatorConfig,
    pub docker: DockerConfig,
    pub database: DatabaseConfig,
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
    pub compose_file: String,
    pub postgres_container: String,
    pub validator_container: String,
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
            postgres_container: "knishio-postgres".into(),
            validator_container: "knishio-validator".into(),
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

// ── Loading ─────────────────────────────────────────────────

impl Config {
    /// Load config from file (if found), then apply env var overrides.
    pub fn load(search_start: &Path) -> Self {
        let mut config = match find_config_file(search_start) {
            Some(path) => match Self::from_file(&path) {
                Ok(cfg) => {
                    output::info(&format!("Config loaded from {}", path.display()));
                    cfg
                }
                Err(e) => {
                    output::warn(&format!("Failed to parse {}: {}", path.display(), e));
                    Config::default()
                }
            },
            None => Config::default(),
        };

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
    }

    /// Apply CLI flag override for the validator URL.
    /// Only overrides if the user explicitly passed --url (not the default).
    pub fn with_url_override(mut self, cli_url: &str) -> Self {
        // clap always provides a value (default or explicit), so we check
        // if it differs from the compiled-in default to detect explicit use.
        // This isn't perfect but covers the common case.
        let clap_default = "https://localhost:8080";
        if cli_url != clap_default || self.validator.url == clap_default {
            self.validator.url = cli_url.to_string();
        }
        self
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
