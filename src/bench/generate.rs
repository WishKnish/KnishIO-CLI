//! Benchmark plan generation — builds fully-signed, ContinuID-compliant
//! molecules into a SQLite plan file.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use knishio_client::{MetaItem, Molecule, Wallet};
use rusqlite::Connection;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{
    BENCH_META_TYPES, BENCH_RULE_TARGETS, BENCH_TOKEN_PREFIX, FIXTURE_CELL_SLUG,
    META_IDS_PER_TYPE,
};

// ═══════════════════════════════════════════════════════════════
// Public Args
// ═══════════════════════════════════════════════════════════════

/// Arguments for the `generate` subcommand.
pub struct GenerateArgs {
    pub identities: usize,
    pub types: Vec<String>,
    pub metas_per_identity: usize,
    pub transfers_per_identity: usize,
    pub rules_per_identity: usize,
    pub burns_per_identity: usize,
    pub token_amount: f64,
    pub output: String,
}

// ═══════════════════════════════════════════════════════════════
// TypeSet — parsed molecule type flags
// ═══════════════════════════════════════════════════════════════

const VALID_TYPES: &[&str] = &["meta", "value-transfer", "rule", "burn"];

struct TypeSet {
    has_meta: bool,
    has_value_transfer: bool,
    has_rule: bool,
    has_burn: bool,
    /// `meta` + `value-transfer` together: even identities run metas, odd ones run
    /// value transfers. One identity cannot interleave the two on a single chain:
    /// a value transfer must sign `atoms[0]` from the funded token wallet at the
    /// current ContinuID pointer (¶0074 Rule 3), and a meta molecule advances that
    /// pointer to a fresh USER position that holds no token balance — every
    /// transfer after the first meta was rejected "Wallet not found". Splitting by
    /// identity keeps both chains valid while concurrent identities still put
    /// mixed load on the validator.
    split_meta_value: bool,
}

impl TypeSet {
    fn from_args(types: &[String], identities: usize) -> Result<Self> {
        for t in types {
            if !VALID_TYPES.contains(&t.as_str()) {
                anyhow::bail!(
                    "Unknown molecule type '{}'. Valid types: {}",
                    t,
                    VALID_TYPES.join(", ")
                );
            }
        }

        let has_meta = types.iter().any(|t| t == "meta");
        let mut has_value_transfer = types.iter().any(|t| t == "value-transfer");
        let has_rule = types.iter().any(|t| t == "rule");
        let has_burn = types.iter().any(|t| t == "burn");

        if has_value_transfer && identities < 2 {
            eprintln!("WARNING: value-transfer requires at least 2 identities. Skipping value-transfer.");
            has_value_transfer = false;
        }

        Ok(TypeSet {
            has_meta,
            has_value_transfer,
            has_rule,
            has_burn,
            split_meta_value: has_meta && has_value_transfer,
        })
    }

    /// Which of meta / value-transfer identity `k` runs in phase 2.
    fn roles(&self, k: usize) -> (bool, bool) {
        if self.split_meta_value {
            let even = k.is_multiple_of(2);
            (even, !even)
        } else {
            (self.has_meta, self.has_value_transfer)
        }
    }

    /// Whether identity `k` needs a token minted in phase 1.
    fn identity_needs_token(&self, k: usize) -> bool {
        self.roles(k).1 || self.has_burn
    }

    /// (identities running metas, identities running value transfers).
    fn role_counts(&self, identities: usize) -> (usize, usize) {
        (0..identities).fold((0, 0), |(m, v), k| {
            let (rm, rv) = self.roles(k);
            (m + rm as usize, v + rv as usize)
        })
    }
}

// ═══════════════════════════════════════════════════════════════
// SQLite Schema
// ═══════════════════════════════════════════════════════════════

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS gen_report;
        DROP TABLE IF EXISTS molecules;
        DROP TABLE IF EXISTS identities;
        DROP TABLE IF EXISTS config;

        CREATE TABLE config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE identities (
            idx INTEGER PRIMARY KEY,
            secret TEXT NOT NULL,
            bundle TEXT NOT NULL
        );

        CREATE TABLE molecules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            identity_idx INTEGER NOT NULL,
            phase INTEGER NOT NULL,
            chain_order INTEGER NOT NULL,
            global_order INTEGER NOT NULL,
            mol_type TEXT NOT NULL,
            molecular_hash TEXT NOT NULL UNIQUE,
            payload_json TEXT NOT NULL,
            FOREIGN KEY (identity_idx) REFERENCES identities(idx)
        );

        CREATE INDEX idx_mol_phase ON molecules(phase, global_order);
        CREATE INDEX idx_mol_chain ON molecules(identity_idx, chain_order);

        CREATE TABLE gen_report (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            metric TEXT NOT NULL,
            value TEXT NOT NULL
        );
        ",
    )
    .context("Failed to initialize SQLite schema")?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Molecule Helpers
