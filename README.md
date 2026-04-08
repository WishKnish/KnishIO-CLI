# knishio

[![Crates.io](https://img.shields.io/crates/v/knishio-cli.svg)](https://crates.io/crates/knishio-cli)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

Unified CLI for orchestrating the KnishIO validator stack — production deployment, Docker control, cell management, database management, benchmarks, embeddings, and health checks.

## Quick Start

```bash
# Install the CLI
cargo install knishio-cli

# Start the validator stack
knishio start -d --build

# Create a cell and check health
knishio cell create TESTCELL --name "Test Cell"
knishio health

# Run a benchmark
knishio bench run --types meta --identities 50 --cell-slug TESTCELL

# Tear it all down
knishio destroy --volumes
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
[validator]
url = "https://localhost:8080"
insecure_tls = false  # set to true for self-signed certs

[docker]
compose_file = "docker-compose.standalone.yml"
postgres_container = "knishio-postgres"
validator_container = "knishio-validator"

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

### Global CLI Flags

```
--url <URL>    Validator base URL for health commands [default: https://localhost:8080]
-h, --help     Print help
-V, --version  Print version
```

The `--url` flag applies to `health`, `ready`, `full`, and `db` commands. TLS certificates are validated by default. To accept self-signed certificates (e.g., local dev), set `insecure_tls = true` in `knishio.toml` or `KNISHIO_INSECURE_TLS=true`. All health requests have a 30-second timeout.

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
knishio start [--build] [-d, --detach]
```

| Flag | Description |
|------|-------------|
| `--build` | Build images before starting |
| `-d, --detach` | Run in detached mode (background) |

```bash
# Interactive foreground
knishio start

# Detached with rebuild
knishio start -d --build
```

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
knishio rebuild
```

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

Ask a natural language question about DAG data using RAG (Retrieval-Augmented Generation). Requires `GENERATION_ENABLED=true` on the validator.

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

Quick liveness check.

```bash
knishio health
# ✓ Healthy (https://localhost:8080)
```

Hits `GET /healthz`. Returns success on HTTP 200.

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

## Path Discovery

The CLI automatically finds required files by walking up the directory tree from your current working directory.

**Docker Compose file** — checks in order:
1. `./<compose_file>` (from `knishio.toml` or default)
2. `./knishio-validator-rust/<compose_file>`
3. `./servers/knishio-validator-rust/<compose_file>`

The default compose file is `docker-compose.standalone.yml`. After running `knishio init`, the generated `knishio.toml` points to `docker-compose.production.yml` instead.

**Env file auto-loading** — when the compose file name contains "production" and a `.env.production` file exists in the same directory, the CLI automatically passes `--env-file .env.production` to Docker Compose.

**Config file** — checks in order:
1. `./knishio.toml`
2. `./knishio-validator-rust/knishio.toml`
3. `./servers/knishio-validator-rust/knishio.toml`
4. *(walks up parent directories repeating the above)*

This means the CLI works whether you run it from inside the validator dir, the servers dir, or the monorepo root.

## Example Workflows

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
