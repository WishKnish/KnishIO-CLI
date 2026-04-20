//! Colored terminal output helpers.
//!
//! Convention: **meta-messages go to stderr**, actual command data goes
//! to stdout. That makes pipes work cleanly — `knishio metrics --raw |
//! prom-parser` only sees the Prometheus body, not the "ℹ Config
//! loaded from …" banner; `knishio ai status > snapshot.txt` writes
//! only the status body, not the meta-chatter.
//!
//! - `success` / `info` / `warn` / `error` — status / progress /
//!   diagnostics about the command (stderr).
//! - `header` — section label *within* the command's rendered output
//!   (stdout). Do not use for meta-chatter.

use colored::Colorize;

pub fn success(msg: &str) {
    eprintln!("{} {}", "✓".green().bold(), msg);
}

pub fn info(msg: &str) {
    eprintln!("{} {}", "ℹ".blue().bold(), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", "⚠".yellow().bold(), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", "✗".red().bold(), msg);
}

pub fn header(msg: &str) {
    println!("\n{}", msg.bold().underline());
}
