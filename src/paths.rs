//! Path discovery for Docker Compose files.

use std::path::{Path, PathBuf};

const VALIDATOR_DIR: &str = "knishio-validator-rust";

/// Walk up from `start` looking for the validator's docker-compose file.
/// The `compose_filename` comes from config (e.g. "docker-compose.standalone.yml").
pub fn find_compose_file(start: &Path, compose_filename: &str) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        // Direct: we're inside the validator dir
        let candidate = dir.join(compose_filename);
        if candidate.exists() {
            return Some(candidate);
        }

        // One level up: servers/knishio-validator-rust/
        let candidate = dir.join(VALIDATOR_DIR).join(compose_filename);
        if candidate.exists() {
            return Some(candidate);
        }

        // Two levels up: inside a monorepo with servers/ prefix
        let candidate = dir.join("servers").join(VALIDATOR_DIR).join(compose_filename);
        if candidate.exists() {
            return Some(candidate);
        }

        if !dir.pop() {
            break;
        }
    }
    None
}