// ═══════════════════════════════════════════════════════════════

/// Extract the next ContinuID position from a molecule's remainder wallet.
fn advance_chain(mol: &Molecule) -> Result<String> {
    super::molecules::next_position(mol)
}

/// Generate a fresh random OTS position (64-char hex). KnishIO positions are
/// random (not sequential), so a throwaway wallet yields a unique position; the
/// signer re-derives the key from `secret + position + token` when signing.
/// Used to thread the value-transfer ContinuID/token chain (see `make_value_transfer`).
fn fresh_position(secret: &str) -> Result<String> {
    Wallet::create(Some(secret), None, "USER", None, None)
        .context("Failed to derive fresh position wallet")?
        .position
        .context("Derived wallet has no position")
}

/// Serialize a molecule to JSON string for storage.
fn mol_to_payload(mol: &Molecule) -> Result<(String, String)> {
    let mol_hash = mol
        .molecular_hash
        .clone()
        .context("Molecular hash missing")?;
    let mol_json = serde_json::to_value(mol).context("Failed to serialize molecule")?;
    let payload = serde_json::to_string(&mol_json).context("Failed to stringify molecule")?;
    Ok((mol_hash, payload))
}

/// Insert a molecule into the SQLite database.
fn insert_mol(
    conn: &Connection,
    identity_idx: usize,
    phase: i32,
    chain_order: usize,
    global_order: usize,
    mol_type: &str,
    mol_hash: &str,
    payload: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO molecules (identity_idx, phase, chain_order, global_order, mol_type, molecular_hash, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            identity_idx as i64,
            phase,
            chain_order as i64,
            global_order as i64,
            mol_type,
            mol_hash,
            payload
        ],
    )
    .with_context(|| format!("Failed to insert {mol_type} molecule"))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// Molecule Factories
// ═══════════════════════════════════════════════════════════════

/// Create an auth molecule (U+I). Auto-adds I-atom.
fn make_auth(secret: &str, bundle: &str) -> Result<Molecule> {
    super::molecules::make_auth(secret, bundle, FIXTURE_CELL_SLUG)
}

/// Create a meta molecule (M+I). Auto-adds I-atom.
/// `meta_type` is cycled from BENCH_META_TYPES for realistic bonding diversity.
/// The metaId carries the plan's `run_id`: meta ownership is global on the
/// validator (first writer's bundle), so two plans replayed into one database
/// without a purge in between must not share metaIds.
fn make_meta(
    secret: &str,
    bundle: &str,
    position: &str,
    identity_idx: usize,
    meta_idx: usize,
    meta_type: &str,
    run_id: u64,
) -> Result<Molecule> {
    let type_seq = meta_idx / BENCH_META_TYPES.len();
    let id_slot = type_seq % META_IDS_PER_TYPE;
    let meta_id = format!("bench-{meta_type}-{identity_idx}-{id_slot}-{run_id}");
    super::molecules::make_meta_custom(
        secret,
        bundle,
        position,
        FIXTURE_CELL_SLUG,
        meta_type,
        &meta_id,
        vec![
            MetaItem::new("iteration", meta_idx.to_string()),
            MetaItem::new("benchmark", "true"),
            MetaItem::new("identity", identity_idx.to_string()),
            MetaItem::new("metaType", meta_type),
        ],
    )
}

/// Create a token-create molecule (C + I) that mints the initial supply to a
/// token wallet AT `mint_position` and advances the bundle's ContinuID pointer to
/// that same `mint_position`.
///
/// This alignment is what makes a subsequent value transfer valid: per patent
/// ¶0074 Rule 3 the validator requires `atoms[0].position == current ContinuID
/// pointer`, and a value transfer signs `atoms[0]` from the funded token wallet.
/// By minting the supply to `mint_position` AND forcing the ContinuID I-atom
/// (USER remainder) to `mint_position`, the first transfer signs from
/// `mint_position` (where the balance lives) and matches the pointer. The C-atom
/// itself signs from `source_position` (the pre-create ContinuID pointer).
fn make_token_create(
    secret: &str,
    bundle: &str,
    source_position: &str,
    mint_position: &str,
    token_slug: &str,
    amount: f64,
) -> Result<Molecule> {
    let source_wallet = Wallet::create(Some(secret), None, "USER", Some(source_position), None)
        .context("Failed to create token-create wallet")?;

    // Mint the initial supply to the token wallet at `mint_position`.
    let recipient_wallet =
        Wallet::create(Some(secret), None, token_slug, Some(mint_position), None)
            .context("Failed to create token recipient wallet")?;

    // Force the ContinuID I-atom (USER remainder) to `mint_position` too, so the
    // post-create ContinuID pointer == the funded token wallet position.
    // `add_continuid_atom` keeps a remainder that is already a USER wallet.
    let continuid_remainder = Wallet::create(Some(secret), None, "USER", Some(mint_position), None)
        .context("Failed to create token-create ContinuID remainder wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        Some(continuid_remainder),
        Some(FIXTURE_CELL_SLUG.to_string()),
        None,
    );

    mol.init_token_creation(
        &recipient_wallet,
        amount,
        vec![
            MetaItem::new("name", format!("Bench Token {token_slug}")),
            MetaItem::new("fungibility", "fungible"),
            MetaItem::new("supply", "replenishable"),
            MetaItem::new("decimals", "0"),
        ],
    )
    .context("Failed to init token creation")?;

    mol.add_continuid_atom()
        .context("Failed to add ContinuID atom to token-create")?;

    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign token-create molecule")?;

    Ok(mol)
}

