# Changelog

## 0.2.7

Bug-fix release: `cell` / `audit` now work on a bare-metal deployment. Found by
running the CLI on the testnet server itself.

- **`cell` / `audit` had no transport for a local bare-metal PostgreSQL** (CLI-7).
  `PsqlTransport` offered only `DockerExec` and `Ssh`, so **"local" was hardcoded to
  mean "docker"** — and a runbook host runs PGDG Postgres with no Docker at all.
  Every route dead-ended: bare `cell list` and `--local` both hit
  `docker exec` → *"Failed to exec into postgres container — is the stack running?"*,
  telling the operator to start a stack that cannot exist there, while
  `--host user@host` meant ssh-ing to yourself. New `LocalPsql` variant runs
  `sudo -n -u postgres psql`, which is exactly what the
  `(postgres) NOPASSWD: /usr/bin/psql` grant that `deploy bootstrap` already installs
  permits — so nothing new needs provisioning. No new flags, no new config.
- **Selection cannot regress a developer machine.** A container that *exists but is
  stopped* still resolves to Docker, preserving the correct "start the stack"
  diagnosis; only a genuinely absent container (or no Docker) falls through to bare
  metal. Silently connecting to some *other* local Postgres would be far worse than
  an error, so the decision is a pure function over observed capabilities with the
  whole matrix unit-tested — including a guard, verified failing, that a stopped
  container must not fall through.
- **The TARGET banner no longer lies.** `main.rs` re-derived the banner inline and
  hardcoded `docker://…` for anything non-ssh — a *second* copy of the same
  assumption, which would have reported `docker://` while actually using
  `sudo -u postgres psql`. It now derives from the resolved transport's
  `describe()`, removing the drift structurally (same class as CLI-4/CLI-5), and
  prints nothing when no transport resolves rather than naming a target it could not
  determine.
- **Better errors.** A stopped container now says *"exists but is stopped — run
  `knishio start`"* instead of falling through to a generic message; when nothing is
  available the error names **both** attempted paths (docker container and the sudo
  psql command) plus the `--host` option. The CLI-2 guard's wording no longer says
  `--local` means "the local docker stack", since it now means this machine's
  database either way.
- **Note on confirmation:** bare-metal local psql counts as local, so mutating
  `cell`/`audit` commands there do **not** prompt (deliberate). On a deployed node
  "local" is production — read-only commands never prompted anyway, and `--yes`
  remains for scripts.

Verified on the testnet box (built from source there, since `cell list` had no way to
work before this): banner reads `local://postgres (… bare metal)`, `cell list` matches
raw `psql` exactly (2 cells), `cell usage` and `audit list` work, and
`activate`/`archive` were confirmed against the database on each transition. `--host`
and the CLI-2 guard behave as before.

## 0.2.6

Day-2 ops release: alerting that can actually reach a human, and bounded CD disk
usage. Pairs with validator-side changes to `deploy/health-monitor.sh` and a new
`deploy/cargo-target-prune.sh`.

### New

- **`deploy bootstrap --alert-cmd` / `--ping-url`** wire the readiness monitor to a
  real channel. Both are opt-in: with neither flag the generated unit is unchanged,
  so nothing silently starts paging. `--alert-cmd` runs on sustained failure;
  `--ping-url` is a **dead-man's-switch check-in** curled on every *successful*
  probe (Sentry Crons / Healthchecks.io style). The check-in exists because nothing
  running **on** the host can report that the host died — an external service
  noticing check-ins stopped is the only way to learn that. It also covers the edge
  blind spot: the monitor probes `127.0.0.1`, so an nginx/TLS/certificate failure
  takes the public site down while the local probe still reports healthy.
- **`Environment=STATE_DIR=/var/lib/knishio`** on `knishio-health.service`, which is
  what makes `FAIL_THRESHOLD` mean anything in the `--once` mode the timer actually
  uses. Each firing is a fresh process, so the consecutive-failure count has to be
  persisted; without it every single transient blip alerted. Asserted to be created
  before the probe timer starts.
