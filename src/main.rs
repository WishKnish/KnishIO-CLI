//! KnishIO CLI — unified orchestration for the KnishIO validator stack.
//!
//! ## Usage
//!
//! ```bash
//! # Docker control
//! knishio start --build -d
//! knishio stop
//! knishio destroy --volumes
//! knishio rebuild
//! knishio logs --follow --tail 100
//! knishio status
//!
//! # Cell management
//! knishio cell create TESTCELL --name "Test Cell"
//! knishio cell list
//! knishio cell activate TESTCELL
//! knishio cell pause TESTCELL
//! knishio cell archive TESTCELL
//!
//! # Benchmarks
//! knishio bench run --types meta,value-transfer --identities 50 --concurrency 5
//! knishio bench generate --types meta -o plan.db
//! knishio bench execute plan.db --concurrency 10
//!
//! # Health checks
//! knishio health
//! knishio ready
//! knishio full
//! knishio db
//! ```

mod bench;
mod cell;
mod config;
mod docker;
mod health;
mod output;
mod paths;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;

// ═══════════════════════════════════════════════════════════════
// CLI Definitions
// ═══════════════════════════════════════════════════════════════

#[derive(Parser)]
#[command(
    name = "knishio",
    about = "KnishIO Validator Orchestration CLI",
    version,
    propagate_version = true
)]
struct Cli {
    /// Validator base URL (for health commands)
    #[arg(long, global = true, default_value = "https://localhost:8080")]
    url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the validator stack (docker compose up)
    Start {
        /// Build images before starting
        #[arg(long)]
        build: bool,

        /// Run in detached mode
        #[arg(short, long)]
        detach: bool,
    },

    /// Stop the validator stack
    Stop,

    /// Destroy the validator stack (docker compose down)
    Destroy {
        /// Also remove volumes (all data lost!)
        #[arg(long)]
        volumes: bool,
    },

    /// Rebuild validator image from scratch and restart
    Rebuild,

    /// Show container logs
    Logs {
        /// Follow log output
        #[arg(short, long)]
        follow: bool,

        /// Number of lines to show from end
        #[arg(long)]
        tail: Option<usize>,
    },

    /// Show container status
    Status,

    /// Cell management
    Cell {
        #[command(subcommand)]
        command: CellCommands,
    },

    /// Benchmark operations
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },

    /// Quick health check (GET /healthz)
    Health,

    /// Readiness check (GET /readyz)
    Ready,

    /// Full readiness detail (GET /readyz with body)
    Full,

    /// Database consistency check (GET /db-check)
    Db,
}

#[derive(Subcommand)]
enum CellCommands {
    /// Create a new cell
    Create {
        /// Cell slug identifier
        slug: String,

        /// Human-readable name (defaults to slug)
        #[arg(long)]
        name: Option<String>,

        /// Initial status
        #[arg(long, default_value = "active")]
        status: String,
    },

    /// List all cells
    List,

    /// Activate a cell
    Activate {
        /// Cell slug
        slug: String,
    },

    /// Pause a cell
    Pause {
        /// Cell slug
        slug: String,
    },

    /// Archive a cell
    Archive {
        /// Cell slug
        slug: String,
    },
}

#[derive(Subcommand)]
enum BenchCommands {
    /// Generate a plan and execute it in one shot
    Run {
        /// Number of identities
        #[arg(long, default_value_t = 50)]
        identities: usize,

        /// Comma-separated molecule types (meta, value-transfer, rule, burn)
        #[arg(long, default_value = "meta", value_delimiter = ',')]
        types: Vec<String>,

        /// Meta mutations per identity
        #[arg(long, default_value_t = 100)]
        metas_per_identity: usize,

        /// Value transfers per identity
        #[arg(long, default_value_t = 10)]
        transfers_per_identity: usize,

        /// Rule molecules per identity
        #[arg(long, default_value_t = 5)]
        rules_per_identity: usize,

        /// Burn molecules per identity
        #[arg(long, default_value_t = 5)]
        burns_per_identity: usize,

        /// Initial token supply
        #[arg(long, default_value_t = 1_000_000.0)]
        token_amount: f64,

        /// Validator endpoint URL
        #[arg(long, default_value = "https://localhost:8080")]
        endpoint: String,

        /// Concurrency level
        #[arg(long, default_value_t = 5)]
        concurrency: usize,

        /// Cell slug
        #[arg(long)]
        cell_slug: Option<String>,

        /// Keep benchmark data in DB after execution (default: auto-cleanup)
        #[arg(long)]
        keep: bool,
    },

    /// Generate a benchmark plan file
    Generate {
        /// Number of identities
        #[arg(long, default_value_t = 50)]
        identities: usize,

        /// Comma-separated molecule types
        #[arg(long, default_value = "meta", value_delimiter = ',')]
        types: Vec<String>,

        /// Meta mutations per identity
        #[arg(long, default_value_t = 100)]
        metas_per_identity: usize,

        /// Value transfers per identity
        #[arg(long, default_value_t = 10)]
        transfers_per_identity: usize,

        /// Rule molecules per identity
        #[arg(long, default_value_t = 5)]
        rules_per_identity: usize,

        /// Burn molecules per identity
        #[arg(long, default_value_t = 5)]
        burns_per_identity: usize,

        /// Initial token supply
        #[arg(long, default_value_t = 1_000_000.0)]
        token_amount: f64,

        /// Output plan file path
        #[arg(short, long)]
        output: String,
    },