/// Create a token-request molecule (T + manual I).
fn make_token_request(
    secret: &str,
    bundle: &str,
    position: &str,
    token_slug: &str,
    amount: f64,
    identity_idx: usize,
) -> Result<Molecule> {
    let source_wallet = Wallet::create(Some(secret), None, "USER", Some(position), None)
        .context("Failed to create token-request wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        None,
        Some(FIXTURE_CELL_SLUG.to_string()),
        None,
    );

    let meta_id = format!("bench-request-{identity_idx}");
    mol.init_token_request(
        token_slug,
        amount,
        "tokenRequest",
        &meta_id,
        vec![
            MetaItem::new("reason", "benchmark distribution"),
            MetaItem::new("issuer", bundle),
        ],
        None,
    )
    .context("Failed to init token request")?;

    mol.add_continuid_atom()
        .context("Failed to add ContinuID atom to token-request")?;

    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign token-request molecule")?;

    Ok(mol)
}

/// Create a value-transfer molecule (V+V+V + I) that is ContinuID-chain-correct
/// and chainable.
///
/// `position` is BOTH the OTS signing position (atoms[0], the source V-atom) and
/// the funded token wallet position; it must equal the bundle's current ContinuID
/// pointer (patent ¶0074 Rule 3). `next_position` receives the **token remainder**
/// V-atom AND the **ContinuID I-atom** (USER) — placing both at the same position
/// so the next transfer signs from `next_position` (where the remainder balance now
/// lives) and again matches the advanced ContinuID pointer. This is the key to
/// chaining: the token-balance chain and the ContinuID chain stay unified.
fn make_value_transfer(
    secret: &str,
    bundle: &str,
    position: &str,
    next_position: &str,
    recipient_bundle: &str,
    token_slug: &str,
    amount: f64,
    source_balance: f64,
    transfer_idx: usize,
) -> Result<Molecule> {
    let mut source_wallet = Wallet::create(Some(secret), None, token_slug, Some(position), None)
        .context("Failed to create value-transfer source wallet")?;
    source_wallet.balance = source_balance.to_string();

    let recipient_wallet = Wallet::create(Some(recipient_bundle), None, token_slug, None, None)
        .context("Failed to create value-transfer recipient wallet")?;

    // Token remainder lands at `next_position` (same token). `init_value` reads
    // `self.remainder_wallet` for the remainder V-atom, so set it up front.
    let token_remainder = Wallet::create(Some(secret), None, token_slug, Some(next_position), None)
        .context("Failed to create value-transfer token-remainder wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        Some(token_remainder),
        Some(FIXTURE_CELL_SLUG.to_string()),
        None,
    );

    // I-atom previousPosition = the position being consumed (current ContinuID).
    mol.continuid_position = Some(position.to_string());

    mol.init_value(&recipient_wallet, amount)
        .with_context(|| {
            format!(
                "Failed to init value transfer #{transfer_idx} (amount={amount}, balance={source_balance})"
            )
        })?;

    // Re-point the remainder wallet to a USER wallet at `next_position` so the
    // ContinuID I-atom (which `add_continuid_atom` keeps when it is already USER)
    // advances the pointer to `next_position` — the SAME position the token
    // remainder just landed at. This keeps the next transfer's atoms[0].position
    // (== `next_position`) equal to both the ContinuID pointer and the balance.
    mol.remainder_wallet = Some(
        Wallet::create(Some(secret), None, "USER", Some(next_position), None)
            .context("Failed to create value-transfer ContinuID remainder wallet")?,
    );

    mol.add_continuid_atom()
        .context("Failed to add ContinuID atom to value-transfer")?;

    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign value-transfer molecule")?;

    Ok(mol)
}

