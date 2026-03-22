//! Binary shield data stream parser and sync orchestrator.

use crate::network::PivxNetwork;
use crate::shield::{self, ShieldBlock};
use crate::wallet::WalletData;
use std::error::Error;
use std::io::Read;

/// Maximum packet size from the network (no single shield tx exceeds 1MB)
const MAX_PACKET_SIZE: usize = 1_048_576;

/// Save wallet every N batches during sync to preserve progress
const SAVE_INTERVAL: u32 = 10;

/// Parse a 4-byte little-endian length from a reader
#[inline]
fn read_u32_le(reader: &mut dyn Read) -> Result<Option<u32>, Box<dyn Error>> {
    let mut buf = [0u8; 4];
    match reader.read_exact(&mut buf) {
        Ok(()) => Ok(Some(u32::from_le_bytes(buf))),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

struct RawBlock {
    height: u32,
    txs: Vec<Vec<u8>>,
}

/// Parse the next batch of blocks from the binary stream.
fn parse_next_blocks(
    reader: &mut dyn Read,
    max_blocks: usize,
) -> Result<Option<Vec<RawBlock>>, Box<dyn Error>> {
    let mut txs: Vec<Vec<u8>> = vec![];
    let mut blocks: Vec<RawBlock> = vec![];

    while blocks.len() < max_blocks {
        let length = match read_u32_le(reader)? {
            Some(l) => l as usize,
            None => break,
        };

        if length > MAX_PACKET_SIZE {
            return Err(format!("Packet too large: {} bytes (max {})", length, MAX_PACKET_SIZE).into());
        }
        if length == 0 {
            return Err("Zero-length packet in shield binary stream".into());
        }

        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload)?;

        match payload[0] {
            0x5d => {
                if payload.len() < 9 {
                    return Err(format!("Block header too short: {} bytes (need 9)", payload.len()).into());
                }
                let height = u32::from_le_bytes(payload[1..5].try_into()?);
                blocks.push(RawBlock {
                    height,
                    txs: std::mem::take(&mut txs),
                });
            }
            0x03 => {
                txs.push(payload);
            }
            other => {
                return Err(format!("Unknown packet type 0x{:02x} in shield binary stream", other).into());
            }
        }
    }

    if blocks.is_empty() { Ok(None) } else { Ok(Some(blocks)) }
}

/// Process blocks from a stream reader into the wallet state.
fn sync_stream(
    reader: &mut dyn Read,
    wallet: &mut WalletData,
    total_blocks: &mut u32,
    batches_since_save: &mut u32,
) -> Result<(), Box<dyn Error>> {
    loop {
        let raw_blocks = match parse_next_blocks(reader, 10)? {
            Some(b) => b,
            None => break,
        };

        let shield_blocks: Vec<ShieldBlock> = raw_blocks
            .iter()
            .filter(|b| b.height as i32 > wallet.last_block)
            .map(|b| ShieldBlock { txs: b.txs.clone() })
            .collect();

        if shield_blocks.is_empty() {
            if let Some(last) = raw_blocks.last() {
                wallet.last_block = last.height as i32;
            }
            continue;
        }

        let result = shield::handle_blocks(
            &wallet.commitment_tree,
            shield_blocks,
            &wallet.extfvk,
            &wallet.unspent_notes,
        )?;

        wallet.commitment_tree = result.commitment_tree;

        let mut notes = result.updated_notes;
        notes.extend(result.new_notes);
        notes.retain(|n| !result.nullifiers.contains(&n.nullifier));
        wallet.unspent_notes = notes;

        if let Some(last) = raw_blocks.last() {
            wallet.last_block = last.height as i32;
            *total_blocks += raw_blocks.len() as u32;
        }

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
pub fn sync_shield(
    wallet: &mut WalletData,
    net: &PivxNetwork,
) -> Result<u32, Box<dyn Error>> {
    let start_block = (wallet.last_block + 1) as u32;
    let mut reader = net.get_shield_data(start_block)?;
    let mut total_blocks = 0u32;
    let mut batches_since_save = 0u32;

    sync_stream(&mut *reader, wallet, &mut total_blocks, &mut batches_since_save)?;

    // Validate sapling root — auto-heal if corrupted
    if let Err(_) = validate_sapling_root(wallet, net) {
        eprintln!("Sapling root mismatch detected. Auto-recovering...");
        crate::wallet::reset_to_checkpoint(wallet)?;
        crate::wallet::save_wallet(wallet)?;

        let start_block = (wallet.last_block + 1) as u32;
        let mut reader = net.get_shield_data(start_block)?;
        total_blocks = 0;
        batches_since_save = 0;

        sync_stream(&mut *reader, wallet, &mut total_blocks, &mut batches_since_save)?;

        // Verify recovery succeeded
        if let Err(_) = validate_sapling_root(wallet, net) {
            return Err("Sapling root mismatch persists after recovery. RPC data may be compromised.".into());
        }

        eprintln!(" recovered.");
    }

    Ok(total_blocks)
}

#[inline]
fn validate_sapling_root(
    wallet: &WalletData,
    net: &PivxNetwork,
) -> Result<(), Box<dyn Error>> {
    let our_root = crate::shield::get_sapling_root(&wallet.commitment_tree)?;
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
