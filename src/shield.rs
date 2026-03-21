//! Core Shield block handling and transaction building.
//! Adapted from pivx-shield-rust/src/transaction.rs for native Rust.

use crate::keys::{self, GenericAddress};
use crate::prover;
use crate::wallet::{SerializedNote, WalletData};
use incrementalmerkletree::frontier::CommitmentTree;
use incrementalmerkletree::witness::IncrementalWitness;
use pivx_client_backend::decrypt_transaction;
use pivx_client_backend::keys::UnifiedFullViewingKey;
use pivx_primitives::consensus::{BlockHeight, Network, NetworkConstants, MAIN_NETWORK};
use pivx_primitives::memo::MemoBytes;
use pivx_primitives::merkle_tree::{
    read_commitment_tree, read_incremental_witness, write_commitment_tree,
    write_incremental_witness,
};
use pivx_primitives::transaction::builder::{BuildConfig, Builder};
use pivx_primitives::transaction::components::transparent::builder::TransparentSigningSet;
use pivx_primitives::transaction::fees::fixed::FeeRule;
use pivx_primitives::transaction::Transaction;
use pivx_primitives::zip32::{AccountId, Scope};
use pivx_protocol::memo::Memo;
use pivx_protocol::value::Zatoshis;
use rand_core::OsRng;
use sapling::note::Note;
use sapling::{Anchor, Node, Nullifier, NullifierDerivingKey};
use std::collections::HashMap;
use std::error::Error;
use std::io::Cursor;
use std::str::FromStr;

pub const DEPTH: u8 = 32;

/// A note that can be spent (in-memory representation)
struct SpendableNote {
    note: Note,
    witness: IncrementalWitness<Node, DEPTH>,
    nullifier: String,
    memo: Option<String>,
}

impl SpendableNote {
    fn from_serialized(n: &SerializedNote) -> Result<SpendableNote, Box<dyn Error>> {
        let note: Note = serde_json::from_value(n.note.clone())?;
        let wit_bytes = crate::simd::hex::hex_string_to_bytes(&n.witness);
        let witness = read_incremental_witness(Cursor::new(wit_bytes))?;
        Ok(SpendableNote {
            note,
            witness,
            nullifier: n.nullifier.clone(),
            memo: n.memo.clone(),
        })
    }

    fn to_serialized(&self) -> Result<SerializedNote, Box<dyn Error>> {
        let mut buf = Vec::new();
        write_incremental_witness(&self.witness, &mut buf)?;
        Ok(SerializedNote {
            note: serde_json::to_value(&self.note)?,
            witness: crate::simd::hex::bytes_to_hex_string(&buf),
            nullifier: self.nullifier.clone(),
            memo: self.memo.clone(),
        })
    }
}

/// Result of processing blocks
pub struct HandleBlocksResult {
    /// Updated commitment tree (hex)
    pub commitment_tree: String,
    /// New unspent notes found
    pub new_notes: Vec<SerializedNote>,
    /// Updated existing notes (witnesses updated)
    pub updated_notes: Vec<SerializedNote>,
    /// Nullifiers found in transactions (potential spends)
    pub nullifiers: Vec<String>,
}

/// Block structure for processing — txs are raw bytes (no hex round-trip)
pub struct ShieldBlock {
    pub txs: Vec<Vec<u8>>,
}

/// Process a batch of blocks, decrypting notes and updating the commitment tree.
pub fn handle_blocks(
    tree_hex: &str,
    blocks: Vec<ShieldBlock>,
    enc_extfvk: &str,
    existing_notes: &[SerializedNote],
) -> Result<HandleBlocksResult, Box<dyn Error>> {
    let mut tree: CommitmentTree<Node, DEPTH> =
        read_commitment_tree(Cursor::new(crate::simd::hex::hex_string_to_bytes(tree_hex)))?;

    let extfvk = keys::decode_extfvk(enc_extfvk)?;
    let key = UnifiedFullViewingKey::from_sapling_extended_full_viewing_key(extfvk)
        .map_err(|_| "Failed to create unified full viewing key")?;

    let mut comp_notes: Vec<SpendableNote> = existing_notes
        .iter()
        .map(SpendableNote::from_serialized)
        .collect::<Result<Vec<_>, _>>()?;

    let mut new_notes: Vec<SpendableNote> = vec![];
    let mut nullifiers: Vec<String> = vec![];

    // Build the decryption key map once for all transactions
    let mut key_map = HashMap::new();
    key_map.insert(AccountId::default(), key.clone());
    let nullif_key = key
        .sapling()
        .ok_or("Cannot generate nullifier key")?
        .to_nk(Scope::External);

    for block in blocks {
        for tx_bytes in &block.txs {
            let tx_nullifiers = handle_transaction(
                &mut tree,
                tx_bytes,
                &key_map,
                &nullif_key,
                &mut comp_notes,
                &mut new_notes,
            )?;
            let tx_nullifier_strs: Vec<String> = tx_nullifiers
                .iter()
                .map(|n| crate::simd::hex::bytes_to_hex_string(&n.0))
                .collect();
            nullifiers.extend(tx_nullifier_strs);
        }
    }

    let updated_notes: Vec<SerializedNote> = comp_notes
        .into_iter()
        .map(|n| n.to_serialized())
        .collect::<Result<Vec<_>, _>>()?;

    let new_serialized: Vec<SerializedNote> = new_notes
        .into_iter()
        .map(|n| n.to_serialized())
        .collect::<Result<Vec<_>, _>>()?;

    let mut tree_buf = Vec::new();
    write_commitment_tree(&tree, &mut tree_buf)?;

    Ok(HandleBlocksResult {
        commitment_tree: crate::simd::hex::bytes_to_hex_string(&tree_buf),
        new_notes: new_serialized,
        updated_notes,
        nullifiers,
    })
}

