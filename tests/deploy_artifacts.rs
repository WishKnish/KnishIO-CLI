//! Deploy-artifact generation tests — run in CI (no live stack needed).
//! Drives the built binary (CARGO_BIN_EXE_knishio, same pattern as
//! cell_integration.rs), then asserts the generated artifacts carry the
//! invariants learned from the live testnet deployment.

use std::path::{Path, PathBuf};
use std::process::Command;

fn knishio() -> Command {
    Command::new(env!("CARGO_BIN_EXE_knishio"))
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("knishio-deploy-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn bash_n(path: &Path) {
    let out = Command::new("bash").arg("-n").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "bash -n failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn bootstrap_artifact_invariants() {
    let dir = tmpdir("bootstrap");
    let out = knishio()
        .args([
            "deploy", "bootstrap",
            "--user", "forge",
            "--behind-proxy",
            "--output", dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let script = dir.join("knishio-bootstrap.sh");
    bash_n(&script);
    let s = std::fs::read_to_string(&script).unwrap();

    // F-13: uuid-ossp AND vector created before the service starts.
    let uuid = s.find(r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp""#).expect("uuid-ossp");
    let vector = s.find("CREATE EXTENSION IF NOT EXISTS vector").expect("vector");
    let enable = s.find("systemctl enable --now knishio-validator").expect("enable");
    assert!(uuid < enable && vector < enable, "extensions must precede first boot");
    // F-1: directive-only docker check.
    assert!(s.contains(r"^(Requires|After)=.*docker\.service"));
    // Forge sudoers lesson: scoped validation only.
    assert!(s.contains("visudo -cf /etc/sudoers.d/knishio-deploy"));
    // hex-only DB password (base64 breaks DSN parsing).
    assert!(s.contains("openssl rand -hex"));
    assert!(!s.contains("rand -base64"));
    // F-5: behind-proxy env carries TRUSTED_PROXY_IPS.
    assert!(s.contains("TRUSTED_PROXY_IPS=127.0.0.1"));
    // F-10: KEM secret file.
    assert!(s.contains("VALIDATOR_KEM_SECRET_FILE=/etc/knishio/kem.secret"));
    // F-11: stray CWD .env check.
    assert!(s.contains("/var/lib/knishio/.env"));
    // F-3: endpoint-driven migration comparison, no hardcoded count.
    assert!(s.contains("m.get('applied') == m.get('expected')"));
    assert!(!s.contains("== 50"), "must not hardcode a migration count");
    // pgvector floor gate.
    assert!(s.contains("0.7.0"));
}

#[test]
fn env_artifact_variants() {
    let dir = tmpdir("env");
    // Behind proxy.
    assert!(knishio()
        .args(["deploy", "env", "--behind-proxy", "--output", dir.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let e = std::fs::read_to_string(dir.join("knishio.env")).unwrap();
    assert!(e.contains("SERVER_HOST=127.0.0.1"));
    assert!(e.contains("TRUSTED_PROXY_IPS=127.0.0.1"));
    assert!(e.contains("KNISHIO_ENV=production"));
    assert!(e.contains("JWT_SECRET_FILE="));
    assert!(e.contains("VALIDATOR_KEM_SECRET_FILE="));
}

#[test]
fn edge_artifacts_generic_and_forge() {
    let dir = tmpdir("edge");
    for flavor in ["generic", "forge"] {
        assert!(knishio()
            .args([
                "deploy", "edge",
                "--domain", "testnet.example.com",
                "--flavor", flavor,
                "--forge-server-id", "12345",
                "--output", dir.to_str().unwrap(),
            ])
            .output()
            .unwrap()
            .status
            .success());
    }
    let generic = std::fs::read_to_string(dir.join("nginx-testnet.example.com.conf")).unwrap();
    let forge = std::fs::read_to_string(dir.join("nginx-testnet.example.com.forge.conf")).unwrap();
    for conf in [&generic, &forge] {
        // WS on BOTH routes; SSE unbuffered; body limit; HSTS at edge;
        // internals blocked; X-Forwarded on every location.
        assert!(conf.contains("location ~ ^/(graphql/ws|ws)$"));
        assert!(conf.contains("proxy_buffering off"));
        assert!(conf.contains("client_max_body_size 10m"));
        assert!(conf.contains("Strict-Transport-Security"));
        assert!(conf.contains("location = /metrics { return 404; }"));
        assert!(conf.contains("location = /config  { return 404; }"));
        assert!(conf.contains("proxy_set_header X-Forwarded-For"));
        assert!(conf.contains("map $http_upgrade $connection_upgrade"));
    }
    // Forge flavor: markers preserved, shared site.conf NOT included.
    assert!(forge.contains("# FORGE CONFIG (DO NOT REMOVE!)"));
    assert!(forge.contains("# FORGE SSL (DO NOT REMOVE!)"));
    assert!(forge.contains("forge-conf/12345/before/*"));
    // The shared site.conf must not be *included* (its location / collides);
    // it may be mentioned in an explanatory comment.
    assert!(!forge
        .lines()
        .any(|l| l.trim_start().starts_with("include") && l.contains("site.conf")));
}

#[test]
fn forge_pair_invariants() {
    let dir = tmpdir("forge");
    assert!(knishio()
        .args(["deploy", "forge", "--user", "forge", "--output", dir.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let deploy_script = std::fs::read_to_string(dir.join("forge-deploy-script.txt")).unwrap();
    // Forge template expansion breaks on non-ASCII / comments / long scripts —
    // validated live (a working upgrade was marked FAILED by a parse error).
    assert!(deploy_script.is_ascii());
    assert!(!deploy_script.contains('#'));
    assert!(deploy_script.lines().filter(|l| !l.trim().is_empty()).count() <= 6);
    assert!(deploy_script.contains("|| exit 1"));
    assert!(deploy_script.contains("$CREATE_RELEASE()"));
    assert!(deploy_script.contains("$ACTIVATE_RELEASE()"));

    let build = dir.join("knishio-deploy-build.sh");
    bash_n(&build);
    let b = std::fs::read_to_string(&build).unwrap();
    assert!(b.contains("cargo build --release --locked"));
    assert!(b.contains("CARGO_TARGET_DIR"));
    assert!(b.contains("/usr/local/bin/upgrade.sh"));
    assert!(b.contains("knishio-Cargo.lock.pinned"), "F-12 interim line expected without --lock-committed");

    // The pre-deploy gate must run BEFORE the binary swap. That ordering IS the
    // safety property: with `set -e`, a failed gate aborts before pg_dump, before
    // the swap and before the restart, so production keeps the running binary.
    // A gate placed after upgrade.sh would report failures on an already-deployed
    // build — worse than no gate, because it reads as protection.
    let gate = b
        .find("make deploy-gate")
        .expect("build script must invoke the validator's pre-deploy gate");
    let swap = b.find("/usr/local/bin/upgrade.sh").unwrap();
    assert!(gate < swap, "deploy-gate must precede upgrade.sh, not follow it");
    // Guarded with `make -n` so validator revisions predating the target skip the
    // gate with a notice instead of failing every deploy.
    assert!(
        b.contains("make -n deploy-gate"),
        "gate call must be guarded for older validator revisions"
    );
}

/// Remote targets must never infer accel from local hardware: the deploy /
/// verify / psql-ssh modules must not import the detection machinery.
#[test]
fn no_accel_detection_in_remote_modules() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for f in ["deploy/mod.rs", "deploy/ship.rs", "deploy/upgrade.rs", "deploy/ssh.rs", "verify.rs"] {
        let content = std::fs::read_to_string(src_root.join(f)).unwrap();
        assert!(
            !content.contains("detect::") && !content.contains("resolve_accel_and_files"),
            "{} must not use local hardware detection",
            f
        );
    }
}
