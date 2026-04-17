//! Path discovery for Docker Compose files.

use anyhow::{anyhow, Result};
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

/// Resolve a list of compose filenames to absolute paths, preserving input
/// order (so base-then-overlay semantics stay intact when passed to
/// `docker compose -f a.yml -f b.yml`).
///
/// Errors if any filename cannot be found under the validator tree; the error
/// message names the missing file so the operator knows which overlay to ship.
pub fn find_compose_files(start: &Path, filenames: &[String]) -> Result<Vec<PathBuf>> {
    if filenames.is_empty() {
        return Err(anyhow!("no compose files configured for this accel profile"));
    }

    let mut resolved = Vec::with_capacity(filenames.len());
    for name in filenames {
        match find_compose_file(start, name) {
            Some(path) => resolved.push(path),
            None => {
                return Err(anyhow!(
                    "compose file '{}' not found; walk up from {:?} didn't locate it \
                     under the validator tree. \
                     Did the overlay ship in this version of the repo?",
                    name,
                    start
                ));
            }
        }
    }
    Ok(resolved)
}
