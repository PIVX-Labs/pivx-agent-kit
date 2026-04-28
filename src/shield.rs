//! Agent-kit adapters around `pivx-wallet-kit`'s transaction builders.
//!
//! The kit owns the pure builders (which take a loaded prover and a block
//! height as arguments). This shim fetches the prover from the local cache
//! and the height from the network before delegating.

use crate::prover;
use crate::wallet::WalletData;
use pivx_wallet_kit::sapling::builder as shield_builder;
use pivx_wallet_kit::transparent::builder as transparent_builder;
use std::error::Error;

// Re-export result types so existing callers keep working unchanged.
pub use pivx_wallet_kit::sapling::builder::TransactionResult;
pub use pivx_wallet_kit::transparent::builder::TransparentTransactionResult;

/// Build a shield transaction. Prover is loaded (downloading on first use) before delegation.
pub fn create_shield_transaction(
    wallet: &mut WalletData,
    to_address: &str,
    amount: u64,
    memo: &str,
    block_height: u32,
) -> Result<TransactionResult, Box<dyn Error>> {
    prover::ensure_prover_loaded()?;
    let prover = prover::get_prover()?;
    shield_builder::create_shield_transaction(
        wallet,
        to_address,
        amount,
        memo,
        block_height,
        prover,
    )
}

/// Build a raw v1 P2PKH transparent transaction. Shield destinations trigger
/// prover load + network height fetch internally — pure transparent sends
/// stay zero-RTT.
pub fn create_raw_transparent_transaction(
    wallet: &mut WalletData,
    bip39_seed: &[u8],
    to_address: &str,
    amount: u64,
) -> Result<TransparentTransactionResult, Box<dyn Error>> {
    // Only the shield-destination fallback needs a block height and a prover;
    // skipping these for pure transparent sends avoids an RPC round-trip and
    // potentially a multi-MB prover download on the hot path.
    let (block_height, prover) = if to_address.starts_with("ps") {
        let net = crate::network::PivxNetwork::new();
        let bh = net.get_block_count().unwrap_or(0) + 1;
        prover::ensure_prover_loaded()?;
        (bh, Some(prover::get_prover()?))
    } else {
        (0, None)
    };

    transparent_builder::create_raw_transparent_transaction(
        wallet,
        bip39_seed,
        to_address,
        amount,
        block_height,
        prover,
    )
}
