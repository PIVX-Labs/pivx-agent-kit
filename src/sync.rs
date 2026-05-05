//! Sync orchestrator — pairs the kit's pure block parser with network I/O.

use crate::network::PivxNetwork;
use crate::wallet::WalletData;
use pivx_wallet_kit::sapling::sync as shield_sync;
use pivx_wallet_kit::sapling::tree;
use pivx_wallet_kit::sync as kit_sync;
use pivx_wallet_kit::wallet as kit_wallet;
use std::error::Error;
use std::io::Read;

/// Save wallet every N batches during sync to preserve progress.
const SAVE_INTERVAL: u32 = 10;

/// Drive the kit's stream parser + block handler to completion, saving every
/// [`SAVE_INTERVAL`] batches.
fn sync_stream(
    reader: &mut dyn Read,
    wallet: &mut WalletData,
    total_blocks: &mut u32,
    batches_since_save: &mut u32,
) -> Result<(), Box<dyn Error>> {
    loop {
        let raw_blocks = match kit_sync::parse_next_blocks(reader, 10)? {
            Some(b) => b,
            None => break,
        };

        let shield_blocks: Vec<shield_sync::ShieldBlock> = raw_blocks
            .into_iter()
            .filter(|b| b.height as i32 > wallet.last_block)
            .collect();

        if shield_blocks.is_empty() {
            continue;
        }

        let last_height = shield_blocks
            .last()
            .map(|b| b.height)
            .unwrap_or(wallet.last_block as u32);
        let batch_len = shield_blocks.len() as u32;

        // Clone (not mem::take): if handle_blocks errors mid-batch we
        // do not want to strand the wallet with an empty note set —
        // the next sync attempt should resume from a coherent state.
        let existing = wallet.unspent_notes.clone();
        let result = shield_sync::handle_blocks(
            &wallet.commitment_tree,
            shield_blocks,
            &wallet.extfvk,
            existing,
        )?;

        wallet.commitment_tree = result.commitment_tree;

        let mut notes = result.updated_notes;
        notes.extend(result.new_notes);
        notes.retain(|n| !result.nullifiers.contains(&n.nullifier));
        wallet.unspent_notes = notes;

        wallet.last_block = last_height as i32;
        *total_blocks += batch_len;

        *batches_since_save += 1;
        if *batches_since_save >= SAVE_INTERVAL {
            crate::wallet::save_wallet(wallet)?;
            *batches_since_save = 0;
        }

        eprint!(".");
    }
    Ok(())
}

/// Sync shield data from the network into the wallet.
pub fn sync_shield(wallet: &mut WalletData, net: &PivxNetwork) -> Result<u32, Box<dyn Error>> {
    let start_block = (wallet.last_block + 1) as u32;
    let mut reader = net.get_shield_data(start_block)?;
    let mut total_blocks = 0u32;
    let mut batches_since_save = 0u32;

    sync_stream(&mut *reader, wallet, &mut total_blocks, &mut batches_since_save)?;

    if validate_sapling_root(wallet, net).is_err() {
        eprintln!("Sapling root mismatch detected. Auto-recovering...");
        crate::wallet::reset_to_checkpoint(wallet)?;
        crate::wallet::save_wallet(wallet)?;

        let start_block = (wallet.last_block + 1) as u32;
        let mut reader = net.get_shield_data(start_block)?;
        total_blocks = 0;
        batches_since_save = 0;

        sync_stream(&mut *reader, wallet, &mut total_blocks, &mut batches_since_save)?;

        if validate_sapling_root(wallet, net).is_err() {
            return Err(
                "Sapling root mismatch persists after recovery. RPC data may be compromised.".into(),
            );
        }
        eprintln!(" recovered.");
    }

    Ok(total_blocks)
}

#[inline]
fn validate_sapling_root(wallet: &WalletData, net: &PivxNetwork) -> Result<(), Box<dyn Error>> {
    let our_root = tree::get_sapling_root(&wallet.commitment_tree)?;
    let block = net.get_block(wallet.last_block as u32)?;
    let network_root = block
        .get("finalsaplingroot")
        .and_then(|r| r.as_str())
        .unwrap_or("");
    if !network_root.is_empty() && our_root != network_root {
        return Err("mismatch".into());
    }
    Ok(())
}

/// Sync transparent UTXOs for the wallet's transparent address.
pub fn sync_transparent(wallet: &mut WalletData, net: &PivxNetwork) -> Result<(), Box<dyn Error>> {
    let address = wallet.get_transparent_address()?;
    let raw_utxos = net.get_utxos(&address)?;
    wallet.unspent_utxos = kit_wallet::parse_blockbook_utxos(&raw_utxos);
    Ok(())
}