/// Process a single transaction: update commitment tree, decrypt notes, track nullifiers.
#[inline]
fn handle_transaction(
    tree: &mut CommitmentTree<Node, DEPTH>,
    tx_bytes: &[u8],
    key_map: &HashMap<AccountId, UnifiedFullViewingKey>,
    nullif_key: &NullifierDerivingKey,
    existing_witnesses: &mut Vec<SpendableNote>,
    new_witnesses: &mut Vec<SpendableNote>,
) -> Result<Vec<Nullifier>, Box<dyn Error>> {
    let tx = Transaction::read(
        Cursor::new(tx_bytes),
        pivx_primitives::consensus::BranchId::Sapling,
    )?;

    let decrypted_tx =
        decrypt_transaction(&MAIN_NETWORK, BlockHeight::from_u32(320), &tx, key_map);

    let mut nullifiers: Vec<Nullifier> = vec![];

    if let Some(sapling) = tx.sapling_bundle() {
        for spend in sapling.shielded_spends() {
            nullifiers.push(*spend.nullifier());
        }

        for (i, out) in sapling.shielded_outputs().iter().enumerate() {
            tree.append(Node::from_cmu(out.cmu()))
                .map_err(|_| "Failed to add cmu to tree")?;

            for witness in existing_witnesses.iter_mut().chain(new_witnesses.iter_mut()) {
                witness
                    .witness
                    .append(Node::from_cmu(out.cmu()))
                    .map_err(|_| "Failed to add cmu to witness")?;
            }

            for output in decrypted_tx.sapling_outputs() {
                if output.index() == i {
                    let witness = IncrementalWitness::<Node, DEPTH>::from_tree(tree.clone());
                    let nullifier =
                        get_nullifier_from_note(nullif_key, output.note(), &witness)?;
                    let memo = Memo::from_bytes(output.memo().as_slice())
                        .map(|m| match m {
                            Memo::Text(t) => t.to_string(),
                            _ => String::new(),
                        })
                        .ok();

                    new_witnesses.push(SpendableNote {
                        note: output.note().clone(),
                        witness,
                        nullifier,
                        memo,
                    });
                    break;
                }
            }
        }
    }

    Ok(nullifiers)
}

#[inline]
fn get_nullifier_from_note(
    nullif_key: &NullifierDerivingKey,
    note: &Note,
    witness: &IncrementalWitness<Node, DEPTH>,
) -> Result<String, Box<dyn Error>> {
    let path = witness.path().ok_or("Cannot find witness path")?;
    Ok(crate::simd::hex::bytes_to_hex_string(
        &note.nf(nullif_key, path.position().into()).0,
    ))
}

/// Get the sapling root hash from a hex-encoded commitment tree, byte-reversed
/// to match the network's finalsaplingroot format.
pub fn get_sapling_root(tree_hex: &str) -> Result<String, Box<dyn Error>> {
    let tree: CommitmentTree<Node, DEPTH> =
        read_commitment_tree(Cursor::new(crate::simd::hex::hex_string_to_bytes(tree_hex)))?;
    let root_bytes = tree.root().to_bytes();
    // Reverse bytes to match PIVX node's finalsaplingroot endianness
    let reversed: Vec<u8> = root_bytes.iter().rev().cloned().collect();
    Ok(crate::simd::hex::bytes_to_hex_string(&reversed))
}

// ---------------------------------------------------------------------------
// Transaction building
// ---------------------------------------------------------------------------

#[inline]
fn fee_calculator(
    transparent_input_count: u64,
    transparent_output_count: u64,
    sapling_input_count: u64,
    sapling_output_count: u64,
) -> u64 {
    let fee_per_byte = 1000;
    fee_per_byte
        * (sapling_output_count * 948
            + sapling_input_count * 384
            + transparent_input_count * 150
            + transparent_output_count * 34
            + 85)
}

pub struct TransactionResult {
    pub txhex: String,
    pub nullifiers: Vec<String>,
    pub amount: u64,
    pub fee: u64,
}

