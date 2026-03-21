use crate::checkpoint;
use crate::keys;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Coin type for PIVX mainnet (BIP44)
const PIVX_COIN_TYPE: u32 = 119;

/// A serializable spendable note (mirrors pivx-shield-rust's JSSpendableNote)
#[derive(Clone, Serialize, Deserialize)]
pub struct SerializedNote {
    pub note: serde_json::Value, // sapling Note serialized as JSON
    pub witness: String,         // hex-encoded incremental witness
    pub nullifier: String,       // hex-encoded nullifier
    pub memo: Option<String>,
}

/// Persistent wallet state.
/// Sensitive fields (seed, mnemonic) are device-encrypted on disk and zeroized in memory on drop.
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct WalletData {
    #[zeroize(skip)]
    pub version: u32,
    /// 32-byte seed, device-encrypted on disk (NEVER output via CLI)
    seed: [u8; 32],
    /// Encoded extended full viewing key (not secret)
    #[zeroize(skip)]
    pub extfvk: String,
    /// Block height when the wallet was created (never changes)
    #[serde(default)]
    #[zeroize(skip)]
    pub birthday_height: i32,
    /// Last synced block height
    #[zeroize(skip)]
    pub last_block: i32,
    /// Hex-encoded Sapling commitment tree
    #[zeroize(skip)]
    pub commitment_tree: String,
    /// Spendable notes
    #[zeroize(skip)]
    pub unspent_notes: Vec<SerializedNote>,
    /// BIP39 mnemonic, device-encrypted on disk (NEVER output via CLI)
    mnemonic: String,
}

