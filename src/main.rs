//! KnishIO CLI — unified orchestration for the KnishIO validator stack.
//!
//! ## Usage
//!
//! ```bash
//! # First-time production setup
//! knishio init --tls
//!
//! # Environment probe (no side effects)
//! knishio detect
//!
//! # Docker control (auto-detects accel profile by default)
//! knishio start --build -d
//! knishio start --accel cuda -d       # force NVIDIA path
//! knishio start --accel cpu -d        # force portable CPU path
//! knishio stop
//! knishio destroy --volumes
//! knishio rebuild
//! knishio logs --follow --tail 100
//! knishio status
//!
//! # Docker Model Runner (macOS GPU bridge)
//! knishio dmr status
//! knishio dmr enable
//! knishio dmr pull                     # fetches default Qwen GGUFs
//! knishio dmr pull --model hf.co/...   # arbitrary ref
//!
//! # Cell management
//! knishio cell create TESTCELL --name "Test Cell"
//! knishio cell list
//! knishio cell activate TESTCELL
//! knishio cell pause TESTCELL
//! knishio cell archive TESTCELL
//!
//! # Database management
//! knishio backup
//! knishio backup --output mybackup.sql
//! knishio backup list
//! knishio restore backups/knishio_20260406.sql
//! knishio psql
//! knishio psql -c "SELECT count(*) FROM molecules"
//!
//! # Updates
//! knishio update
//! knishio update --build
//! knishio update --rollback
//!
//! # Benchmarks
//! knishio bench run --types meta,value-transfer --identities 50 --concurrency 5
//! knishio bench generate --types meta -o plan.db
//! knishio bench execute plan.db --concurrency 10
//!
//! # Embedding management
//! knishio embed status
//! knishio embed reset
//! knishio embed search "user profile settings"
//! knishio embed ask "what stores sell kitchen stuff?"
//!
//! # Validator AI introspection
//! knishio ai status                    # fetch /ai/status and pretty-print
//!
//! # Health checks
//! knishio health
//! knishio ready
//! knishio full
//! knishio db
//! ```

mod ai;
mod audit;
mod backup;
mod bench;
mod cell;
mod config;
mod detect;
mod dmr;
mod docker;
mod embed;
mod health;
mod init;
mod metrics;
mod osmosis;
mod output;
mod p2p;
mod package;
mod paths;
mod update;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::env;
use std::path::PathBuf;

use crate::detect::Accel;

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
    /// Validator base URL for all HTTP + WS subcommands (health, ready,
    /// full, db, ai, metrics, watch, embed).
    #[arg(long, global = true, default_value = "https://localhost:8080")]
    url: String,

    /// Accept self-signed TLS certs from the validator. Mirrors curl's
    /// `--insecure`: the *server* keeps running HTTPS as always; this flag
    /// just tells the CLI HTTP client to skip cert verification for THIS
    /// invocation (overrides `[validator] insecure_tls = false` in
    /// knishio.toml on a per-call basis). Useful for prod-like configs where
    /// insecure_tls is normally off but you need a one-off cert-skip.
    #[arg(long, global = true)]
    insecure: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
enum AccelFlag {
    Auto,
    Cpu,
    Cuda,
    Dmr,
    MetalNative,
    Rocm,
    Vulkan,
}

#[derive(Subcommand)]
enum Commands {
    /// Print environment detection result; no side effects
    Detect,

    /// Initialize production deployment (generates secrets, config, certs)
    Init {
        /// Generate self-signed TLS certificates
        #[arg(long)]
        tls: bool,

        /// Set CORS_ORIGINS in generated config
        #[arg(long)]
        cors: Option<String>,

        /// Regenerate config files even if they already exist. Touches
        /// .env.production, knishio.toml, and certs/ (when --tls is set).
        /// Always preserves secrets/ — to regenerate secrets, run
        /// `rm -rf secrets/ && knishio init --force` explicitly.
        #[arg(long)]
        force: bool,
    },

