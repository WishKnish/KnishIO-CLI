//! Path discovery for Docker Compose files and the knishio-bench binary.

use std::path::{Path, PathBuf};

const VALIDATOR_DIR: &str = "knishio-validator-rust";
const BENCH_BINARY: &str = "knishio-bench";

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

/// Locate the knishio-bench binary.
/// Checks: (1) sibling target dir, (2) PATH
pub fn find_bench_binary(compose_file: &Path) -> Option<PathBuf> {
    if let Some(validator_dir) = compose_file.parent() {
        if let Some(servers_dir) = validator_dir.parent() {
            for profile in ["release", "debug"] {
                let candidate = servers_dir
                    .join("knishio-bench")
                    .join("target")
                    .join(profile)
                    .join(BENCH_BINARY);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }

    // Fallback: check if it's on PATH
    which_in_path(BENCH_BINARY)
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}
