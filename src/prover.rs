//! Sapling proving parameter management.
//! Downloads, caches, and loads the Groth16 parameters needed for proof generation.

use sapling::circuit::{OutputParameters, SpendParameters};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

pub type ImplTxProver = (OutputParameters, SpendParameters);

static PROVER: OnceLock<ImplTxProver> = OnceLock::new();

const OUTPUT_PARAMS_SHA256: &str =
    "2f0ebbcbb9bb0bcffe95a397e7eba89c29eb4dde6191c339db88570e3f3fb0e4";
const SPEND_PARAMS_SHA256: &str =
    "8e48ffd23abb3a5fd9c5589204f32d9c31285a04b78096ba40a79b75677efc13";

fn params_dir() -> PathBuf {
    crate::wallet::get_data_dir().join("params")
}

fn sha256_hex(data: &[u8]) -> String {
    crate::simd::hex::bytes_to_hex_string(&Sha256::digest(data))
}

/// Ensure the prover is loaded (from cache or download)
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

    if sha256_hex(&output_bytes) != OUTPUT_PARAMS_SHA256 {
        return Err("SHA256 mismatch for sapling output parameters".into());
    }
    if sha256_hex(&spend_bytes) != SPEND_PARAMS_SHA256 {
        return Err("SHA256 mismatch for sapling spend parameters".into());
    }

    let output_params = OutputParameters::read(&output_bytes[..], false)?;
    let spend_params = SpendParameters::read(&spend_bytes[..], false)?;

    let _ = PROVER.set((output_params, spend_params));
    Ok(())
}

/// Get a reference to the loaded prover
pub fn get_prover() -> Result<&'static ImplTxProver, Box<dyn Error>> {
    PROVER
        .get()
        .ok_or_else(|| "Prover not loaded. Call ensure_prover_loaded() first.".into())
}
