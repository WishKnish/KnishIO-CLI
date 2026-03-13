# knishio

[![Crates.io](https://img.shields.io/crates/v/knishio-cli.svg)](https://crates.io/crates/knishio-cli)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

Unified CLI for orchestrating the KnishIO validator stack — Docker control, cell management, benchmarks, and health checks.

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
- **Running validator stack** for cell and health commands
- **knishio-bench binary** for benchmark commands — build it with:
  ```bash
  cd servers/knishio-bench && cargo build --release
  ```

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

## Docker Control

All Docker commands locate `docker-compose.standalone.yml` automatically by walking up from your current directory (see [Path Discovery](#path-discovery)).

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
| Slug | 1–64 characters, alphanumeric + dashes + underscores only (`[a-zA-Z0-9_-]`) |
| Name | 1–256 characters, no null bytes or control characters |
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

## Benchmarks

Benchmark commands delegate to the `knishio-bench` binary. The CLI locates it automatically (see [Path Discovery](#path-discovery)).

If the bench binary isn't found, you'll see:
```
knishio-bench binary not found. Build it first:
  cd servers/knishio-bench && cargo build --release
```

### bench run

Generate a benchmark plan and execute it in one shot. The temporary plan file is cleaned up automatically.

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

```bash
# Quick meta-only benchmark
knishio bench run --types meta --identities 20 --cell-slug TESTCELL

# Mixed isotope benchmark
knishio bench run --types meta,value-transfer,rule --identities 50 --concurrency 10 --cell-slug TESTCELL

# High-throughput stress test
knishio bench run --types meta --identities 100 --metas-per-identity 200 --concurrency 20
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

```bash
# Execute with high concurrency
knishio bench execute plan.db --concurrency 20 --cell-slug TESTCELL
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
#   "database": { "status": "connected", "latency_ms": 0 },
#   "migrations": { "applied": 38, "expected": 34, "is_current": true },
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
#   Applied: 38 / 38 expected
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
1. `./docker-compose.standalone.yml`
2. `./knishio-validator-rust/docker-compose.standalone.yml`
3. `./servers/knishio-validator-rust/docker-compose.standalone.yml`

This means the CLI works whether you run it from inside the validator dir, the servers dir, or the monorepo root.

**Bench binary** — checks in order:
1. `../knishio-bench/target/release/knishio-bench` (relative to validator dir)
2. `../knishio-bench/target/debug/knishio-bench`
3. `knishio-bench` on your `PATH`

## Example Workflow

A typical development session:

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

## Output Symbols

| Symbol | Meaning |
|--------|---------|
| ✓ | Success (green) |
| ℹ | Informational (blue) |
| ⚠ | Warning (yellow) |
| ✗ | Error (red) |
