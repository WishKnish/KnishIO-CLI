# Changelog

## 0.2.0

Deployment-orchestration release. Turns the CLI into the single tool for
standing up and operating a KnishIO validator, encoding the OPS-010 bare-metal
runbook and the 14 findings from the first live testnet deployment
(`testnet.knish.io`, validator repo `docs/audits/TESTNET-DEPLOY-2026-07-23.md`).

### New

- **`knishio verify`** — deployment acceptance gauntlet against any validator
  URL: liveness/readiness (migrations `applied == expected`, read from the
  endpoint — never hardcoded), GraphQL, WebSocket subscriptions on
  `/graphql/ws` **and** `/ws`, unbuffered SSE, edge hardening (HSTS,
  http→https redirect, `/metrics` + `/config` blocked), rate-limit headers,
  and TLS certificate expiry. `--json` for CI; `--edge auto|edge|direct`;
  `--write-smoke` runs the full crypto write path (U-isotope auth →
  OTS-signed `createMeta` → readback, leaving one `KNISHIO_VERIFY_SMOKE` meta).
- **`knishio deploy`** — generates the validated deployment artifacts (run
  with `--execute` for the ssh steps):
  - `bootstrap` — one idempotent root script (runbook §1–§7 + day-2 timers)
    with every learned hard gate: PGDG PG16 + pgvector (≥0.7 floor),
    `uuid-ossp` **and** `vector` pre-created before first boot, SHAKE256
    chain-of-custody, directive-only docker-unit check, `_FILE` secrets
    generated on-server, mandatory `TRUSTED_PROXY_IPS` behind a proxy,
    scoped-`visudo -cf` sudoers.
  - `env` — the `/etc/knishio/.env` production baseline.
  - `edge` — nginx reverse-proxy vhost, generic or `--flavor forge`
    (preserves FORGE markers, drops the colliding shared `site.conf`).
  - `forge` — the Forge CD pair (minimal ASCII deploy script + server-side
    build script; multi-line deploy scripts break Forge's template expansion).
  - `ship` — scp a release tarball with SHAKE256 verified on both ends.
  - `upgrade` — drive the server's `upgrade.sh` (backup → swap → readiness
    gate → auto-rollback) over ssh, with a `sudo -n -l` preflight.
- **`--profile dev|production`** (+ `[docker] profile`) — makes
  `docker-compose.production.yml` reachable from `start`/`update`/etc.
  **Fixes CLI-1** (no accel profile could select the production stack).
- **`--host user@host` / `--local`** — cell/audit administration can now run
  `psql` on a remote server over ssh, not only the local docker stack.
- `package --arch amd64|arm64|all` — multi-arch Linux tarballs.
- `[deploy]` config section (`host`/`domain`/`arch`/`staging_dir`).

### Changed (behavior)

- **Uniform target model (CLI-2 fix).** Every command prints a `TARGET:`
  banner naming where its effects land — the HTTP URL (with the precedence
  source), the local docker container, or the ssh host — and **never the URL
  for a command that doesn't speak HTTP**. Mutating commands aimed at a
  non-local target now **prompt for confirmation** (`--yes` to bypass;
  non-interactive sessions must pass `--yes` or they fail fast).
- `--url` is honored everywhere with precedence
  `--url` > `KNISHIO_URL` > `knishio.toml` > default; an explicit
  `--url https://localhost:8080` now correctly overrides a config-file URL.
- **`bench --endpoint` is deprecated** — bench now uses the resolved target
  URL like every other command (it silently ignored `--url` before).
- **`cell`/`audit` refuse a bare non-local `--url`** with guidance to pass
  `--host` (remote psql) or `--local` (explicit local) — this is the exact
  footgun that cost hours in the first deployment.
- `update` errors instead of restarting the local stack while health-checking
  a non-local URL.
- `[docker] compose_file` is deprecated (it was never consulted); a
  non-default value warns and points at `profile`. `knishio init` now writes
  `profile = "production"`.

### Internal

- Shared HTTP client (`src/http.rs`) replaces ~11 duplicated `reqwest`
  builders; shared WS handshake (`src/ws.rs`) is reused by `watch` and
  `verify`. Removed `update.rs`'s private `compose()` (it applied
  `.env.production` only when "production" was in the file *name*).
- **`Cargo.lock` is now committed** (binary-crate convention; F-12): the
  test CI job builds `--locked`; the audit job resolves fresh (`cargo update`)
  so advisory coverage still tracks new consumers.
- New dependencies: `x509-parser` (cert expiry), `sha3` + `hex` (ship
  chain-of-custody).

## 0.1.8

Prior release (Docker-stack lifecycle, health, bench, cells, embed, AI
introspection).