/// Create a Shield transaction spending from notes.
pub fn create_shield_transaction(
    wallet: &mut WalletData,
    to_address: &str,
    amount: u64,
    memo: &str,
    block_height: u32,
) -> Result<TransactionResult, Box<dyn Error>> {
    let extsk = keys::decode_extsk(&wallet.derive_extsk()?)?;
    let network = Network::MainNetwork;

    let mut notes: Vec<(Note, String, bool)> = wallet
        .unspent_notes
        .iter()
        .map(|n| {
            let note: Note = serde_json::from_value(n.note.clone())?;
            let has_memo = n.memo.as_ref().is_some_and(|m| !m.is_empty());
            Ok((note, n.witness.clone(), has_memo))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    // Spend non-memo notes first, then by value ascending
    notes.sort_by_key(|(note, _, has_memo)| (*has_memo, note.value().inner()));

    let anchor = match notes.first() {
        Some((_, witness_hex, _)) => {
            let witness = read_incremental_witness::<Node, _, DEPTH>(Cursor::new(
                crate::simd::hex::hex_string_to_bytes(witness_hex),
            ))?;
            Anchor::from_bytes(witness.root().to_bytes())
                .into_option()
                .unwrap_or(Anchor::empty_tree())
        }
        None => return Err("No spendable notes available".into()),
    };

    let mut builder = Builder::new(
        network,
        BlockHeight::from_u32(block_height),
        BuildConfig::Standard {
            sapling_anchor: Some(anchor),
            orchard_anchor: None,
        },
    );
    let transparent_signing_set = TransparentSigningSet::new();

    let (transparent_output_count, sapling_output_count) =
        if to_address.starts_with(network.hrp_sapling_payment_address()) {
            (0u64, 2u64)
        } else {
            (1u64, 2u64)
        };

    let dfvk = extsk.to_diversifiable_full_viewing_key();
    let fvk = dfvk.fvk().clone();
    let nk = dfvk.to_nk(Scope::External);

    let mut total = 0u64;
    let mut nullifiers = vec![];
    let mut sapling_input_count = 0u64;
    let mut fee = 0u64;
    let mut amount = amount;

    for (note, witness_hex, _) in &notes {
        let witness = read_incremental_witness::<Node, _, DEPTH>(Cursor::new(
            crate::simd::hex::hex_string_to_bytes(witness_hex),
        ))?;
        builder
            .add_sapling_spend::<FeeRule>(
                fvk.clone(),
                note.clone(),
                witness.path().ok_or("Empty commitment tree")?,
            )
            .map_err(|_| "Failed to add sapling spend")?;

        let nullifier = note.nf(&nk, witness.witnessed_position().into());
        nullifiers.push(crate::simd::hex::bytes_to_hex_string(&nullifier.to_vec()));

        sapling_input_count += 1;
        fee = fee_calculator(
            0,
            transparent_output_count,
            sapling_input_count,
            sapling_output_count,
        );
        total += note.value().inner();
        if total >= amount + fee {
            break;
        }
    }

    if total < amount + fee {
        return Err(format!(
            "Not enough balance. Have: {} sat, need: {} sat (amount) + {} sat (fee)",
            total, amount, fee
        ).into());
    }

    let send_amount = Zatoshis::from_u64(amount).map_err(|_| "Invalid amount")?;
    let change_amount =
        Zatoshis::from_u64(total - amount - fee).map_err(|_| "Invalid change")?;

    let to = keys::decode_generic_address(to_address)?;
    match to {
        GenericAddress::Transparent(addr) => {
            builder
                .add_transparent_output(&addr, send_amount)
                .map_err(|e| format!("Failed to add transparent output: {:?}", e))?;
        }
        GenericAddress::Shield(addr) => {
            let memo_bytes = if memo.is_empty() {
                MemoBytes::empty()
            } else {
                Memo::from_str(memo)
                    .map_err(|e| format!("Invalid memo: {}", e))?
                    .encode()
            };
            builder
                .add_sapling_output::<FeeRule>(None, addr, send_amount, memo_bytes)
                .map_err(|_| "Failed to add sapling output")?;
        }
    }

    if change_amount.is_positive() {
        let extfvk = keys::decode_extfvk(&wallet.extfvk)?;
        let (_idx, change_addr) = extfvk
            .to_diversifiable_full_viewing_key()
            .default_address();
        builder
            .add_sapling_output::<FeeRule>(None, change_addr, change_amount, MemoBytes::empty())
            .map_err(|_| "Failed to add change output")?;
    }

    let prover = prover::get_prover()?;
    let result = builder.build(
        &transparent_signing_set,
        &[extsk],
        &[],
        OsRng,
        &prover.1,
        &prover.0,
        &FeeRule::non_standard(Zatoshis::from_u64(fee).map_err(|_| "Invalid fee")?),
    )?;

    let mut tx_hex = vec![];
    result.transaction().write(&mut tx_hex)?;

    Ok(TransactionResult {
        txhex: crate::simd::hex::bytes_to_hex_string(&tx_hex),
        nullifiers,
        amount,
        fee,
    })
}