    /// Execute an existing benchmark plan
    Execute {
        /// Path to the plan file
        plan: String,

        /// Validator endpoint URL
        #[arg(long, default_value = "https://localhost:8080")]
        endpoint: String,

        /// Concurrency level
        #[arg(long, default_value_t = 5)]
        concurrency: usize,

        /// Cell slug
        #[arg(long)]
        cell_slug: Option<String>,

        /// Keep benchmark data in DB after execution (default: auto-cleanup)
        #[arg(long)]
        keep: bool,
    },

    /// Clean up benchmark data from the database
    Clean {
        /// Specific cell slug to purge
        #[arg(long, conflicts_with = "all")]
        cell_slug: Option<String>,

        /// Purge ALL benchmark cells (BENCH_CLI_*)
        #[arg(long, conflicts_with = "cell_slug")]
        all: bool,
    },
}

// ═══════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir()?;

    // Load config: file -> env vars -> CLI flags
    let cfg = config::Config::load(&cwd).with_url_override(&cli.url);

    match cli.command {
        // ── Docker control ──────────────────────────────────
        Commands::Start { build, detach } => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::start(&compose, build, detach).await?;
        }
        Commands::Stop => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::stop(&compose).await?;
        }
        Commands::Destroy { volumes } => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::destroy(&compose, volumes).await?;
        }
        Commands::Rebuild => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::rebuild(&compose).await?;
        }
        Commands::Logs { follow, tail } => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::logs(&compose, follow, tail).await?;
        }
        Commands::Status => {
            let compose = require_compose(&cwd, &cfg)?;
            docker::status(&compose).await?;
        }

        // ── Cell management ─────────────────────────────────
        Commands::Cell { command } => match command {
            CellCommands::Create { slug, name, status } => {
                cell::create(&cfg, &slug, name.as_deref(), &status).await?;
            }
            CellCommands::List => {
                cell::list(&cfg).await?;
            }
            CellCommands::Activate { slug } => {
                cell::set_status(&cfg, &slug, "active").await?;
            }
            CellCommands::Pause { slug } => {
                cell::set_status(&cfg, &slug, "paused").await?;
            }
            CellCommands::Archive { slug } => {
                cell::set_status(&cfg, &slug, "archived").await?;
            }
        },

        // ── Benchmarks ──────────────────────────────────────
        Commands::Bench { command } => match command {
            BenchCommands::Run {
                identities,
                types,
                metas_per_identity,
                transfers_per_identity,
                rules_per_identity,
                burns_per_identity,
                token_amount,
                endpoint,
                concurrency,
                cell_slug,
                keep,
            } => {
                let gen_args = bench::generate::GenerateArgs {
                    identities,
                    types,
                    metas_per_identity,
                    transfers_per_identity,
                    rules_per_identity,
                    burns_per_identity,
                    token_amount,
                    output: String::new(), // filled by run()
                };
                let exec_args = bench::execute::ExecuteArgs {
                    plan: String::new(), // filled by run()
                    endpoint: Some(endpoint),
                    endpoints: None,
                    strategy: bench::execute::Strategy::RoundRobin,
                    concurrency,
                    cell_slug,
                    csv: None,
                    plot: None,
                    insecure_tls: cfg.validator.insecure_tls,
                };
                bench::run(gen_args, exec_args, &cfg, keep).await?;
            }
            BenchCommands::Generate {
                identities,
                types,
                metas_per_identity,
                transfers_per_identity,
                rules_per_identity,
                burns_per_identity,
                token_amount,
                output: output_path,
            } => {
                let gen_args = bench::generate::GenerateArgs {
                    identities,
                    types,
                    metas_per_identity,
                    transfers_per_identity,
                    rules_per_identity,
                    burns_per_identity,
                    token_amount,
                    output: output_path,
                };
                bench::generate(gen_args)?;
                output::success("Plan generation complete");
            }
            BenchCommands::Execute {
                plan,
                endpoint,
                concurrency,
                cell_slug,
                keep,
            } => {
                let exec_args = bench::execute::ExecuteArgs {
                    plan,
                    endpoint: Some(endpoint),
                    endpoints: None,
                    strategy: bench::execute::Strategy::RoundRobin,
                    concurrency,
                    cell_slug,
                    csv: None,
                    plot: None,
                    insecure_tls: cfg.validator.insecure_tls,
                };
                bench::execute(exec_args, &cfg, keep).await?;
            }
            BenchCommands::Clean { cell_slug, all } => {
                bench::clean(&cfg, cell_slug.as_deref(), all).await?;
            }
        },

        // ── Health checks ───────────────────────────────────
        Commands::Health => {
            health::healthz(&cfg.validator.url, cfg.validator.insecure_tls).await?;
        }
        Commands::Ready => {
            health::readyz(&cfg.validator.url, false, cfg.validator.insecure_tls).await?;
        }
        Commands::Full => {
            health::readyz(&cfg.validator.url, true, cfg.validator.insecure_tls).await?;
        }
        Commands::Db => {
            health::db_check(&cfg.validator.url, cfg.validator.insecure_tls).await?;
        }
    }

    Ok(())
}

/// Resolve the docker-compose file or exit with a helpful error.
/// Uses the compose_file name from config for discovery.
fn require_compose(cwd: &std::path::Path, cfg: &config::Config) -> Result<std::path::PathBuf> {
    paths::find_compose_file(cwd, &cfg.docker.compose_file).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not find {}.\n\
             Run this command from inside the KnishIO project tree.",
            cfg.docker.compose_file
        )
    })
}