    /// Start the validator stack (docker compose up)
    Start {
        /// Build images before starting
        #[arg(long)]
        build: bool,

        /// Run in detached mode
        #[arg(short, long)]
        detach: bool,

        /// Hardware acceleration profile. `auto` auto-detects the host
        /// (Apple Silicon → DMR or metal-native, NVIDIA → cuda, otherwise cpu).
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,

        /// Override generation model. Accepts short aliases
        /// (`gemma`, `qwen3.5`, `qwen3-0.6b`) or a full ref like
        /// `huggingface.co/unsloth/...:latest`. Takes precedence over
        /// the `GENERATION_MODEL` shell env; overridden by nothing.
        #[arg(long)]
        gen_model: Option<String>,
    },

    /// Stop the validator stack
    Stop {
        /// Hardware acceleration profile used by the running stack.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,
    },

    /// Destroy the validator stack (docker compose down)
    Destroy {
        /// Also remove volumes (all data lost!)
        #[arg(long)]
        volumes: bool,

        /// Hardware acceleration profile used by the running stack.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,
    },

    /// Rebuild the validator image and restart.
    ///
    /// Uses Docker's layer cache + the Dockerfile's BuildKit cargo cache mounts,
    /// so an unchanged-source rebuild takes ~1–2 min and a code change recompiles
    /// only the affected crates. Pass `--no-cache` for a true from-scratch build
    /// (also refreshes apt/base-image layers — use after upstream package updates).
    Rebuild {
        /// Hardware acceleration profile to target on restart.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,

        /// Override generation model on the restart. Same alias set
        /// as `start --gen-model`.
        #[arg(long)]
        gen_model: Option<String>,

        /// Bust ALL build caches (layer + image) for a clean from-scratch build.
        /// Slower (re-runs apt/rustfmt/compile); use to pick up upstream apt or
        /// base-image updates. The BuildKit cargo cache mount still persists
        /// (it survives --no-cache), so the compile stays incremental.
        #[arg(long)]
        no_cache: bool,
    },

    /// Pull/build latest version, restart, and verify health
    Update {
        /// Build from source instead of pulling images
        #[arg(long)]
        build: bool,

        /// Roll back to previous version
        #[arg(long, conflicts_with = "build")]
        rollback: bool,

        /// Hardware acceleration profile to target on restart.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,

        /// Override generation model for this update (same alias set
        /// as `start --gen-model`). Ignored when `--rollback` is set.
        #[arg(long)]
        gen_model: Option<String>,
    },

    /// Show container logs
    Logs {
        /// Follow log output
        #[arg(short, long)]
        follow: bool,

        /// Number of lines to show from end
        #[arg(long)]
        tail: Option<usize>,

        /// Hardware acceleration profile used by the running stack.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,
    },

    /// Show container status
    Status {
        /// Hardware acceleration profile used by the running stack.
        #[arg(long, value_enum, default_value_t = AccelFlag::Auto)]
        accel: AccelFlag,
    },

    /// Docker Model Runner (macOS GPU bridge) control
    Dmr {
        #[command(subcommand)]
        command: DmrCommands,
    },

    /// Validator AI-pipeline introspection (via /ai/status)
    Ai {
        #[command(subcommand)]
        command: AiCommands,
    },

    /// Cell management
    Cell {
        #[command(subcommand)]
        command: CellCommands,
    },

    /// Database backup and restore
    Backup {
        #[command(subcommand)]
        command: BackupCommands,
    },

    /// Restore database from a backup file
    Restore {
        /// Path to the backup file
        path: String,

        /// Skip post-restore verification
        #[arg(long)]
        skip_verify: bool,
    },

    /// Open an interactive psql session
    Psql {
        /// Run a single SQL command instead of interactive mode
        #[arg(short, long)]
        command: Option<String>,
    },