impl WalletData {
    /// Sum of all unspent note values in satoshis
    #[inline]
    pub fn get_balance(&self) -> u64 {
        self.unspent_notes
            .iter()
            .map(|n| {
                n.note
                    .get("value")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Derive the extended spending key on-the-fly from the stored seed
    pub fn derive_extsk(&self) -> Result<String, Box<dyn Error>> {
        let extsk = keys::spending_key_from_seed(&self.seed, PIVX_COIN_TYPE, 0)?;
        Ok(keys::encode_extsk(&extsk))
    }

    /// Get the mnemonic (for export only)
    pub fn get_mnemonic(&self) -> &str {
        &self.mnemonic
    }

    /// Mark notes as spent by removing those whose nullifiers match
    pub fn finalize_transaction(&mut self, spent_nullifiers: &[String]) {
        self.unspent_notes
            .retain(|n| !spent_nullifiers.contains(&n.nullifier));
    }
}

// ---------------------------------------------------------------------------
// Device-bound encryption
// ---------------------------------------------------------------------------

/// Derive a device-specific encryption key from machine ID + data directory path.
/// This key is deterministic on the same device but different on every other machine.
#[inline]
fn device_key() -> Result<[u8; 32], Box<dyn Error>> {
    let machine_id = machine_uid::get()
        .map_err(|_| "Failed to read machine ID")?;
    let mut hasher = Sha256::new();
    hasher.update(machine_id.as_bytes());
    hasher.update(get_data_dir().to_string_lossy().as_bytes());
    hasher.update(b"pivx-agent-kit-device-encryption");
    Ok(hasher.finalize().into())
}

/// SHA256-CTR stream cipher: XOR data with a keystream derived from the device key.
/// Symmetric — same function encrypts and decrypts.
#[inline]
fn device_crypt(data: &[u8], key: &[u8; 32]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut offset = 0;
    let mut counter = 0u64;

    while offset < data.len() {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(&counter.to_le_bytes());
        let block: [u8; 32] = hasher.finalize().into();

        let chunk_len = (data.len() - offset).min(32);
        for i in 0..chunk_len {
            result.push(data[offset + i] ^ block[i]);
        }
        offset += chunk_len;
        counter += 1;
    }
    result
}

/// Encrypt seed and mnemonic before serialization
fn encrypt_secrets(data: &mut WalletData) -> Result<(), Box<dyn Error>> {
    let key = device_key()?;

    // Encrypt seed (32 bytes → 32 bytes)
    let encrypted_seed = device_crypt(&data.seed, &key);
    data.seed.copy_from_slice(&encrypted_seed);

    // Encrypt mnemonic (string → hex-encoded encrypted bytes)
    let encrypted_mnemonic = device_crypt(data.mnemonic.as_bytes(), &key);
    data.mnemonic.zeroize();
    data.mnemonic = crate::simd::hex::bytes_to_hex_string(&encrypted_mnemonic);

    Ok(())
}

/// Decrypt seed and mnemonic after deserialization
fn decrypt_secrets(data: &mut WalletData) -> Result<(), Box<dyn Error>> {
    let key = device_key()?;

    // Decrypt seed
    let decrypted_seed = device_crypt(&data.seed, &key);
    data.seed.copy_from_slice(&decrypted_seed);

    // Decrypt mnemonic (hex → decrypt → string)
    let encrypted_bytes = crate::simd::hex::hex_string_to_bytes(&data.mnemonic);
    let decrypted_bytes = device_crypt(&encrypted_bytes, &key);
    data.mnemonic = String::from_utf8(decrypted_bytes)
        .map_err(|_| "Failed to decrypt wallet — wrong device?")?;

    // Verify decryption by re-deriving extfvk and comparing
    let extsk = keys::spending_key_from_seed(&data.seed, PIVX_COIN_TYPE, 0)?;
    let extfvk = keys::full_viewing_key(&extsk);
    let derived_extfvk = keys::encode_extfvk(&extfvk);
    if derived_extfvk != data.extfvk {
        return Err("Failed to decrypt wallet — wrong device or corrupted file.".into());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Data directory and paths
// ---------------------------------------------------------------------------

/// Get the data directory for wallet files
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

// ---------------------------------------------------------------------------
// Wallet creation
// ---------------------------------------------------------------------------

/// Create a brand new wallet from a fresh BIP39 mnemonic
pub fn create_new_wallet() -> Result<WalletData, Box<dyn Error>> {
    let mut entropy = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut entropy);
    let mnemonic = bip39::Mnemonic::from_entropy(&entropy)?;
    entropy.zeroize();
    create_wallet_from_mnemonic(&mnemonic.to_string())
}

/// Import a wallet from an existing BIP39 mnemonic phrase
pub fn import_wallet(mnemonic_str: &str) -> Result<WalletData, Box<dyn Error>> {
    let _ = bip39::Mnemonic::parse_normalized(mnemonic_str)
        .map_err(|e| format!("Invalid mnemonic: {}", e))?;
    create_wallet_from_mnemonic(mnemonic_str)
}

fn create_wallet_from_mnemonic(mnemonic_str: &str) -> Result<WalletData, Box<dyn Error>> {
    let mnemonic = bip39::Mnemonic::parse_normalized(mnemonic_str)
        .map_err(|e| format!("Invalid mnemonic: {}", e))?;

    let mut bip39_seed = mnemonic.to_seed("");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bip39_seed[..32]);
    bip39_seed.zeroize();

    let extsk = keys::spending_key_from_seed(&seed, PIVX_COIN_TYPE, 0)?;
    let extfvk = keys::full_viewing_key(&extsk);

    let block_count = crate::network::PivxNetwork::new()
        .get_block_count()
        .unwrap_or(0);
    let (checkpoint_height, commitment_tree) =
        checkpoint::get_checkpoint(block_count as i32);

    Ok(WalletData {
        version: 1,
        seed,
        extfvk: keys::encode_extfvk(&extfvk),
        birthday_height: checkpoint_height,
        last_block: checkpoint_height,
        commitment_tree: commitment_tree.to_string(),
        unspent_notes: vec![],
        mnemonic: mnemonic_str.to_string(),
    })
}

/// Reset wallet to its birthday checkpoint, clearing all sync state
pub fn reset_to_checkpoint(data: &mut WalletData) -> Result<(), Box<dyn Error>> {
    let birthday = if data.birthday_height > 0 {
        data.birthday_height
    } else {
        5_236_346
    };
    let (checkpoint_height, commitment_tree) =
        checkpoint::get_checkpoint(birthday);

    data.last_block = checkpoint_height;
    data.commitment_tree = commitment_tree.to_string();
    data.unspent_notes.clear();
    Ok(())
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Save wallet data to disk atomically via write-then-rename.
/// Seed and mnemonic are device-encrypted before writing.
pub fn save_wallet(data: &WalletData) -> Result<(), Box<dyn Error>> {
    let dir = get_data_dir();
    fs::create_dir_all(&dir)?;

    // Clone to encrypt without mutating the in-memory wallet
    let mut disk_data = WalletData {
        version: data.version,
        seed: data.seed,
        extfvk: data.extfvk.clone(),
        birthday_height: data.birthday_height,
        last_block: data.last_block,
        commitment_tree: data.commitment_tree.clone(),
        unspent_notes: data.unspent_notes.clone(),
        mnemonic: data.mnemonic.clone(),
    };
    encrypt_secrets(&mut disk_data)?;

    let path = wallet_path();
    let tmp_path = path.with_extension("json.tmp");

    let json = serde_json::to_string_pretty(&disk_data)?;
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
    let mut data: WalletData = serde_json::from_str(&json)?;
    decrypt_secrets(&mut data)?;
    Ok(data)
}
