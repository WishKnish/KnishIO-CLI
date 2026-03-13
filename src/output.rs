//! Colored terminal output helpers.

use colored::Colorize;

pub fn success(msg: &str) {
    println!("{} {}", "✓".green().bold(), msg);
}

pub fn info(msg: &str) {
    println!("{} {}", "ℹ".blue().bold(), msg);
}

pub fn warn(msg: &str) {
    println!("{} {}", "⚠".yellow().bold(), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", "✗".red().bold(), msg);
}

pub fn header(msg: &str) {
    println!("\n{}", msg.bold().underline());
}
