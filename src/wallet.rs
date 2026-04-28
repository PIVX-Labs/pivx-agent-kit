//! Filesystem persistence layer for `pivx-wallet-kit`'s `WalletData`.
//!
//! The kit owns the wallet state shape and all pure operations; this module
//! adds OS-specific I/O: atomic save, load, device-bound encryption key
//! derivation, and resolving the data directory.

use pivx_wallet_kit::wallet;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

// Re-export the kit types and pure functions so existing callsites
// (`crate::wallet::WalletData`, `crate::wallet::SerializedUTXO`, etc.) keep working.
pub use pivx_wallet_kit::wallet::{reset_to_checkpoint, WalletData};

/// Derive a device-specific encryption key from machine ID + data directory path.
fn device_key() -> Result<[u8; 32], Box<dyn Error>> {
    let machine_id = machine_uid::get().map_err(|_| "Failed to read machine ID")?;
    let mut hasher = Sha256::new();
    hasher.update(machine_id.as_bytes());
    hasher.update(get_data_dir().to_string_lossy().as_bytes());
    hasher.update(b"pivx-agent-kit-device-encryption");
    Ok(hasher.finalize().into())
}

/// Get the data directory for wallet files.
pub fn get_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pivx-agent-kit")
}

#[inline]
fn wallet_path() -> PathBuf {
    get_data_dir().join("wallet.json")
}

pub fn wallet_exists() -> bool {
    wallet_path().exists()
}

/// Create a brand-new wallet, fetching the current block height to pick the birthday checkpoint.
pub fn create_new_wallet() -> Result<WalletData, Box<dyn Error>> {
    let block_count = crate::network::PivxNetwork::new().get_block_count().unwrap_or(0);
    wallet::create_new_wallet(block_count)
}

/// Import a wallet from a mnemonic, fetching the current block height for birthday selection.
pub fn import_wallet(mnemonic_str: &str) -> Result<WalletData, Box<dyn Error>> {
    let block_count = crate::network::PivxNetwork::new().get_block_count().unwrap_or(0);
    wallet::import_wallet(mnemonic_str, block_count)
}

/// Save wallet data to disk atomically via write-then-rename.
/// Seed and mnemonic are device-encrypted before writing.
pub fn save_wallet(data: &WalletData) -> Result<(), Box<dyn Error>> {
    let dir = get_data_dir();
    fs::create_dir_all(&dir)?;

    let json = wallet::serialize_encrypted(data, &device_key()?)?;

    let path = wallet_path();
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
    }

    fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Load wallet data from disk and decrypt device-encrypted secrets.
pub fn load_wallet() -> Result<WalletData, Box<dyn Error>> {
    let path = wallet_path();
    if !path.exists() {
        return Err("No wallet found. Run 'init' first.".into());
    }
    let json = fs::read_to_string(&path)?;
    wallet::deserialize_encrypted(&json, &device_key()?)
}