- **`knishio-cargo-prune` unit + weekly timer** (mirroring `knishio-backup-prune`).
  `make deploy-gate` compiles the test targets, and Forge builds each release in a
  fresh path, so the crate's artifacts are re-emitted under a new metadata hash every
  deploy (~1–2 GB) and cargo never collects the old ones. The prune wipes the **dev**
  profile only when free space is genuinely low; the release profile holding the
  installed binary is never touched.

### Internal

- Both the prune-script install and its unit are **guarded** on the file existing:
  tarballs built before this work do not ship `cargo-target-prune.sh`, and an
  unguarded `install` of a missing file would abort the whole bootstrap under
  `set -e`. Same reasoning as the `make -n deploy-gate` guard — the generator and the
  validator revision being installed can legitimately differ in age.
- New assertions cover STATE_DIR, its ordering before the timer, both guards, and
  that the alert/ping `Environment=` lines land inside the health unit *before* its
  `ExecStart`. All three were confirmed to fail when their generated line is removed
  or moved.

## 0.2.5

Single-purpose release: the generated Forge CD script disables incremental
compilation, which on a deploy box is pure write-only waste.

- **`deploy forge` exports `CARGO_INCREMENTAL=0`.** Incremental caches are keyed to
  the source directory, and Forge compiles every deploy in a fresh
  `releases/<id>` path — so each session dir is written once and never read again.
  Measured on testnet: **43 session dirs, 4.6 GB, after only two gated deploys**,
  growing with every push. The gate's speed comes from the dependency artifacts in
  `$CARGO_TARGET_DIR/debug/deps`, which are path-independent and untouched by this,
  so disabling incremental costs nothing here. A new assertion requires the export
  and requires it to appear **before** the build it applies to.

## 0.2.4

Single-purpose release: the generated Forge CD script now gates the deploy on the
validator's own test suite.

- **`deploy forge` runs `make deploy-gate` before the binary swap.** The generated
  build script previously gated only on `cargo build --release --locked`, so a
  commit that compiled but failed the validator's gates (patent traceability,
  no-unwrap, GraphQL SDL / OpenAPI drift, protocol invariants, offline tests) still
  reached production — and with Forge quick-deploy enabled, *every push* takes that
  path automatically. `set -euo pipefail` makes a failing gate abort **before**
  `upgrade.sh`, so there is no pg_dump, no swap and no restart: production keeps
  the running binary. The call is guarded by `make -n deploy-gate`, so validator
  revisions that predate the target skip it with a notice instead of failing every
  deploy. A new assertion checks the gate is emitted **before** `upgrade.sh` — a
  gate placed after the swap would report failures on an already-deployed build,
  which is worse than no gate because it reads as protection. Verified by breaking
  a gate in a scratch release directory and confirming the script exits non-zero
  with the installed binary and service start time untouched.

## 0.2.3

Closes the CLI-5 bug class with two independent nets, and completes `watch`
coverage of the validator's subscriptions.

- **Schema-contract tests (build-time net).** Every subscription query is now
  validated against the validator's GraphQL SDL — recursing into nested
  selections — so a wire-name mismatch fails `cargo test` instead of shipping.
  This is the check that CLI-5 needed: the SDL *is* the wire contract, so
  `#[graphql(name = …)]` renames are handled by construction. The SDL is vendored
  at `tests/validator-schema.graphql` so the test also runs in CI (which checks
  out only this repo), plus a monorepo-only test that fails if the vendored copy
  drifts from the validator's committed baseline. All three were verified to FAIL
  on purpose (reintroduced CLI-5, drifted the SDL, unregistered a subscription).
- **`verify` now opens a real subscription (runtime net).** The `ws-graphql` /
  `ws-alias` checks previously stopped at the `connection_init`→`ack` handshake,
  which reports a green socket even when the server rejects the query document —
  exactly how CLI-5 passed verification while being completely broken. They now
  subscribe: a rejection is a Fail, while silence in the window is a Pass (an
  idle DAG legitimately emits nothing). Confirmed live: with CLI-5 reintroduced,
  `verify` fails with the server's own rejection message.
