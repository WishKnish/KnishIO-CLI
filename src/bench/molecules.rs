//! Signed-molecule factories shared by bench (fixture generation) and
//! `verify --write-smoke` (live write-path acceptance). Cell slug is a
//! parameter — bench pins its BENCH_CLI_* fixture cell, verify targets an
//! existing open cell (default `public`).

use anyhow::{Context, Result};
use knishio_client::{MetaItem, Molecule, Wallet};

/// Extract the next ContinuID position from a molecule's remainder wallet.
pub(crate) fn next_position(mol: &Molecule) -> Result<String> {
    mol.remainder_wallet
        .as_ref()
        .context("Molecule has no remainder_wallet")?
        .position
        .clone()
        .context("Remainder wallet has no position")
}

/// Create an auth molecule (U+I). Auto-adds I-atom.
pub(crate) fn make_auth(secret: &str, bundle: &str, cell_slug: &str) -> Result<Molecule> {
    let auth_wallet = Wallet::create(Some(secret), None, "AUTH", None, None)
        .context("Failed to create auth wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(auth_wallet),
        None,
        Some(cell_slug.to_string()),
        None,
    );

    mol.init_authorization(vec![MetaItem::new("pubkey", "bench-pubkey")])
        .context("Failed to init authorization")?;
    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign auth molecule")?;

    Ok(mol)
}

/// Create a meta molecule (M+I) with caller-supplied meta payload.
pub(crate) fn make_meta_custom(
    secret: &str,
    bundle: &str,
    position: &str,
    cell_slug: &str,
    meta_type: &str,
    meta_id: &str,
    metas: Vec<MetaItem>,
) -> Result<Molecule> {
    let source_wallet = Wallet::create(Some(secret), None, "USER", Some(position), None)
        .context("Failed to create meta wallet")?;

    let mut mol = Molecule::with_params(
        Some(secret.to_string()),
        Some(bundle.to_string()),
        Some(source_wallet),
        None,
        Some(cell_slug.to_string()),
        None,
    );

    mol.init_meta(metas, meta_type, meta_id, None)
        .context("Failed to init meta")?;
    mol.sign(Some(bundle.to_string()), false, false)
        .context("Failed to sign meta molecule")?;

    Ok(mol)
}
