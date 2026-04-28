//! Sapling proving parameter download + on-disk cache.
//!
//! The kit verifies hashes and parses bytes; this shim handles the network
//! fetch and filesystem cache that native consumers need.

use pivx_wallet_kit::sapling::prover::{verify_and_load_params, SaplingProver};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

static PROVER: OnceLock<SaplingProver> = OnceLock::new();

fn params_dir() -> PathBuf {
    crate::wallet::get_data_dir().join("params")
}

/// Load the prover from the on-disk cache, downloading if absent.
/// Idempotent — subsequent calls are no-ops.
pub fn ensure_prover_loaded() -> Result<(), Box<dyn Error>> {
    if PROVER.get().is_some() {
        return Ok(());
    }

    let output_path = params_dir().join("sapling-output.params");
    let spend_path = params_dir().join("sapling-spend.params");

    let (output_bytes, spend_bytes) = if output_path.exists() && spend_path.exists() {
        (fs::read(&output_path)?, fs::read(&spend_path)?)
    } else {
        eprintln!("Downloading sapling parameters (this may take a while)...");
        let (output, spend) = crate::network::download_sapling_params(|pct| {
            eprint!("\rDownloading sapling parameters: {:.0}%", pct * 100.0);
        })?;
        eprintln!();

        let dir = params_dir();
        fs::create_dir_all(&dir)?;
        fs::write(&output_path, &output)?;
        fs::write(&spend_path, &spend)?;

        (output, spend)
    };

    let loaded = verify_and_load_params(&output_bytes, &spend_bytes)?;
    let _ = PROVER.set(loaded);
    Ok(())
}

/// Get a reference to the loaded prover. Panics if [`ensure_prover_loaded`] hasn't been called.
pub fn get_prover() -> Result<&'static SaplingProver, Box<dyn Error>> {
    PROVER
        .get()
        .ok_or_else(|| "Prover not loaded. Call ensure_prover_loaded() first.".into())
}