- **`watch` covers all six validator subscriptions** — added `wallet-status`
  (`--bundle --token`), `active-user` (`--meta-type --meta-id`) and
  `active-wallet` (`--bundle`). Previously only `embeddings`, `dag` and
  `molecules` were watchable, while the docstring wrongly claimed those were "the
  validator's only subscriptions". A new test asserts the registry covers every
  subscription the SDL exposes, so a newly added server subscription surfaces
  instead of going unnoticed.
- **Internal:** subscription queries now live in one `SUBSCRIPTIONS` registry
  (single source of truth for dispatch and the contract tests, so a new
  subscription is covered automatically), and graphql-transport-ws message
  classification is single-sourced in `src/ws.rs` — shared by `watch`'s streaming
  loop and `verify`'s probe rather than written twice.

## 0.2.2

Bug-fix release. Both HIGH items were found by exercising the CLI against a
live deployment (testnet.knish.io) — neither was reachable by the unit/stub
tests, and both shipped broken in 0.2.0/0.2.1.

- **`deploy ship --execute` was broken on its own default `--dest`** (CLI-4).
  The remote SHAKE256 command embedded the path in a *Python string literal*,
  and `open("~/…")` does not expand `~` (tilde expansion is a shell feature) —
  so verification raised `FileNotFoundError` and the CLI reported a bogus
  **"chain-of-custody FAILED"** on an upload that had actually succeeded. The
  remote command now uses `os.path.expanduser`, is generated by a single helper
  shared by the printed (artifact-mode) and executed paths so the two cannot
  drift, and an empty hash now surfaces the remote stderr instead of a
  misleading mismatch. Regression test added.
- **`knishio watch dag` could never emit an event** (CLI-5). The subscription
  selected `bundle` on `DagChange`, but the validator exposes that field as
  `bundleHash` (`#[graphql(name = …)]`), so GraphQL rejected the whole document
  and tore the subscription down at subscribe time. Worse, the client logged the
  rejection as a warning and kept waiting on a dead socket — printing
  "streaming events…" and hanging until an external timeout, which looks like a
  healthy idle stream. Fixed the field name, and a server-reported error or an
  immediate `complete` with **zero** events delivered now fails loudly with a
  non-zero exit instead of blocking. Verified live: `watch dag` now streams real
  `MOLECULE_ACCEPTED` / `BOND_CREATED` events through the nginx edge.
- **`audit list`**: a target type with no target id rendered as a dangling
  `config=`; it now prints `config` (all four empty/non-empty combinations
  handled).

## 0.2.1

Patch release: honest bench banners, portable Linux installs, and a real
libcrux vulnerability fix (via the SDK).

- **CLI-3 fix — honest `bench` target banners.** `bench` printed the HTTP-URL
  banner even for its `--host` psql/ssh cell-admin transport, so
  `bench clean --host <remote>` showed a wrong `TARGET: https://localhost:8080`
  while correctly operating on the remote. Each bench subcommand now prints its
  own precise banner(s): run/execute show the HTTP submit target **and** the
  cell-admin transport; clean shows the psql/ssh transport; generate is silent.
- **Portable Linux installs / green CI — dropped the built-in bench PNG.** The
  latency plot used `plotters`' `ttf` feature → `font-kit` → system
  `fontconfig`, which broke `cargo clippy`/build in CI and **every
  `cargo install knishio-cli` on a Linux box without libfontconfig**. `bench`
  still writes the latency **CSV** + report **JSON** (plot them with the repo's
  Python script); the `plotters` dependency and its font/image subtree are gone.
- **Security — cleared 3 libcrux advisories at the source.** RUSTSEC-2026-0207,
  -0208 (`libcrux-sha3`) and -0212 (`libcrux-secrets`) reached the CLI
  transitively via `knishio-client`. Fixed in the SDK (bumped `libcrux-ml-kem`
  0.0.9→0.0.10, pulling libcrux-sha3 0.0.10 + libcrux-secrets 0.0.6) and
  consumed here by bumping **`knishio-client` 0.9.2 → 0.9.3** — a real fix, not
  an audit-ignore. `cargo audit` is clean (0 vulnerabilities). ML-KEM byte-parity
  with the other SDKs was validated across the bump.

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
