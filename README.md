# knishio

[![Crates.io](https://img.shields.io/crates/v/knishio-cli.svg)](https://crates.io/crates/knishio-cli)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

Unified CLI for orchestrating the KnishIO validator stack — production deployment, Docker control, cell management, database management, benchmarks, embeddings, and health checks.

## Quick Start

```bash
# Install the CLI
cargo install knishio-cli

# See what the CLI detected about your host (zero side effects)
knishio detect

# Start the validator stack — accel profile is auto-detected
# (NVIDIA host → cuda, Apple Silicon + DMR → dmr, otherwise → cpu)
knishio start -d --build

# Create a cell and check health
knishio cell create TESTCELL --name "Test Cell"
knishio health

# Run a benchmark
knishio bench run --types meta --identities 50 --cell-slug TESTCELL

# Tear it all down
knishio destroy --volumes
```

Every docker-touching command prints an **Environment** block so you always see exactly what was detected and which compose stack is running. Override auto-detection with `--accel <name>` — see [Hardware Acceleration](#hardware-acceleration).

**Pipe-friendly output**: meta-messages (status banners, progress, warnings) go to stderr; actual command data goes to stdout. Everything chains cleanly:

```bash
knishio metrics --raw | grep embedding_backfill
knishio ai status > /tmp/snapshot.txt           # only the body
knishio watch embeddings | jq -r '.metaId'      # only the events
```

### Production Quick Start

```bash
# One-time setup: generates secrets, config, TLS certs
knishio init --tls --cors "https://your-app.example.com"

# Launch the production stack
knishio start --build -d

# Seed your cell
knishio cell create MYCELL --name "My Application"

# Verify everything
knishio full
```

## Installation

### From crates.io (recommended)

```bash
cargo install knishio-cli
```

This installs the `knishio` binary into `~/.cargo/bin/`.

### From source

Requires Rust 1.70+.

```bash
git clone https://github.com/WishKnish/KnishIO-CLI.git
cd KnishIO-CLI
cargo build --release
```

The binary is at `target/release/knishio`. Optionally copy it onto your PATH:

```bash
cp target/release/knishio /usr/local/bin/
```

## Prerequisites

- **Docker** with the `compose` plugin (v2)
- **Running validator stack** for cell, health, backup, and embed commands
- **openssl** for TLS certificate generation (`knishio init --tls`)

## Configuration

The CLI uses a layered configuration system. Values are resolved in this order (highest priority wins):

1. **CLI flags** (`--url`, etc.)
2. **Environment variables** (`KNISHIO_URL`, etc.)
3. **Config file** (`knishio.toml`, auto-discovered)
4. **Built-in defaults**

### Config File

Place a `knishio.toml` anywhere in the project tree — the CLI walks up from your current directory to find it.

```toml
# Optional: pin an accel and skip auto-detection every invocation
# default_accel = "cpu"

[validator]
url = "https://localhost:8080"
insecure_tls = false  # set to true for self-signed certs

[docker]
compose_file = "docker-compose.standalone.yml"
postgres_container = "knishio-postgres"
validator_container = "knishio-validator"

# Per-accel compose file chains (all of these have baked defaults — override here only)
# [docker.accel.cuda]
# files = ["docker-compose.standalone.yml", "docker-compose.cuda.yml"]
#
# [docker.accel.dmr]
# files = ["docker-compose.standalone.yml", "docker-compose.dmr.yml"]
#
# [docker.accel.metal-native]
# files = ["docker-compose.metal.yml"]
# native_validator = true   # emits the cargo-run hint after `start`

[database]
user = "knishio"
name = "knishio"
```

For production, `knishio init` generates a `knishio.toml` that points to `docker-compose.production.yml` instead.

### Environment Variables

| Variable | Config Field | Default |
|----------|-------------|---------|
| `KNISHIO_URL` | `validator.url` | `https://localhost:8080` |
| `KNISHIO_PG_CONTAINER` | `docker.postgres_container` | `knishio-postgres` |
| `KNISHIO_VALIDATOR_CONTAINER` | `docker.validator_container` | `knishio-validator` |
| `KNISHIO_DB_USER` | `database.user` | `knishio` |
| `KNISHIO_DB_NAME` | `database.name` | `knishio` |
| `KNISHIO_INSECURE_TLS` | `validator.insecure_tls` | `false` |
| `KNISHIO_ACCEL` | `default_accel` | *(unset → auto-detect)* |

### Global CLI Flags

```
--url <URL>       Validator base URL for health commands [default: https://localhost:8080]
--accel <ACCEL>   Hardware acceleration profile
                  [default: auto]
                  [possible values: auto, cpu, cuda, dmr, metal-native, rocm, vulkan]
-h, --help        Print help
-V, --version     Print version
```

`--url` applies to `health`, `ready`, `full`, and `db`. TLS certificates are validated by default; set `insecure_tls = true` in `knishio.toml` or `KNISHIO_INSECURE_TLS=true` for self-signed local certs. Health requests have a 30-second timeout.

`--accel auto` (default) auto-detects the host; any other value forces a specific stack and skips detection. See [Hardware Acceleration](#hardware-acceleration) for the full decision table.

## Hardware Acceleration

The CLI auto-detects the host and picks the matching compose stack + env vars so GPU-accelerated inference works without typing the right `-f a.yml -f b.yml` incantations yourself. Every `knishio start / rebuild / stop / status / …` prints the resolved accel, the compose stack being used, and (for DMR) the host-side routing URL — so the active optimization is never a guess.

### Decision Table

| Host signal | → Accel | Compose stack | Validator runs where |
|---|---|---|---|
| macOS + Apple Silicon + DMR TCP reachable on `:12434` | **dmr** | `standalone.yml` + `dmr.yml` | containerised; inference to host DMR |
| macOS + Apple Silicon (DMR missing) | **metal-native** | `metal.yml` *(Postgres only)* + cargo-run hint | host (native `--features metal`) |
| Linux with `nvidia-smi` present | **cuda** | `standalone.yml` + `cuda.yml` | containerised (GPU passthrough via nvidia-container-toolkit) |
| Linux with `rocminfo` present | **rocm** | `standalone.yml` + `rocm.yml` *(overlay TBD)* | containerised |
| everything else | **cpu** | `standalone.yml` | containerised, CPU-only |

If the chosen accel's overlay file isn't present on disk, the CLI falls back to `cpu` with a warning line — you're never blocked because an overlay didn't ship.

### detect

Probe the host and print the resolved accel — no side effects.

```bash
knishio detect
```

Example output on an M4 Mac with DMR enabled:

```
Environment
ℹ Host:    macos (aarch64)
ℹ CPU:     Apple M4 · 32 GB RAM
ℹ GPU:     Apple M4 (Apple)
ℹ Docker:  29.3.1
ℹ DMR:     running, TCP :12434 reachable, 2 cached model(s)
→ Accel:   dmr  (Apple Silicon + DMR TCP reachable)
```

### Forcing a profile

Override auto-detection with `--accel <name>`. The flag is global — works on every docker-touching subcommand.

```bash
knishio start --accel cpu -d          # portable CPU, regardless of host
knishio start --accel cuda --build -d # force NVIDIA path
knishio start --accel dmr -d          # force DMR bridge on Mac
knishio status --accel metal-native   # see what the native-Metal stack would look like
```

For CI determinism, pin a profile in `knishio.toml` instead:

```toml
default_accel = "cpu"
```

Or via env var: `KNISHIO_ACCEL=cpu`.

### Apple Silicon via Docker Model Runner

On macOS, Linux containers cannot access the Metal GPU directly. **Docker Model Runner (DMR)** sidesteps this by running llama.cpp with Metal *on the host* and exposing an OpenAI-compatible API at `model-runner.docker.internal:12434`. The validator stays containerised (plain Linux CPU build) and its `openai-compatible` provider points at the host endpoint over TCP — one ~1ms hop per inference, full Metal acceleration in practice.

One-time setup:

```bash
# 1. Enable DMR's TCP endpoint (Docker Desktop 4.62+ required)
docker desktop enable model-runner --tcp=12434
# ...or via the CLI:
knishio dmr enable

# 2. Pull models (defaults to the two Qwen GGUFs our compose overlay expects)
knishio dmr pull

# 3. Verify
knishio dmr status
```

After that, `knishio start -d` auto-routes through DMR with no extra flags.

If you skip DMR, `knishio start` on an Apple Silicon Mac falls back to the **metal-native** profile: Postgres runs in Docker, and the CLI prints a copy-pasteable `cargo run --release --features metal` block for running the validator binary natively.

### dmr

Docker Model Runner control surface.

```bash
knishio dmr status
knishio dmr enable
knishio dmr pull [--model <REF>]
```

| Subcommand | Description |
|---|---|
| `status` | Print DMR client/server state, TCP reachability, cached model list |
| `enable` | Enable DMR's TCP endpoint on `:12434` (wrapper over `docker desktop enable model-runner --tcp=12434`). Docker Desktop itself is toggled via its GUI (Settings → Beta/AI) — only the TCP exposure is CLI-controllable |
| `pull [--model <REF>]` | Pull a model into the DMR cache. Without `--model`, pulls the two defaults used by `docker-compose.dmr.yml`: `hf.co/Qwen/Qwen3-Embedding-4B-GGUF` and `hf.co/Qwen/Qwen3.5-0.8B-GGUF` |

```bash
# Pull a specific model
knishio dmr pull --model hf.co/Qwen/Qwen3-Embedding-8B-GGUF

# Check what's cached and whether TCP is live
knishio dmr status
```

## Production Deployment

### init

Initialize a production deployment. Generates secrets, configuration, and optionally TLS certificates.

```bash
knishio init [--tls] [--cors <ORIGINS>]
```

| Flag | Description |
|------|-------------|
| `--tls` | Generate self-signed TLS certificates (valid 365 days) |
| `--cors <ORIGINS>` | Set CORS_ORIGINS in the generated `.env.production` |

What it creates:

| File/Directory | Contents |
|----------------|----------|
| `secrets/jwt_secret` | Random 64-character hex string |
| `secrets/db_password` | Random 32-character alphanumeric password |
| `secrets/db_url` | Full Postgres connection string with generated password |
| `knishio.toml` | CLI config pointing to `docker-compose.production.yml` |
| `.env.production` | Environment config (CORS origins, feature flags) |
| `certs/` | Self-signed TLS certificate and key (if `--tls`) |
| `backups/` | Empty directory for database backups |
| `models/` | Empty directory for GGUF model files |

All secret files are created with `600` permissions and the `secrets/` directory with `700`.

```bash
# Full production init
knishio init --tls --cors "https://myapp.example.com"

# Without TLS (bring your own certs)
knishio init --cors "https://myapp.example.com"
```

Running `init` again is safe — it skips files that already exist.

### Production vs Standalone

The production compose (`docker-compose.production.yml`) differs from standalone in:

- Secrets injected via Docker `_FILE` convention (not environment variables)
- `KNISHIO_ENV=production` (enforces strong JWT secret)
- Rate limiting and rule enforcement enabled
- JSON structured logging
- Resource limits on containers (memory + CPU)
- Log rotation (50MB max, 5 files)
- `restart: always`

## Docker Control

All Docker commands locate the compose file automatically by walking up from your current directory (see [Path Discovery](#path-discovery)). When using `docker-compose.production.yml`, the CLI automatically loads `.env.production` as the env file.

### start

Start the validator stack (Postgres + validator).

```bash
knishio start [--build] [-d, --detach] [--accel <profile>] [--gen-model <name>]
```

| Flag | Description |
|------|-------------|
| `--build` | Build images before starting |
| `-d, --detach` | Run in detached mode (background) |
| `--accel <profile>` | Override auto-detected accel (`auto`/`cpu`/`cuda`/`dmr`/`metal-native`/`rocm`/`vulkan`) |
| `--gen-model <name>` | Override generation model for this run. Short aliases: `gemma`, `qwen3.5`, `qwen3-0.6b`. Full refs (`huggingface.co/...`) pass through. |

```bash
# Interactive foreground
knishio start

# Detached with rebuild
knishio start -d --build

# A/B a different generation model for this stack
knishio start -d --gen-model qwen3.5
```

The `--gen-model` flag sets `GENERATION_MODEL` for the subprocess environment; `docker-compose.dmr.yml` expands `${GENERATION_MODEL:-…}` against it.

### stop

Stop all containers without removing them.

```bash
knishio stop
```

### destroy

Remove containers and networks. Optionally remove volumes (all data).

```bash
knishio destroy [--volumes]
```

| Flag | Description |
|------|-------------|
| `--volumes` | Also remove volumes — **all data will be lost** |

### rebuild

Full no-cache rebuild of the validator image, then restart in detached mode.

```bash
knishio rebuild [--accel <profile>] [--gen-model <name>]
```

Same `--accel` + `--gen-model` flags as `start`.

Equivalent to:
```bash
docker compose build --no-cache
docker compose up -d
```

### update

Pull or build the latest version, restart the stack, and verify health before declaring success.

```bash
knishio update [--build] [--rollback]
```

| Flag | Description |
|------|-------------|
| `--build` | Rebuild from source instead of pulling images |
| `--rollback` | Revert to the previous image version |

The update process:

1. Pulls latest images (or rebuilds from source with `--build`)
2. Restarts only changed services (Postgres keeps running)
3. Polls `/readyz` until it returns 200 (up to 120-second timeout)
4. Reports before/after version numbers from `/health`
5. If health check fails: prints recent logs and suggests next steps

```bash
# Pull latest image and restart
knishio update

# Rebuild from source
knishio update --build

# Roll back after a failed update
knishio update --rollback
```

### logs

Show container logs.

```bash
knishio logs [-f, --follow] [--tail <N>]
```

| Flag | Description |
|------|-------------|
| `-f, --follow` | Follow log output in real time |
| `--tail <N>` | Show only the last N lines |

```bash
# Follow logs, last 100 lines
knishio logs -f --tail 100
```

### status

Show running container status (equivalent to `docker compose ps`).

```bash
knishio status
```

## Cell Management

Manage cells (application-specific sub-ledgers) in the validator's database. Commands execute SQL via `docker exec` into the `knishio-postgres` container.

### cell create

Create a new cell or update an existing one.

```bash
knishio cell create <SLUG> [--name <NAME>] [--status <STATUS>]
```

| Argument/Flag | Description | Default |
|---------------|-------------|---------|
| `<SLUG>` | Cell slug identifier (required) | — |
| `--name` | Human-readable display name | Same as slug |
| `--status` | Initial status | `active` |

```bash
knishio cell create TESTCELL --name "Test Cell"
knishio cell create PROD --name "Production" --status active
```

If the slug already exists, the cell's name and status are updated (upsert).

#### Validation Rules

| Field | Constraints |
|-------|-------------|
| Slug | 1-64 characters, alphanumeric + dashes + underscores only (`[a-zA-Z0-9_-]`) |
| Name | 1-256 characters, no null bytes or control characters |
| Status | Must be one of: `active`, `paused`, `archived` |

Invalid input is rejected before any database operation runs.

### cell list

List all cells with their status and creation time.

```bash
knishio cell list
```

Output:
```
Cells
SLUG                 NAME                           STATUS       CREATED
--------------------------------------------------------------------------------
public               Public Cell                    active       1773423688
TESTCELL             Test Cell                      active       1773423694
```

### cell activate / pause / archive

Change a cell's status.

```bash
knishio cell activate <SLUG>
knishio cell pause <SLUG>
knishio cell archive <SLUG>
```

```bash
# Pause a cell (molecules targeting it will be rejected)
knishio cell pause TESTCELL

# Reactivate it
knishio cell activate TESTCELL

# Archive (soft-delete)
knishio cell archive OLD_CELL
```

## Database Management

### backup create

Create a database backup using `pg_dump` via the postgres container.

```bash
knishio backup create [-o, --output <PATH>]
```

| Flag | Description | Default |
|------|-------------|---------|
| `-o, --output` | Output file path | `backups/knishio_YYYYMMDD_HHMMSS.sql` |

```bash
# Default timestamped backup
knishio backup create

# Custom output path
knishio backup create -o /mnt/backups/pre-upgrade.sql
```

Output includes file size:
```
ℹ Backing up database to backups/knishio_20260406_174028.sql...
✓ Backup complete: backups/knishio_20260406_174028.sql (0.1 MB)
```

### backup list

List available backups in the `backups/` directory, sorted newest-first.

```bash
knishio backup list
```

Output:
```
ℹ Found 3 backup(s):
  backups/knishio_20260406_174028.sql (89 KB)
  backups/knishio_20260405_120000.sql (85 KB)
  backups/knishio_20260404_090000.sql (82 KB)
```

### restore

Restore the database from a backup file. Drops and recreates the database, then verifies consistency via `/db-check`.

```bash
knishio restore <PATH> [--skip-verify]
```

| Argument/Flag | Description |
|---------------|-------------|
| `<PATH>` | Path to the backup SQL file (required) |
| `--skip-verify` | Skip the post-restore `/db-check` verification |

```bash
# Restore with automatic verification
knishio restore backups/knishio_20260406_174028.sql

# Restore without verification (faster, for development)
knishio restore backups/pre-upgrade.sql --skip-verify
```

The restore process:

1. Terminates existing database connections
2. Drops and recreates the database
3. Pipes the SQL backup into `psql`
4. Runs `/db-check` to verify migrations and schema integrity

### psql

Open an interactive `psql` session or run a single SQL command against the validator's database.

```bash
knishio psql [-c, --command <SQL>]
```

| Flag | Description |
|------|-------------|
| `-c, --command` | Run a single SQL command instead of interactive mode |

```bash
# Interactive session
knishio psql

# Single query
knishio psql -c "SELECT count(*) FROM molecules"

# Check table sizes
knishio psql -c "SELECT relname, pg_size_pretty(pg_total_relation_size(oid)) FROM pg_class WHERE relkind='r' ORDER BY pg_total_relation_size(oid) DESC LIMIT 10"
```

## Benchmarks

Benchmark commands generate ContinuID-compliant pre-signed molecules and submit them to the validator. Plans are stored as SQLite files for reproducibility.

### bench run

Generate a benchmark plan and execute it in one shot. The temporary plan file is cleaned up automatically (unless `--keep` is set).

```bash
knishio bench run [OPTIONS]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--identities` | int | 50 | Number of test identities |
| `--types` | CSV | `meta` | Molecule types: `meta`, `value-transfer`, `rule`, `burn` |
| `--metas-per-identity` | int | 100 | Meta mutations per identity |
| `--transfers-per-identity` | int | 10 | Value transfers per identity |
| `--rules-per-identity` | int | 5 | Rule molecules per identity |
| `--burns-per-identity` | int | 5 | Burn molecules per identity |
| `--token-amount` | float | 1000000.0 | Initial token supply for value transfers |
| `--endpoint` | URL | `https://localhost:8080` | Validator GraphQL endpoint |
| `--concurrency` | int | 5 | Concurrent molecule submissions |
| `--cell-slug` | string | *(none)* | Target cell slug |
| `--keep` | flag | false | Retain benchmark data in DB after execution |

```bash
# Quick meta-only benchmark
knishio bench run --types meta --identities 20 --cell-slug TESTCELL

# Mixed isotope benchmark
knishio bench run --types meta,value-transfer,rule --identities 50 --concurrency 10 --cell-slug TESTCELL

# High-throughput stress test (keep data for inspection)
knishio bench run --types meta --identities 100 --metas-per-identity 200 --concurrency 20 --keep
```

### bench generate

Generate a pre-signed benchmark plan file (SQLite) without executing it. Useful for reproducible benchmarks.

```bash
knishio bench generate [OPTIONS] -o <PATH>
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `-o, --output` | path | *(required)* | Output SQLite plan file |
| `--identities` | int | 50 | Number of test identities |
| `--types` | CSV | `meta` | Molecule types |
| `--metas-per-identity` | int | 100 | Meta mutations per identity |
| `--transfers-per-identity` | int | 10 | Value transfers per identity |
| `--rules-per-identity` | int | 5 | Rule molecules per identity |
| `--burns-per-identity` | int | 5 | Burn molecules per identity |
| `--token-amount` | float | 1000000.0 | Initial token supply |

```bash
knishio bench generate --types meta,value-transfer --identities 100 -o plan.db
```

### bench execute

Execute a previously generated plan file against the validator.

```bash
knishio bench execute <PLAN> [OPTIONS]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<PLAN>` | path | *(required)* | Path to SQLite plan file |
| `--endpoint` | URL | `https://localhost:8080` | Validator endpoint |
| `--concurrency` | int | 5 | Concurrent submissions |
| `--cell-slug` | string | *(none)* | Target cell slug |
| `--keep` | flag | false | Retain benchmark data after execution |

```bash
# Execute with high concurrency
knishio bench execute plan.db --concurrency 20 --cell-slug TESTCELL
```

### bench clean

Clean up benchmark data from the database. Only cells prefixed with `BENCH_CLI_` can be purged (safety guard).

```bash
knishio bench clean [--cell-slug <SLUG>] [--all]
```

| Flag | Description |
|------|-------------|
| `--cell-slug` | Purge a specific benchmark cell |
| `--all` | Purge ALL benchmark cells (`BENCH_CLI_*`) |

```bash
# Clean up a specific benchmark cell
knishio bench clean --cell-slug my-bench

# Clean up all benchmark data
knishio bench clean --all
```

## Embedding Management

Manage the DataBraid VKG (Vector Knowledge Graph) embedding system. Requires `EMBEDDING_ENABLED=true` on the validator.

> **Heads-up**: the standalone compose profile (`knishio start --accel cpu`) defaults `EMBEDDING_ENABLED=false`. The `--accel dmr` overlay re-enables it and points at Docker Model Runner on the host. For other paths, set `EMBEDDING_ENABLED=true` + `EMBEDDING_PROVIDER` + the provider-specific endpoint/key vars via a `.env` file or compose override.

### embed status

Show embedding coverage statistics — how many metadata records have embeddings, which models are in use, and coverage percentages.

```bash
knishio embed status
```

### embed reset

Clear embeddings so the validator's automatic backfill worker re-embeds them. Useful after changing embedding models or dimensions.

```bash
knishio embed reset [--model <NAME>] [--all] [-y, --yes]
```

| Flag | Description |
|------|-------------|
| `--model` | Clear only embeddings from a specific model |
| `--all` | Clear ALL embeddings (nuclear option) |
| `-y, --yes` | Skip confirmation prompt |

```bash
# Clear embeddings from a specific model
knishio embed reset --model qwen3-embedding-0.6b -y

# Clear everything
knishio embed reset --all -y
```

### embed search

Run semantic (vector similarity) search against DAG metadata from the terminal.

```bash
knishio embed search <QUERY> [--limit <N>] [--threshold <F>] [--meta-type <TYPE>]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<QUERY>` | string | *(required)* | Natural language search query |
| `--limit` | int | 10 | Maximum number of results |
| `--threshold` | float | 0.7 | Minimum cosine similarity (0.0 to 1.0) |
| `--meta-type` | string | *(none)* | Filter results by meta_type |

```bash
knishio embed search "user profile settings"
knishio embed search "token metadata" --limit 20 --threshold 0.8
knishio embed search "device telemetry" --meta-type deviceTelemetry
```

### embed ask

Ask a natural language question about DAG data using RAG (Retrieval-Augmented Generation). Requires `GENERATION_ENABLED=true` on the validator — same caveat as `embed status` above: the standalone profile defaults this to `false`; `--accel dmr` re-enables it via the overlay.

```bash
knishio embed ask <QUESTION> [--max-results <N>] [--threshold <F>] [--meta-type <TYPE>]
```

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<QUESTION>` | string | *(required)* | Natural language question |
| `--max-results` | int | 20 | Maximum source records to consider |
| `--threshold` | float | 0.5 | Minimum cosine similarity |
| `--meta-type` | string | *(none)* | Filter by meta_type |

```bash
knishio embed ask "what stores sell kitchen stuff?"
knishio embed ask "who has the most tokens?" --max-results 30
knishio embed ask "recent device readings" --meta-type deviceTelemetry --threshold 0.6
```

## Health Checks

HTTP GET requests to the validator's health endpoints. TLS certificates are validated by default (30-second timeout). Set `insecure_tls = true` in config to accept self-signed certificates for local development.

### health

Liveness check by default, or a richer report with `--full`.

```bash
# Liveness (GET /healthz) — cheap, always 200 if the process is alive
knishio health
# ✓ Healthy (https://localhost:8080)

# Full report (GET /health) — DB latency + cache stats + version
knishio health --full
# ✓ Healthy (https://localhost:8080) · v0.2.0
#
# Database
#   status        connected
#   latency       1 ms
#
# Query-embedding cache
#   entries       0
#   hit ratio     0.00
```

### ready

Readiness check (is the validator ready to accept traffic?).

```bash
knishio ready
# ✓ Ready
```

Hits `GET /readyz`. Returns success on HTTP 200.

### full

Readiness check with full detail — prints the JSON response body.

```bash
knishio full
# ✓ Ready
# {
#   "status": "ready",
#   "database": { "status": "connected", "latency_ms": 3 },
#   "migrations": { "applied": 40, "expected": 34, "is_current": true },
#   "cache": { "entries": 0, "hit_ratio": "0.00" },
#   "version": "0.2.0"
# }
```

### db

Database consistency check — migrations, schema integrity, and issue reporting.

```bash
knishio db
# ✓ Database consistency check passed
#
# Migrations
#   Applied: 40 / 34 expected
#   Up to date
```

If issues are found:
```bash
knishio db
# ✗ Database consistency check FAILED
#
# Migrations
#   Applied: 36 / 38 expected
#   Migrations pending!
#
# Issues
#   • Missing table: cells
#   • Missing trigger: cascade_on_bond_insert
```

Hits `GET /db-check`. Reports migration status, missing tables, and missing triggers.

## Observability

Live introspection into the AI pipeline + validator metrics.

### ai status

AI pipeline snapshot: provider, model, sampling parameters, recent inference latency, backfill coverage, query-cache hit rate, acceleration label.

```bash
knishio ai status
# Embedding
#   ✓ enabled        openai-compatible
#   model            huggingface.co/qwen/qwen3-embedding-4b-gguf:latest
#   dimensions       2560
#
# Generation
#   ✓ enabled        openai-compatible
#   model            huggingface.co/unsloth/gemma-4-e4b-it-gguf:latest
#   sampling         temp=0.60 top_p=0.95 freq=0.70 pres=0.40
#   tokens           max=6144 n_ctx=12288
#   recent           2 calls over 43s · 0 errors · avg 33.8s · min 25.1s · max 42.5s
#
# Backfill
#   ✓ coverage       5,885 / 5,885
# …
```

Hits `GET /ai/status`. The `recent` block summarises the last 100 generation calls via an in-memory ring buffer — useful for "is the model slow right now?" during debugging. Prometheus histograms at `/metrics` give the long-window view; use `knishio metrics` below.

### metrics

Fetch + pretty-print the validator's Prometheus scrape, grouped by subsystem.

```bash
knishio metrics
# Validator metrics (https://localhost:8080)
#
# AI / Embedding
#   knishio_embedding_backfill_pending                 0
#
# AI / Model Load
#   knishio_model_load_seconds{service="embedding"}    0.000044
#   knishio_model_load_seconds{service="generation"}   0.000009
#
# Database
#   knishio_db_connections_active                      0
#   …
```

| Flag | Description |
|------|-------------|
| `--filter <substring>` | Case-insensitive match on metric name — e.g. `--filter embedding` shows only AI-embedding metrics |
| `--raw` | Passthrough the raw Prometheus text exposition for piping into another parser |

```bash
# Filter to a subsystem
knishio metrics --filter cache

# Pipe into prom-to-JSON
knishio metrics --raw | prom2json
```

Histograms render as `count=N sum=Ts avg=Ts`; for full bucket counts use `--raw`.

### watch embeddings

Live-stream DataBraid embedding-pipeline events as rows get embedded (subscription `embeddingChanges`). Emits one JSON event per line on stdout, jq-friendly. Ctrl-C closes the subscription cleanly.

```bash
knishio watch embeddings
# ℹ Subscribed to embeddingChanges; streaming events (Ctrl-C to stop)…
# {"embeddedAt":1776718124,"key":"audienceData","metaId":"pet-wants-1","metaType":"KKStore","model":"…","molecularHash":"…","state":"COMPLETE"}
# {"embeddedAt":1776718125,"key":"description","metaId":"pet-wants-1","metaType":"KKStore", …}
# …
```

| Flag | Description |
|------|-------------|
| `--meta-type <T>` | Filter to a single MetaType (e.g. `KKStore`) |
| `--meta-id <I>` | Filter to a single MetaId |

### watch dag

Live-stream DAG structure events (subscription `dagChanges`) — molecule acceptance + bond creation.

```bash
knishio watch dag
# ℹ Subscribed to dagChanges; streaming events (Ctrl-C to stop)…
# {"eventType":"MOLECULE_ACCEPTED","molecularHash":"…","status":"accepted","height":42,"cellSlug":"TESTCELL", …}
# {"eventType":"BOND_CREATED","molecularHash":"…","bondType":"M_TIER1","bondHash":"…", …}
# …
```

| Flag | Description |
|------|-------------|
| `--cell <slug>` | Filter to a single cell's DAG events |

Uses the modern `graphql-transport-ws` subprotocol over WSS. Self-signed certificates are accepted when `insecure_tls = true` is set in config.

## Shell Integration

### completions

Generate a shell completion script. Redirect to your shell's completion directory for persistent tab-completion of subcommands, flags, and enum values.

```bash
# Zsh (~/.zsh/completion/)
knishio completions zsh > ~/.zsh/completion/_knishio

# Bash
knishio completions bash > /etc/bash_completion.d/knishio

# Fish
knishio completions fish > ~/.config/fish/completions/knishio.fish
```

Supported shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`.

## Path Discovery

The CLI automatically finds required files by walking up the directory tree from your current working directory.

**Docker Compose files** — the resolved accel profile (see [Hardware Acceleration](#hardware-acceleration)) expands to a list of compose files; each is independently located by walking up from CWD through these candidates:
1. `./<file>`
2. `./knishio-validator-rust/<file>`
3. `./servers/knishio-validator-rust/<file>`

The files are then passed to `docker compose -f a -f b …` in order (base first, overlay second). Default accel-to-files mappings are baked into the CLI and can be overridden in `knishio.toml` under `[docker.accel.<name>]` tables.

**Env file auto-loading** — when any compose file in the resolved chain has "production" in its name and a `.env.production` exists alongside it, the CLI automatically passes `--env-file .env.production` to Docker Compose.

**Config file** — checks in order:
1. `./knishio.toml`
2. `./knishio-validator-rust/knishio.toml`
3. `./servers/knishio-validator-rust/knishio.toml`
4. *(walks up parent directories repeating the above)*

This means the CLI works whether you run it from inside the validator dir, the servers dir, or the monorepo root.

## Example Workflows

### Apple Silicon Development (GPU-accelerated via DMR)

```bash
# 1. One-time DMR setup (only if you haven't already)
knishio dmr enable
knishio dmr pull

# 2. Confirm the CLI sees the right path
knishio detect
# → Accel:   dmr  (Apple Silicon + DMR TCP reachable)

# 3. Start — auto-uses standalone.yml + dmr.yml
knishio start -d --build

# 4. Seed a cell
knishio cell create TESTCELL --name "Test Cell"

# 5. Exercise the RAG pipeline end-to-end
knishio embed status
knishio embed search "user profile"
knishio embed ask "who has the most tokens?"
```

The validator container runs a plain Linux CPU build; embedding and generation traffic is routed to the host-side llama.cpp-with-Metal process DMR manages.

### Development

```bash
# 1. Start the stack
knishio start -d --build

# 2. Wait for readiness
knishio ready

# 3. Create a test cell
knishio cell create TESTCELL --name "Test Cell"

# 4. Check database state
knishio db

# 5. Run a mixed benchmark
knishio bench run \
  --types meta,value-transfer,rule \
  --identities 50 \
  --concurrency 10 \
  --cell-slug TESTCELL

# 6. Check DAG explorer
# Open https://localhost:8080/dag in your browser

# 7. View logs if something looks wrong
knishio logs -f --tail 50

# 8. Rebuild after code changes
knishio rebuild

# 9. Clean up when done
knishio destroy --volumes
```

### Production Deployment

```bash
# 1. First-time setup
knishio init --tls --cors "https://myapp.example.com"

# 2. Launch production stack
knishio start --build -d

# 3. Seed your application cell
knishio cell create MYAPP --name "My Application"

# 4. Verify health
knishio full

# 5. Create initial backup
knishio backup create
```

### Ongoing Operations

```bash
# Before any upgrade, take a backup
knishio backup create

# Pull latest and restart (health-gated)
knishio update

# If something goes wrong
knishio update --rollback

# List available backups
knishio backup list

# Restore from backup if needed
knishio restore backups/knishio_20260406_174028.sql

# Quick database query
knishio psql -c "SELECT count(*) FROM molecules"

# Check embedding coverage
knishio embed status

# Semantic search
knishio embed search "user profile"
```

## Output Symbols

| Symbol | Meaning |
|--------|---------|
| ✓ | Success (green) |
| ℹ | Informational (blue) |
| ⚠ | Warning (yellow) |
| ✗ | Error (red) |