    /// Benchmark operations
    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },

    /// Embedding management (DataBraid VKG)
    Embed {
        #[command(subcommand)]
        command: EmbedCommands,
    },

    /// Query the validator's audit log (audit_events table)
    Audit {
        #[command(subcommand)]
        command: AuditCommands,
    },

    /// P2P observability (peer counts, top peers by reputation)
    P2p {
        #[command(subcommand)]
        command: P2pCommands,
    },

    /// Osmosis pruning observability (last cycle, totals, queue depth)
    Osmosis {
        #[command(subcommand)]
        command: OsmosisCommands,
    },

    /// Health check. Defaults to the liveness probe (GET /healthz);
    /// `--full` hits the richer /health endpoint with DB latency + cache
    /// stats + validator version.
    Health {
        /// Hit `/health` instead of `/healthz` for a fuller report.
        #[arg(long)]
        full: bool,
    },

    /// Readiness check (GET /readyz)
    Ready,

    /// Full readiness detail (GET /readyz with body)
    Full,

    /// Database consistency check (GET /db-check)
    Db,

    /// Fetch and pretty-print the validator's Prometheus scrape
    /// (GET /metrics). Groups samples by subsystem (AI, Cache, DB,
    /// HTTP/GraphQL, Molecule Processing, …); histograms collapse
    /// to count/sum/avg.
    Metrics {
        /// Case-insensitive substring filter on metric name, e.g.
        /// `--filter embedding` to show only AI-embedding counters.
        #[arg(long)]
        filter: Option<String>,

        /// Passthrough the raw Prometheus text exposition to stdout
        /// (equivalent to `curl /metrics`). Useful for piping into
        /// another parser.
        #[arg(long)]
        raw: bool,
    },

    /// Print a shell completion script. Redirect to your shell's
    /// completion directory, e.g.
    ///   knishio completions zsh > ~/.zsh/completion/_knishio
    ///   knishio completions bash > /etc/bash_completion.d/knishio
    Completions {
        /// Target shell: bash, zsh, fish, powershell, or elvish.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Live-stream a validator GraphQL subscription over WebSocket.
    /// Emits one JSON event per line on stdout (jq-friendly). Ctrl-C
    /// closes the subscription gracefully.
    Watch {
        #[command(subcommand)]
        subject: WatchSubject,
    },

    /// Build a distributable tarball of the validator binary (wraps
    /// the validator's `Makefile`). Produces versioned archives in
    /// `servers/knishio-validator-rust/dist/` with binary, deploy
    /// assets, README, and SHAKE256SUMS manifest. See the validator
    /// README "Bare-Metal Deployment" section for the install flow.
    Package {
        /// Which platforms to package. `all` builds both (default).
        #[arg(long, value_enum, default_value_t = PackageTarget::All)]
        target: PackageTarget,

        /// Remove the dist/ output directory (runs `make clean`)
        /// instead of packaging.
        #[arg(long, conflicts_with = "target")]
        clean: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PackageTarget {
    /// Both macOS arm64 and Linux arm64
    All,
    /// macOS arm64 only (native cargo build)
    Mac,
    /// Linux arm64 only (docker buildx binary-export stage)
    Linux,
}

#[derive(Subcommand)]
enum WatchSubject {
    /// Stream DataBraid embedding-pipeline events as rows get embedded
    /// (subscription `embeddingChanges`).
    Embeddings {
        /// Filter by MetaType (e.g. `KKStore`).
        #[arg(long)]
        meta_type: Option<String>,

        /// Filter by MetaId (e.g. `pet-wants-1`).
        #[arg(long)]
        meta_id: Option<String>,
    },

    /// Stream DAG structure events — molecule acceptance + bond
    /// creation (subscription `dagChanges`).
    Dag {
        /// Filter to a single cell slug.
        #[arg(long)]
        cell: Option<String>,
    },
}

#[derive(Subcommand)]
enum DmrCommands {
    /// Show client/server/TCP status and cached model list
    Status,
    /// Enable DMR's TCP endpoint on :12434 (wrapper over `docker desktop enable`)
    Enable,
    /// Pull a model into the DMR cache (defaults to the two Qwen GGUFs)
    Pull {
        /// Model ref: `hf.co/<owner>/<repo>`, `ai/<name>`, or full OCI ref
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum AiCommands {
    /// Fetch `/ai/status` from the validator and pretty-print
    Status,
}

#[derive(Subcommand)]
enum BackupCommands {
    /// Create a database backup (default)
    Create {
        /// Output file path (default: backups/knishio_TIMESTAMP.sql)
        #[arg(short, long)]
        output: Option<String>,
    },

    /// List available backups
    List,
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

        /// Initial ABAC access mode
        #[arg(long, value_enum, default_value_t = AccessMode::Open)]
        mode: AccessMode,

        /// Bundle hash to authorize (permissioned mode); repeatable
        #[arg(long, value_name = "BUNDLE")]
        authorize: Vec<String>,

        /// Bundle hash to grant admin (private mode); repeatable
        #[arg(long, value_name = "BUNDLE")]
        admin: Vec<String>,
    },

    /// List all cells
    List,

    /// Show full cell record including ABAC access state
    Show {
        /// Cell slug
        slug: String,
    },

    /// Set the cell's ABAC access mode (initializes empty bundle lists if absent)
    SetMode {
        /// Cell slug
        slug: String,

        /// New access mode
        #[arg(value_enum)]
        mode: AccessMode,
    },

    /// Authorize a bundle for a permissioned cell
    Grant {
        /// Cell slug
        slug: String,

        /// Bundle hash (64 lowercase hex characters); omit when using --from-file
        #[arg(required_unless_present = "from_file")]
        bundle: Option<String>,

        /// Read bundle hashes from file (one per line; '#' comments and blanks skipped)
        #[arg(long, value_name = "PATH", conflicts_with = "bundle")]
        from_file: Option<std::path::PathBuf>,
    },

    /// Remove a bundle from a permissioned cell's authorized list
    Revoke {
        /// Cell slug
        slug: String,

        /// Bundle hash
        bundle: String,
    },

    /// Add a bundle to a private cell's admin list
    AddAdmin {
        /// Cell slug
        slug: String,

        /// Bundle hash (64 lowercase hex characters); omit when using --from-file
        #[arg(required_unless_present = "from_file")]
        bundle: Option<String>,

        /// Read bundle hashes from file (one per line; '#' comments and blanks skipped)
        #[arg(long, value_name = "PATH", conflicts_with = "bundle")]
        from_file: Option<std::path::PathBuf>,
    },

    /// Remove a bundle from a private cell's admin list
    RemoveAdmin {
        /// Cell slug
        slug: String,

        /// Bundle hash
        bundle: String,
    },

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

#[derive(Copy, Clone, Debug, ValueEnum)]
#[value(rename_all = "lowercase")]
enum AccessMode {
    /// Any bundle may submit (default)
    Open,
    /// Only bundles in `authorized_bundles` may submit
    Permissioned,
    /// Only bundles in `admin_bundles` may submit
    Private,
}

impl AccessMode {
    fn as_str(&self) -> &'static str {
        match self {
            AccessMode::Open => "open",
            AccessMode::Permissioned => "permissioned",
            AccessMode::Private => "private",
        }
    }
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

#[derive(Subcommand)]
enum EmbedCommands {
    /// Show embedding coverage and model statistics
    Status,

    /// Clear embeddings so the automatic backfill re-embeds them
    Reset {
        /// Clear only embeddings from this specific model name
        #[arg(long, conflicts_with = "all")]
        model: Option<String>,

        /// Clear ALL embeddings (nuclear option)
        #[arg(long, conflicts_with = "model")]
        all: bool,

        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Run semantic search from the terminal
    Search {
        /// Natural language search query
        query: String,

        /// Maximum number of results
        #[arg(long, default_value_t = 10)]
        limit: i32,

        /// Minimum cosine similarity threshold (0.0 to 1.0)
        #[arg(long, default_value_t = 0.7)]
        threshold: f64,

        /// Filter by meta_type
        #[arg(long)]
        meta_type: Option<String>,
    },

    /// Ask a natural language question about DAG data (RAG)
    Ask {
        /// Question to ask
        question: String,

        /// Maximum source records to consider
        #[arg(long, default_value_t = 20)]
        max_results: i32,

        /// Minimum cosine similarity threshold (0.0 to 1.0)
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,

        /// Filter by meta_type
        #[arg(long)]
        meta_type: Option<String>,
    },
}

#[derive(Subcommand)]
enum P2pCommands {
    /// Show peer counts (active/suspended/banned/stale), top peers by
    /// reputation, and the configured bootstrap list. Works against
    /// any validator; returns "P2P disabled" when P2P_ENABLED=false.
    Status,
}

#[derive(Subcommand)]
enum OsmosisCommands {
    /// Show last-cycle pruning counts, lifetime totals, queue depth
    /// estimate, and dry-run flag. Always available; reports "disabled"
    /// when OSMOSIS_ENABLED=false at validator startup.
    Status,
}

#[derive(Subcommand)]
enum AuditCommands {
    /// List recent audit events (newest first). Filters compose; all
    /// optional. Without filters, prints the most recent N events
    /// (--limit, default 50).
    List {
        /// Match exact action (e.g. `cell.grant`, `molecule.reject`)
        #[arg(long)]
        action: Option<String>,

        /// Match exact category (e.g. `auth`, `validation`, `lifecycle`)
        #[arg(long)]
        category: Option<String>,

        /// Match actor bundle hash (64-char lowercase hex)
        #[arg(long)]
        bundle: Option<String>,

        /// Match cell slug
        #[arg(long)]
        cell: Option<String>,

        /// Match severity (`info`, `warn`, `critical`)
        #[arg(long)]
        severity: Option<String>,

        /// Filter to events newer than this. Accepts duration suffixes
        /// (`30s`/`15m`/`2h`/`7d`) or bare Unix epoch seconds.
        #[arg(long)]
        since: Option<String>,

        /// Maximum rows to return (1-500, default 50)
        #[arg(long, default_value_t = 50)]
        limit: u32,
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
    let mut cfg = config::Config::load(&cwd).with_url_override(&cli.url);

    // `--insecure` is a per-invocation override of `[validator] insecure_tls`.
    // Mirrors curl's `-k`: server keeps running HTTPS as always; this just
    // tells the CLI HTTP client to skip cert verification for THIS run.
    if cli.insecure {
        cfg.validator.insecure_tls = true;
    }

    match cli.command {
        // ── Detect (no docker dispatch) ─────────────────────
        Commands::Detect => {
            let env = detect::detect();
            detect::print_summary(&env);
        }

        // ── Init ────────────────────────────────────────────
        Commands::Init { tls, cors, force } => {
            init::run(&cwd, tls, cors.as_deref(), force).await?;
        }

        // ── Docker control (all accel-aware) ────────────────
        Commands::Start {
            build,
            detach,
            accel,
            gen_model,
        } => {
            let (accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            warn_feature_gated_accel_without_rebuild(accel, build);
            let resolved = gen_model.as_deref().map(docker::resolve_gen_model);
            let env: Vec<(&str, &str)> = match &resolved {
                Some(m) => vec![("GENERATION_MODEL", m.as_str())],
                None => vec![],
            };
            docker::start(&files, build, detach, &env).await?;
            if cfg.accel_is_native(accel) {
                docker::print_metal_native_hint(&cwd, &cfg);
            }
        }
        Commands::Stop { accel } => {
            let (_accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            docker::stop(&files).await?;
        }
        Commands::Destroy { volumes, accel } => {
            let (_accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            docker::destroy(&files, volumes).await?;
        }
        Commands::Rebuild { accel, gen_model, no_cache } => {
            let (_accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            let resolved = gen_model.as_deref().map(docker::resolve_gen_model);
            let env: Vec<(&str, &str)> = match &resolved {
                Some(m) => vec![("GENERATION_MODEL", m.as_str())],
                None => vec![],
            };
            docker::rebuild(&files, &env, no_cache).await?;
        }
        Commands::Update {
            build,
            rollback,
            accel,
            gen_model,
        } => {
            // update module still takes a single compose file; reuse the first
            // (base) file in the resolved chain so back-compat is preserved.
            let (accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            if !rollback {
                warn_feature_gated_accel_without_rebuild(accel, build);
            }
            let base = files
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("resolved accel chain is empty"))?;
            if rollback {
                update::rollback(&base, &cfg).await?;
            } else {
                let resolved = gen_model.as_deref().map(docker::resolve_gen_model);
                let env: Vec<(&str, &str)> = match &resolved {
                    Some(m) => vec![("GENERATION_MODEL", m.as_str())],
                    None => vec![],
                };
                update::update(&base, &cfg, build, &env).await?;
            }
        }
        Commands::Logs { follow, tail, accel } => {
            let (_accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            docker::logs(&files, follow, tail).await?;
        }
        Commands::Status { accel } => {
            let (_accel, files) = resolve_accel_and_files(&cwd, &cfg, accel)?;
            docker::status(&files).await?;
        }

        // ── Validator AI introspection ──────────────────────
        Commands::Ai { command } => match command {
            AiCommands::Status => ai::status(&cfg).await?,
        },

        // ── Docker Model Runner ─────────────────────────────
        Commands::Dmr { command } => match command {
            DmrCommands::Status => dmr::status().await?,
            DmrCommands::Enable => dmr::enable().await?,
            DmrCommands::Pull { model } => dmr::pull(model).await?,
        },

        // ── Cell management ─────────────────────────────────
        Commands::Cell { command } => match command {
            CellCommands::Create { slug, name, status, mode, authorize, admin } => {
                cell::create(&cfg, &slug, name.as_deref(), &status).await?;
                // Seed access state if any non-default flag was supplied.
                if mode.as_str() != "open" || !authorize.is_empty() || !admin.is_empty() {
                    cell::set_mode(&cfg, &slug, mode.as_str()).await?;
                    for bundle in &authorize {
                        cell::grant(&cfg, &slug, bundle).await?;
                    }
                    for bundle in &admin {
                        cell::add_admin(&cfg, &slug, bundle).await?;
                    }
                }
            }
            CellCommands::List => {
                cell::list(&cfg).await?;
            }
            CellCommands::Show { slug } => {
                cell::show(&cfg, &slug).await?;
            }
            CellCommands::SetMode { slug, mode } => {
                cell::set_mode(&cfg, &slug, mode.as_str()).await?;
            }
            CellCommands::Grant { slug, bundle, from_file } => {
                match (bundle, from_file) {
                    (Some(b), None) => cell::grant(&cfg, &slug, &b).await?,
                    (None, Some(p)) => cell::grant_from_file(&cfg, &slug, &p).await?,
                    (None, None) => unreachable!("clap required_unless_present"),
                    (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                }
            }
            CellCommands::Revoke { slug, bundle } => {
                cell::revoke(&cfg, &slug, &bundle).await?;
            }
            CellCommands::AddAdmin { slug, bundle, from_file } => {
                match (bundle, from_file) {
                    (Some(b), None) => cell::add_admin(&cfg, &slug, &b).await?,
                    (None, Some(p)) => cell::add_admin_from_file(&cfg, &slug, &p).await?,
                    (None, None) => unreachable!("clap required_unless_present"),
                    (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                }
            }
            CellCommands::RemoveAdmin { slug, bundle } => {
                cell::remove_admin(&cfg, &slug, &bundle).await?;
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

        // ── Backup / Restore ────────────────────────────────
        Commands::Backup { command } => match command {
            BackupCommands::Create { output } => {
                backup::backup(&cfg, output.as_deref()).await?;
            }
            BackupCommands::List => {
                backup::list().await?;
            }
        },
        Commands::Restore { path, skip_verify } => {
            backup::restore(&cfg, &path, skip_verify).await?;
        }

        // ── Psql ────────────────────────────────────────────
        Commands::Psql { command } => {
            docker::psql(&cfg, command.as_deref()).await?;
        }

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

        // ── Embedding management ────────────────────────────
        Commands::Embed { command } => match command {
            EmbedCommands::Status => {
                embed::status(&cfg).await?;
            }
            EmbedCommands::Reset { model, all, yes } => {
                embed::reset(&cfg, model.as_deref(), all, yes).await?;
            }
            EmbedCommands::Search {
                query,
                limit,
                threshold,
                meta_type,
            } => {
                embed::search(&cfg, &query, limit, threshold, meta_type.as_deref()).await?;
            }
            EmbedCommands::Ask {
                question,
                max_results,
                threshold,
                meta_type,
            } => {
                embed::ask(&cfg, &question, max_results, threshold, meta_type.as_deref()).await?;
            }
        },

        // ── P2P observability ───────────────────────────────
        Commands::P2p { command } => match command {
            P2pCommands::Status => p2p::status(&cfg).await?,
        },

        // ── Osmosis pruning observability ───────────────────
        Commands::Osmosis { command } => match command {
            OsmosisCommands::Status => osmosis::status(&cfg).await?,
        },

        // ── Audit log queries ───────────────────────────────
        Commands::Audit { command } => match command {
            AuditCommands::List {
                action,
                category,
                bundle,
                cell,
                severity,
                since,
                limit,
            } => {
                audit::list(
                    &cfg,
                    audit::ListFilters {
                        action,
                        category,
                        bundle,
                        cell,
                        severity,
                        since,
                        limit,
                    },
                )
                .await?;
            }
        },

        // ── Health checks ───────────────────────────────────
        Commands::Health { full } => {
            if full {
                health::health_full(&cfg.validator.url, cfg.validator.insecure_tls).await?;
            } else {
                health::healthz(&cfg.validator.url, cfg.validator.insecure_tls).await?;
            }
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
        Commands::Metrics { filter, raw } => {
            metrics::metrics(&cfg, filter.as_deref(), raw).await?;
        }
        Commands::Completions { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "knishio", &mut std::io::stdout());
        }
        Commands::Watch { subject } => match subject {
            WatchSubject::Embeddings { meta_type, meta_id } => {
                watch::embeddings(&cfg, meta_type, meta_id).await?;
            }
            WatchSubject::Dag { cell } => {
                watch::dag(&cfg, cell).await?;
            }
        },
        Commands::Package { target, clean } => {
            if clean {
                package::clean(&cwd).await?;
            } else {
                match target {
                    PackageTarget::All => package::package_all(&cwd).await?,
                    PackageTarget::Mac => package::package_mac(&cwd).await?,
                    PackageTarget::Linux => package::package_linux(&cwd).await?,
                }
            }
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Accel resolution
// ═══════════════════════════════════════════════════════════════

/// Turn a CLI `--accel` flag + config + host detection into a concrete
/// (Accel, compose file list) pair. Prints the Environment header and the
/// "Stack" line every time, so the operator always sees what's happening.
///
/// Precedence:
///   1. Explicit `--accel` flag (not Auto)
///   2. `knishio.toml` `default_accel` or env `KNISHIO_ACCEL` (plumbed via cfg.default_accel)
///   3. Auto-detection via `detect::detect()`
fn resolve_accel_and_files(
    cwd: &std::path::Path,
    cfg: &config::Config,
    cli_accel: AccelFlag,
) -> Result<(Accel, Vec<PathBuf>)> {
    let (accel, source) = match cli_accel {
        AccelFlag::Auto => {
            // R24 A: prefer detection of the currently-running validator
            // stack via its compose-project labels. Fixes the destroy/restart
            // UX bug where hardware-based detection picked metal-native after
            // a `start --accel dmr`. Only fires when something is running;
            // falls through to config + hardware detection on a fresh init.
            if let Some(running) = docker::detect_running_stack(cwd) {
                (running, AccelSource::RunningStack)
            } else {
                match cfg.default_accel.as_deref() {
                    Some(name) => {
                        let a = parse_accel_name(name).ok_or_else(|| {
                            anyhow::anyhow!(
                                "config `default_accel = \"{}\"` is not a known accel name",
                                name
                            )
                        })?;
                        (a, AccelSource::Config)
                    }
                    None => {
                        let env = detect::detect();
                        detect::print_summary(&env);
                        (env.accel, AccelSource::Auto)
                    }
                }
            }
        }
        explicit => (flag_to_accel(explicit), AccelSource::Explicit),
    };

    // Print the override / config origin line when we didn't already print a
    // full Environment block above.
    let arrow = colored::Colorize::bold(colored::Colorize::green("→"));
    match source {
        AccelSource::Auto => {
            // print_summary already printed the Accel line; just add Stack below.
        }
        AccelSource::Explicit => {
            output::header("Environment");
            println!(
                "{} Accel:   {}  (explicit --accel; detection skipped)",
                arrow,
                accel
            );
        }
        AccelSource::Config => {
            output::header("Environment");
            println!(
                "{} Accel:   {}  (config default_accel; detection skipped)",
                arrow,
                accel
            );
        }
        AccelSource::RunningStack => {
            output::header("Environment");
            println!(
                "{} Accel:   {}  (detected from running validator container)",
                arrow,
                accel
            );
        }
    }

    // Resolve overlay chain, with CPU fallback when the chosen profile's
    // files aren't present on disk.
    let configured = cfg.accel_files(accel).to_vec();
    if configured.is_empty() {
        output::warn(&format!(
            "accel `{}` has no configured compose files; falling back to cpu",
            accel
        ));
        let cpu_files = cfg.accel_files(Accel::Cpu).to_vec();
        let resolved = paths::find_compose_files(cwd, &cpu_files)?;
        print_stack_line(&cpu_files);
        return Ok((Accel::Cpu, resolved));
    }

    match paths::find_compose_files(cwd, &configured) {
        Ok(resolved) => {
            print_stack_line(&configured);
            if accel == Accel::Dmr {
                println!(
                    "{} {:8} validator → model-runner.docker.internal:12434/engines/llama.cpp/v1",
                    arrow, "Routing:"
                );
            }
            Ok((accel, resolved))
        }
        Err(e) => {
            output::warn(&format!("{}", e));
            output::warn(&format!(
                "falling back from `{}` to `cpu`",
                accel
            ));
            let cpu_files = cfg.accel_files(Accel::Cpu).to_vec();
            let resolved = paths::find_compose_files(cwd, &cpu_files)?;
            print_stack_line(&cpu_files);
            Ok((Accel::Cpu, resolved))
        }
    }
}

/// Warn when the operator selected a feature-gated GPU accel on a
/// command that uses the EXISTING validator image (no rebuild). The
/// `cuda.yml` / `rocm.yml` / `vulkan.yml` overlays set `CARGO_FEATURES`
/// at build time only; on a default CPU-only image, the --accel flag
/// then becomes a silent no-op and inference quietly falls back to
/// CPU. This helper nudges the operator toward `knishio rebuild
/// --accel <X>` first.
///
/// Deliberately excludes:
///   - `Cpu`, `Dmr` — neither needs a feature-gated build.
///   - `MetalNative` — that path prints its own bespoke hint via
///     `docker::print_metal_native_hint` (which spells out the cargo
///     rebuild command), so a second warning would be noise.
fn warn_feature_gated_accel_without_rebuild(accel: Accel, will_rebuild: bool) {
    if will_rebuild {
        return;
    }
    let feature = match accel {
        Accel::Cuda => "cuda",
        Accel::Rocm => "rocm",
        Accel::Vulkan => "vulkan",
        _ => return,
    };
    output::warn(&format!(
        "Accel '{}' requires a validator image built with --features {}. \
         If this is a fresh setup, run `knishio rebuild --accel {}` first \
         (or pass `--build` to this command); otherwise the --accel flag \
         is a no-op on the existing image and inference stays on CPU.",
        feature, feature, feature
    ));
}

fn print_stack_line(files: &[String]) {
    let arrow = colored::Colorize::bold(colored::Colorize::green("→"));
    println!(
        "{} {:8} {}",
        arrow,
        "Stack:",
        files.join(" + ")
    );
}

enum AccelSource {
    Auto,
    Explicit,
    Config,
    /// R24 A: running validator container's compose-project label was the source.
    RunningStack,
}

fn flag_to_accel(f: AccelFlag) -> Accel {
    match f {
        AccelFlag::Auto => unreachable!("Auto handled by caller"),
        AccelFlag::Cpu => Accel::Cpu,
        AccelFlag::Cuda => Accel::Cuda,
        AccelFlag::Dmr => Accel::Dmr,
        AccelFlag::MetalNative => Accel::MetalNative,
        AccelFlag::Rocm => Accel::Rocm,
        AccelFlag::Vulkan => Accel::Vulkan,
    }
}

fn parse_accel_name(name: &str) -> Option<Accel> {
    match name {
        "cpu" => Some(Accel::Cpu),
        "cuda" => Some(Accel::Cuda),
        "dmr" => Some(Accel::Dmr),
        "metal-native" => Some(Accel::MetalNative),
        "rocm" => Some(Accel::Rocm),
        "vulkan" => Some(Accel::Vulkan),
        _ => None,
    }
}