/// Create a rule molecule. Uses init_meta with isotope R pathway. The rule id
/// carries the plan's `run_id` for the same reason as `make_meta`'s metaId.
fn make_rule(
    secret: &str,
    bundle: &str,
    position: &str,
    identity_idx: usize,
    rule_idx: usize,
    rule_target: &str,
    run_id: u64,
) -> Result<Molecule> {
    let source_wallet = Wallet::create(Some(secret), None, "USER", Some(position), None)
        .context("Failed to create rule wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        None,
        Some(FIXTURE_CELL_SLUG.to_string()),
        None,
    );

    let rule_id = format!("bench-rule-{rule_target}-{identity_idx}-{rule_idx}-{run_id}");
    mol.init_meta(
        vec![
            MetaItem::new("action", "reject"),
            MetaItem::new("condition", "amount > 999999"),
            MetaItem::new("benchmark", "true"),
            MetaItem::new("targetType", rule_target),
        ],
        rule_target,
        &rule_id,
        None,
    )
    .context("Failed to init rule")?;
    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign rule molecule")?;

    Ok(mol)
}

/// Create a burn molecule (V+V + manual I).
fn make_burn(
    secret: &str,
    bundle: &str,
    position: &str,
    token_slug: &str,
    amount: f64,
    source_balance: f64,
) -> Result<Molecule> {
    let mut source_wallet =
        Wallet::create(Some(secret), None, token_slug, Some(position), None)
            .context("Failed to create burn wallet")?;
    source_wallet.balance = source_balance.to_string();

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        None,
        Some(FIXTURE_CELL_SLUG.to_string()),
        None,
    );

    mol.burn_token(amount, None)
        .context("Failed to init burn")?;

    mol.add_continuid_atom()
        .context("Failed to add ContinuID atom to burn")?;

    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign burn molecule")?;

    Ok(mol)
}

// ═══════════════════════════════════════════════════════════════
// Generate Command
// ═══════════════════════════════════════════════════════════════

pub fn generate(args: GenerateArgs) -> Result<()> {
    let type_set = TypeSet::from_args(&args.types, args.identities)?;

    // Calculate expected molecule counts
    let auth_count = args.identities;
    let (meta_ids, vt_ids) = type_set.role_counts(args.identities);
    let token_creates = (0..args.identities)
        .filter(|&k| type_set.identity_needs_token(k))
        .count();
    // token-create for every identity that needs a token; token-request only on the
    // burn-only path. Value transfers skip the request (the C-isotope already funds
    // the creator and the request would advance ContinuID off the funded wallet).
    let token_requests = if type_set.has_value_transfer { 0 } else { token_creates };
    let setup_count = token_creates + token_requests;
    let test_count = {
        let mut c = 0usize;
        c += meta_ids * args.metas_per_identity;
        c += vt_ids * args.transfers_per_identity;
        if type_set.has_rule {
            c += args.identities * args.rules_per_identity;
        }
        if type_set.has_burn {
            c += args.identities * args.burns_per_identity;
        }
        c
    };
    let total = auth_count + setup_count + test_count;

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!(" KnishIO Benchmark Plan Generator");
    println!("═══════════════════════════════════════════════════════════════");
    println!(" Identities:          {}", args.identities);
    println!(" Molecule types:      {}", args.types.join(", "));
    if type_set.split_meta_value {
        println!("   split:             {meta_ids} identities run meta, {vt_ids} run value-transfer");
    }
    println!(" Phase 0 (auth):      {auth_count}");
    if setup_count > 0 {
        println!(" Phase 1 (setup):     {setup_count}");
        println!("   token-create:      {token_creates}");
        if token_requests > 0 {
            println!("   token-request:     {token_requests}");
        }
    }
    println!(" Phase 2 (test):      {test_count}");
    if type_set.has_meta {
        println!(
            "   meta:              {} ({} metaTypes: {})",
            meta_ids * args.metas_per_identity,
            BENCH_META_TYPES.len(),
            BENCH_META_TYPES.join(", "),
        );
    }
    if type_set.has_value_transfer {
        println!(
            "   value-transfer:    {}",
            vt_ids * args.transfers_per_identity
        );
    }
    if type_set.has_rule {
        println!(
            "   rule:              {} ({} targets: {})",
            args.identities * args.rules_per_identity,
            BENCH_RULE_TARGETS.len(),
            BENCH_RULE_TARGETS.join(", "),
        );
    }
    if type_set.has_burn {
        println!(
            "   burn:              {}",
            args.identities * args.burns_per_identity
        );
    }
    println!(" Total molecules:     {total}");
    println!(" Output:              {}", args.output);
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    // Open SQLite
    let conn =
        Connection::open(&args.output).context("Failed to open SQLite database")?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
    init_schema(&conn)?;

    // Store config
    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock before UNIX epoch")?
        .as_secs();
    let config_pairs = [
        ("identities", args.identities.to_string()),
        ("types", args.types.join(",")),
        ("metas_per_identity", args.metas_per_identity.to_string()),
        (
            "transfers_per_identity",
            args.transfers_per_identity.to_string(),
        ),
        ("rules_per_identity", args.rules_per_identity.to_string()),
        ("burns_per_identity", args.burns_per_identity.to_string()),
        ("token_amount", args.token_amount.to_string()),
        ("run_id", run_id.to_string()),
        ("generated_at", run_id.to_string()),
        ("generator_version", "1.0.0".to_string()),
        ("cell_slug", FIXTURE_CELL_SLUG.to_string()),
    ];
    for (k, v) in &config_pairs {
        conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)",
            [*k, v.as_str()],
        )
        .with_context(|| format!("Failed to insert config key '{k}'"))?;
    }

    // Progress bar
    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template(
            " {spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({per_sec}) {msg}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let total_start = Instant::now();
    let mut phase_timings: HashMap<&str, (usize, f64)> = HashMap::new();
    let mut type_timings: HashMap<String, (usize, f64)> = HashMap::new();
    let mut global_order: usize = 0;

    // Collect all bundles for value-transfer recipients
    let mut bundles: Vec<String> = Vec::with_capacity(args.identities);

    // First pass: generate identities and collect bundles
    for k in 0..args.identities {
        let secret = format!("bench-cli-identity-{k}-{run_id}");
        let auth_wallet = Wallet::create(Some(&secret), None, "AUTH", None, None)
            .context("Failed to create auth wallet for bundle lookup")?;
        let bundle = auth_wallet
            .bundle
            .clone()
            .context("Auth wallet has no bundle")?;
        bundles.push(bundle.clone());

        conn.execute(
            "INSERT INTO identities (idx, secret, bundle) VALUES (?1, ?2, ?3)",
            rusqlite::params![k as i64, &secret, &bundle],
        )
        .context("Failed to insert identity")?;
    }

    // Second pass: generate molecules for each identity
    for k in 0..args.identities {
        let secret = format!("bench-cli-identity-{k}-{run_id}");
        let bundle = &bundles[k];
        let token_slug = format!("{BENCH_TOKEN_PREFIX}{k}_{run_id}");
        let mut chain_order: usize = 0;

        // ── Phase 0: Auth ──
        let phase_start = Instant::now();
        let auth_mol = make_auth(&secret, bundle)?;
        let mut next_pos = advance_chain(&auth_mol)?;
        let (hash, payload) = mol_to_payload(&auth_mol)?;
        insert_mol(&conn, k, 0, chain_order, global_order, "auth", &hash, &payload)?;
        chain_order += 1;
        global_order += 1;
        pb.inc(1);
        pb.set_message(format!("identity {}/{}", k + 1, args.identities));

        let auth_elapsed = phase_start.elapsed().as_secs_f64();
        let e = phase_timings.entry("auth").or_insert((0, 0.0));
        e.0 += 1;
        e.1 += auth_elapsed;
        let e = type_timings.entry("auth".to_string()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += auth_elapsed;

        // ── Phase 1: Token setup (if needed) ──
        let (run_meta, run_vt) = type_set.roles(k);
        let mut token_balance = args.token_amount;

        if type_set.identity_needs_token(k) {
            // token-create: mint the supply to `mint_position` AND advance the
            // ContinuID pointer to `mint_position`, so a value transfer can sign
            // atoms[0] from the funded token wallet and satisfy ¶0074 Rule 3.
            let phase_start = Instant::now();
            let mint_position = fresh_position(&secret)?;
            let tc_mol = make_token_create(
                &secret,
                bundle,
                &next_pos,
                &mint_position,
                &token_slug,
                args.token_amount,
            )?;
            next_pos = mint_position; // ContinuID pointer now == the funded token wallet
            let (hash, payload) = mol_to_payload(&tc_mol)?;
            insert_mol(
                &conn,
                k,
                1,
                chain_order,
                global_order,
                "token-create",
                &hash,
                &payload,
            )?;
            chain_order += 1;
            global_order += 1;
            pb.inc(1);

            let tc_elapsed = phase_start.elapsed().as_secs_f64();
            let e = phase_timings.entry("setup").or_insert((0, 0.0));
            e.0 += 1;
            e.1 += tc_elapsed;
            let e = type_timings
                .entry("token-create".to_string())
                .or_insert((0, 0.0));
            e.0 += 1;
            e.1 += tc_elapsed;

            // token-request: legacy distribution step retained ONLY for burn-only
            // runs. For value transfers it is unnecessary (the C-isotope already
            // credited the creator's balance, c_isotope.rs) and would advance
            // ContinuID off the funded wallet, breaking the unified value chain.
            if !type_set.has_value_transfer {
                let phase_start = Instant::now();
                let tr_mol = make_token_request(
                    &secret,
                    bundle,
                    &next_pos,
                    &token_slug,
                    args.token_amount,
                    k,
                )?;
                next_pos = advance_chain(&tr_mol)?;
                let (hash, payload) = mol_to_payload(&tr_mol)?;
                insert_mol(
                    &conn,
                    k,
                    1,
                    chain_order,
                    global_order,
                    "token-request",
                    &hash,
                    &payload,
                )?;
                chain_order += 1;
                global_order += 1;
                pb.inc(1);

                let tr_elapsed = phase_start.elapsed().as_secs_f64();
                let e = phase_timings.entry("setup").or_insert((0, 0.0));
                e.0 += 1;
                e.1 += tr_elapsed;
                let e = type_timings
                    .entry("token-request".to_string())
                    .or_insert((0, 0.0));
                e.0 += 1;
                e.1 += tr_elapsed;
            }
        }

        // ── Phase 2: Test molecules ──
        let meta_count = if run_meta {
            args.metas_per_identity
        } else {
            0
        };
        let vt_count = if run_vt {
            args.transfers_per_identity
        } else {
            0
        };
        let max_interleave = std::cmp::max(meta_count, vt_count);

        let amount_per_transfer = if run_vt {
            let burn_budget = if type_set.has_burn {
                args.burns_per_identity
            } else {
                0
            };
            let divisor = args.transfers_per_identity + burn_budget + 1;
            (args.token_amount / divisor as f64).floor()
        } else {
            0.0
        };

        let mut meta_idx = 0usize;
        let mut vt_idx = 0usize;

        for i in 0..max_interleave {
            // Meta molecule
            if run_meta && i < meta_count {
                let start = Instant::now();
                let meta_type = BENCH_META_TYPES[(meta_idx + k) % BENCH_META_TYPES.len()];
                let mol = make_meta(&secret, bundle, &next_pos, k, meta_idx, meta_type, run_id)?;
                next_pos = advance_chain(&mol)?;
                let (hash, payload) = mol_to_payload(&mol)?;
                insert_mol(&conn, k, 2, chain_order, global_order, "meta", &hash, &payload)?;
                chain_order += 1;
                global_order += 1;
                meta_idx += 1;
                pb.inc(1);

                let elapsed = start.elapsed().as_secs_f64();
                let e = phase_timings.entry("test").or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
                let e = type_timings.entry("meta".to_string()).or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
            }

            // Value transfer molecule
            if run_vt && i < vt_count {
                let recipient_idx = (k + 1 + vt_idx) % args.identities;
                let recipient_bundle = &bundles[recipient_idx];

                let start = Instant::now();
                // Unified value chain: sign from `next_pos` (== current ContinuID
                // pointer == funded token wallet), and route BOTH the token remainder
                // and the ContinuID I-atom to a fresh `next` position so the next
                // transfer signs from where the remainder balance now lives.
                let next = fresh_position(&secret)?;
                let mol = make_value_transfer(
                    &secret,
                    bundle,
                    &next_pos,
                    &next,
                    recipient_bundle,
                    &token_slug,
                    amount_per_transfer,
                    token_balance,
                    vt_idx,
                )?;
                next_pos = next;
                token_balance -= amount_per_transfer;
                let (hash, payload) = mol_to_payload(&mol)?;
                insert_mol(
                    &conn,
                    k,
                    2,
                    chain_order,
                    global_order,
                    "value-transfer",
                    &hash,
                    &payload,
                )?;
                chain_order += 1;
                global_order += 1;
                vt_idx += 1;
                pb.inc(1);

                let elapsed = start.elapsed().as_secs_f64();
                let e = phase_timings.entry("test").or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
                let e = type_timings
                    .entry("value-transfer".to_string())
                    .or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
            }
        }

        // Rule molecules
        if type_set.has_rule {
            for r in 0..args.rules_per_identity {
                let start = Instant::now();
                let rule_target = BENCH_RULE_TARGETS[r % BENCH_RULE_TARGETS.len()];
                let mol = make_rule(&secret, bundle, &next_pos, k, r, rule_target, run_id)?;
                next_pos = advance_chain(&mol)?;
                let (hash, payload) = mol_to_payload(&mol)?;
                insert_mol(
                    &conn, k, 2, chain_order, global_order, "rule", &hash, &payload,
                )?;
                chain_order += 1;
                global_order += 1;
                pb.inc(1);

                let elapsed = start.elapsed().as_secs_f64();
                let e = phase_timings.entry("test").or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
                let e = type_timings.entry("rule".to_string()).or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
            }
        }

        // Burn molecules
        if type_set.has_burn && type_set.identity_needs_token(k) {
            let transfer_budget = if run_vt {
                args.transfers_per_identity as f64
            } else {
                0.0
            };
            let burn_amount =
                args.token_amount / (transfer_budget + args.burns_per_identity as f64 + 1.0);

            for _b in 0..args.burns_per_identity {
                let start = Instant::now();
                let mol = make_burn(
                    &secret,
                    bundle,
                    &next_pos,
                    &token_slug,
                    burn_amount,
                    token_balance,
                )?;
                next_pos = advance_chain(&mol)?;
                token_balance -= burn_amount;
                let (hash, payload) = mol_to_payload(&mol)?;
                insert_mol(
                    &conn, k, 2, chain_order, global_order, "burn", &hash, &payload,
                )?;
                chain_order += 1;
                global_order += 1;
                pb.inc(1);

                let elapsed = start.elapsed().as_secs_f64();
                let e = phase_timings.entry("test").or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
                let e = type_timings
                    .entry("burn".to_string())
                    .or_insert((0, 0.0));
                e.0 += 1;
                e.1 += elapsed;
            }
        }

        // Suppress unused variable warning when neither value-transfer nor burn
        let _ = next_pos;
        let _ = token_balance;
    }

    pb.finish_with_message("done");

    let total_elapsed = total_start.elapsed();

    // Store gen_report metrics
    for (mol_type, (count, secs)) in &type_timings {
        conn.execute(
            "INSERT INTO gen_report (metric, value) VALUES (?1, ?2)",
            [
                &format!("gen_{mol_type}_count"),
                &count.to_string(),
            ],
        )
        .with_context(|| format!("Failed to insert gen_report for {mol_type}"))?;
        conn.execute(
            "INSERT INTO gen_report (metric, value) VALUES (?1, ?2)",
            [
                &format!("gen_{mol_type}_secs"),
                &format!("{secs:.3}"),
            ],
        )
        .with_context(|| format!("Failed to insert gen_report secs for {mol_type}"))?;
    }
    conn.execute(
        "INSERT INTO gen_report (metric, value) VALUES ('gen_total_secs', ?1)",
        [&format!("{:.3}", total_elapsed.as_secs_f64())],
    )
    .context("Failed to insert gen_total_secs")?;

    // Verify counts
    let actual_total: i64 = conn
        .query_row("SELECT COUNT(*) FROM molecules", [], |r| r.get(0))
        .context("Failed to count molecules")?;

    // Print generation report
    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!(" Generation Complete");
    println!("═══════════════════════════════════════════════════════════════");

    if let Some((count, secs)) = phase_timings.get("auth") {
        let rate = *count as f64 / secs;
        println!(" Phase 0 (auth):      {count:>6} molecules  ({secs:.1}s, {rate:.1} mol/s)");
    }
    if let Some((count, secs)) = phase_timings.get("setup") {
        let rate = *count as f64 / secs;
        println!(" Phase 1 (setup):     {count:>6} molecules  ({secs:.1}s, {rate:.1} mol/s)");
        if let Some((c, _)) = type_timings.get("token-create") {
            println!("   token-create:      {c:>6}");
        }
        if let Some((c, _)) = type_timings.get("token-request") {
            println!("   token-request:     {c:>6}");
        }
    }
    if let Some((count, secs)) = phase_timings.get("test") {
        let rate = *count as f64 / secs;
        println!(" Phase 2 (test):      {count:>6} molecules  ({secs:.1}s, {rate:.1} mol/s)");
        for mol_type in &["meta", "value-transfer", "rule", "burn"] {
            if let Some((c, _)) = type_timings.get(*mol_type) {
                println!("   {mol_type:<18}{c:>6}");
            }
        }
    }

    let total_rate = actual_total as f64 / total_elapsed.as_secs_f64();
    println!();
    println!(
        " Total:               {actual_total:>6} molecules in {:.1}s ({total_rate:.1} mol/s)",
        total_elapsed.as_secs_f64()
    );
    println!(" Output:              {}", args.output);
    println!("═══════════════════════════════════════════════════════════════");

    if actual_total != total as i64 {
        anyhow::bail!(
            "Expected {total} molecules but generated {actual_total}. This is a bug."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `meta,value-transfer` plans: every value transfer must sign from the token
    /// wallet its chain predecessor funded — the token-create (which mints to the
    /// position it also moves the ContinuID pointer to) or the previous transfer
    /// (whose remainder V-atom lands at the next signing position). Interleaving
    /// metas on the same chain broke this: a meta moved the pointer to a position
    /// with no token balance, and the validator rejected every later transfer
    /// "Wallet not found".
    #[test]
    fn mixed_plan_keeps_every_value_chain_funded() {
        let dir = std::env::temp_dir().join(format!("knishio-gen-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.db");
        generate(GenerateArgs {
            identities: 4,
            types: vec!["meta".into(), "value-transfer".into()],
            metas_per_identity: 3,
            transfers_per_identity: 3,
            rules_per_identity: 0,
            burns_per_identity: 0,
            token_amount: 1000.0,
            output: plan.to_string_lossy().into_owned(),
        })
        .unwrap();

        let conn = Connection::open(&plan).unwrap();
        let mut stmt = conn
            .prepare("SELECT identity_idx, mol_type, payload_json FROM molecules ORDER BY identity_idx, chain_order")
            .unwrap();
        let rows: Vec<(i64, String, serde_json::Value)> = stmt
            .query_map([], |r| {
                let p: String = r.get(2)?;
                Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&p).unwrap()))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();

        let i_position = |m: &serde_json::Value| {
            m["atoms"].as_array().unwrap().iter().rev().find(|a| a["isotope"] == "I").map(|a| a["position"].clone())
        };
        let (mut metas, mut transfers) = (0, 0);
        for (i, (idx, mol_type, mol)) in rows.iter().enumerate() {
            match mol_type.as_str() {
                "meta" => metas += 1,
                "value-transfer" => {
                    transfers += 1;
                    let signer = &mol["atoms"][0];
                    let (prev_idx, prev_type, prev) = &rows[i - 1];
                    assert_eq!(prev_idx, idx, "a transfer cannot open an identity's chain");
                    let funded = match prev_type.as_str() {
                        // Mint position == the pointer the token-create set.
                        "token-create" => i_position(prev).as_ref() == Some(&signer["position"]),
                        // Remainder of the same token at the signing position.
                        "value-transfer" => prev["atoms"].as_array().unwrap().iter().any(|a| {
                            a["isotope"] == "V" && a["token"] == signer["token"] && a["position"] == signer["position"]
                        }),
                        _ => false,
                    };
                    assert!(
                        funded,
                        "identity {idx}: transfer signs from {}/{} after a {prev_type}, which funded no such wallet",
                        signer["token"], signer["position"]
                    );
                }
                _ => {}
            }
        }
        // Two meta identities x 3 and two value identities x 3.
        assert_eq!((metas, transfers), (6, 6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identity 0's secret, bundle and first signing position (after its auth
    /// molecule), derived exactly as `generate()` does for a plan's `run_id`.
    fn identity0(run_id: u64) -> (String, String, String) {
        let secret = format!("bench-cli-identity-0-{run_id}");
        let bundle = Wallet::create(Some(&secret), None, "AUTH", None, None)
            .unwrap()
            .bundle
            .unwrap();
        let pos = advance_chain(&make_auth(&secret, &bundle).unwrap()).unwrap();
        (secret, bundle, pos)
    }

    /// The id of a molecule's non-ContinuID atom (M for metas, the rule atom for rules).
    fn signed_id(mol: &Molecule) -> String {
        let v = serde_json::to_value(mol).unwrap();
        v["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["isotope"] != "I" && a["metaId"].is_string())
            .and_then(|a| a["metaId"].as_str())
            .expect("molecule has a metaId-bearing atom")
            .to_string()
    }

    /// Meta ownership on the validator is global (first writer's bundle), so two
    /// plans replayed into one database without a purge must not share a metaId.
    #[test]
    fn plans_from_different_runs_share_no_meta_id() {
        let ids: Vec<String> = [1u64, 2]
            .iter()
            .map(|&r| {
                let (secret, bundle, pos) = identity0(r);
                signed_id(&make_meta(&secret, &bundle, &pos, 0, 0, BENCH_META_TYPES[0], r).unwrap())
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert!(ids[0].ends_with("-1"), "{}", ids[0]);
        assert!(ids[1].ends_with("-2"), "{}", ids[1]);
    }

    /// Rule ids follow the same global-ownership rule as metaIds.
    #[test]
    fn plans_from_different_runs_share_no_rule_id() {
        let ids: Vec<String> = [1u64, 2]
            .iter()
            .map(|&r| {
                let (secret, bundle, pos) = identity0(r);
                signed_id(&make_rule(&secret, &bundle, &pos, 0, 0, BENCH_RULE_TARGETS[0], r).unwrap())
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert!(ids[0].ends_with("-1"), "{}", ids[0]);
        assert!(ids[1].ends_with("-2"), "{}", ids[1]);
    }
}
