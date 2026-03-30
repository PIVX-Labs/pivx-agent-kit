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
            + transparent_input_count * 180
            + transparent_output_count * 34
            + 100)
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
    let amount = amount;

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

// ---------------------------------------------------------------------------
// Transparent transaction building
// ---------------------------------------------------------------------------

use zcash_transparent::bundle::OutPoint;

/// Build and sign a transparent transaction spending UTXOs.
pub fn create_transparent_transaction(
    wallet: &mut WalletData,
    bip39_seed: &[u8],
    to_address: &str,
    amount: u64,
    block_height: u32,
) -> Result<TransparentTransactionResult, Box<dyn Error>> {
    let network = MAIN_NETWORK;

    // Derive transparent private key at m/44'/119'/0'/0/0
    let (own_address, pubkey_bytes, privkey_bytes) =
        keys::transparent_key_from_bip39_seed(bip39_seed, 0, 0)?;

    // Build secp256k1 key pair (v0.29 — matches librustpivx)
    let secp = secp256k1::Secp256k1::new();
    let sk = secp256k1::SecretKey::from_slice(&privkey_bytes)
        .map_err(|e| format!("Invalid private key: {e}"))?;
    let pk = secp256k1::PublicKey::from_secret_key(&secp, &sk);

    // Get the P2PKH script for our own address
    let own_transparent = keys::decode_generic_address(&own_address)?;
    let own_script = match &own_transparent {
        GenericAddress::Transparent(addr) => addr.script(),
        _ => return Err("Own address is not transparent".into()),
    };

    // Sort UTXOs by amount descending (spend large ones first)
    let mut utxos = wallet.unspent_utxos.clone();
    utxos.sort_by(|a, b| b.amount.cmp(&a.amount));

    if utxos.is_empty() {
        return Err("No transparent UTXOs available".into());
    }

    // Destination
    let to = keys::decode_generic_address(to_address)?;
    let is_shield_dest = matches!(to, GenericAddress::Shield(_));

    // Calculate output counts for fee estimation
    let transparent_output_count: u64 = if is_shield_dest { 0 } else { 2 }; // dest + change
    let sapling_output_count: u64 = if is_shield_dest { 2 } else { 0 }; // dest + change (shield)

    // Select UTXOs
    let mut selected: Vec<crate::wallet::SerializedUTXO> = Vec::new();
    let mut total: u64 = 0;
    let mut fee: u64 = 0;

    for utxo in &utxos {
        selected.push(utxo.clone());
        total += utxo.amount;

        fee = fee_calculator(
            selected.len() as u64,
            transparent_output_count,
            0, // no sapling inputs
            sapling_output_count,
        );

        if total >= amount + fee {
            break;
        }
    }

    if total < amount + fee {
        return Err(format!(
            "Insufficient public balance. Have: {} sat, need: {} sat (amount) + {} sat (fee)",
            total, amount, fee
        ).into());
    }

    let change = total - amount - fee;

    // Build transaction — need sapling anchor if destination is shielded
    let sapling_anchor = if is_shield_dest {
        // Get anchor from wallet's commitment tree
        if !wallet.commitment_tree.is_empty() && wallet.commitment_tree != "00" {
            let tree: CommitmentTree<Node, DEPTH> = read_commitment_tree(Cursor::new(
                crate::simd::hex::hex_string_to_bytes(&wallet.commitment_tree),
            ))?;
            Some(Anchor::from_bytes(tree.root().to_bytes())
                .into_option()
                .unwrap_or(Anchor::empty_tree()))
        } else {
            Some(Anchor::empty_tree())
        }
    } else {
        None
    };

    let mut builder = Builder::new(
        network,
        BlockHeight::from_u32(block_height),
        BuildConfig::Standard {
            sapling_anchor,
            orchard_anchor: None,
        },
    );

    let mut signing_set = TransparentSigningSet::new();
    let builder_pk = signing_set.add_key(sk);

    // Add inputs
    for utxo in &selected {
        let mut txid_bytes = crate::simd::hex::hex_string_to_bytes(&utxo.txid);
        txid_bytes.reverse(); // txid is displayed in reverse byte order
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&txid_bytes);
        let outpoint = OutPoint::new(hash, utxo.vout);

        let txout = zcash_transparent::bundle::TxOut {
            value: pivx_primitives::transaction::components::amount::NonNegativeAmount::from_u64(utxo.amount)
                .map_err(|_| "Invalid amount")?,
            script_pubkey: own_script.clone(),
        };

        builder.add_transparent_input(builder_pk, outpoint, txout)
            .map_err(|e| format!("Failed to add transparent input: {:?}", e))?;
    }

    // Add output
    let send_amount = Zatoshis::from_u64(amount).map_err(|_| "Invalid amount")?;
    match to {
        GenericAddress::Transparent(addr) => {
            builder.add_transparent_output(&addr, send_amount)
                .map_err(|e| format!("Failed to add output: {:?}", e))?;
        }
        GenericAddress::Shield(addr) => {
            // Transparent → Shield requires sapling prover
            crate::prover::ensure_prover_loaded()?;
            builder.add_sapling_output::<FeeRule>(None, addr, send_amount, MemoBytes::empty())
                .map_err(|_| "Failed to add shield output")?;
        }
    }

    // Add change (back to own transparent address)
    if change > 0 {
        let change_amount = Zatoshis::from_u64(change).map_err(|_| "Invalid change")?;
        if let GenericAddress::Transparent(addr) = &own_transparent {
            builder.add_transparent_output(addr, change_amount)
                .map_err(|e| format!("Failed to add change: {:?}", e))?;
        }
    }

    // Build and sign
    let prover = if is_shield_dest {
        let p = prover::get_prover()?;
        Some(p)
    } else {
        None
    };

    let (output_prover, spend_prover) = match &prover {
        Some((o, s)) => (o, s),
        None => {
            // Dummy provers for transparent-only tx
            // We need references but they won't actually be used
            // Use ensure_prover_loaded and get_prover for safety
            crate::prover::ensure_prover_loaded()?;
            let p = prover::get_prover()?;
            // Leak is safe here — provers are static-lifetime cached
            let leaked = Box::leak(Box::new(p));
            (&leaked.0, &leaked.1)
        }
    };

    let fee_rule = FeeRule::non_standard(
        Zatoshis::from_u64(fee).map_err(|_| "Invalid fee")?
    );
    let result = builder.build(
        &signing_set,
        &[], // no sapling spending keys
        &[], // no orchard spending keys
        OsRng,
        spend_prover,
        output_prover,
        &fee_rule,
    )?;

    let mut tx_hex = vec![];
    result.transaction().write(&mut tx_hex)?;

    // Collect spent UTXO identifiers
    let spent: Vec<(String, u32)> = selected.iter()
        .map(|u| (u.txid.clone(), u.vout))
        .collect();

    Ok(TransparentTransactionResult {
        txhex: crate::simd::hex::bytes_to_hex_string(&tx_hex),
        spent,
        amount,
        fee,
    })
}

pub struct TransparentTransactionResult {
    pub txhex: String,
    pub spent: Vec<(String, u32)>,
    pub amount: u64,
    pub fee: u64,
}

// ---------------------------------------------------------------------------
// Raw v1 transparent transaction builder (bypasses librustpivx builder)
// ---------------------------------------------------------------------------

/// Build a raw v1 transparent transaction, signed with secp256k1.
/// This avoids the v3/Sapling format that PIVX nodes reject for pure transparent txs.
pub fn create_raw_transparent_transaction(
    wallet: &mut WalletData,
    bip39_seed: &[u8],
    to_address: &str,
    amount: u64,
) -> Result<TransparentTransactionResult, Box<dyn Error>> {
    // Shield destination → use the v3 builder (supports sapling outputs with transparent inputs)
    if to_address.starts_with(MAIN_NETWORK.hrp_sapling_payment_address()) {
        return create_transparent_transaction(wallet, bip39_seed, to_address, amount,
            crate::network::PivxNetwork::new().get_block_count()? + 1);
    }

    // Derive transparent key at m/44'/119'/0'/0/0
    let (own_address, pubkey_bytes, privkey_bytes) =
        keys::transparent_key_from_bip39_seed(bip39_seed, 0, 0)?;

    // Build the destination scriptPubKey
    let to_script = address_to_p2pkh_script(to_address)?;
    let own_script = address_to_p2pkh_script(&own_address)?;

    // Sort UTXOs by amount descending
    let mut utxos = wallet.unspent_utxos.clone();
    utxos.sort_by(|a, b| b.amount.cmp(&a.amount));
    if utxos.is_empty() {
        return Err("No transparent UTXOs available".into());
    }

    // Select UTXOs
    let mut selected: Vec<crate::wallet::SerializedUTXO> = Vec::new();
    let mut total: u64 = 0;

    for utxo in &utxos {
        selected.push(utxo.clone());
        total += utxo.amount;

        // Estimate fee: 10 sat/byte, ~150 bytes per input + ~34 per output + ~10 overhead
        let est_size = selected.len() * 150 + 2 * 34 + 10;
        let fee = (est_size as u64) * 10;

        if total >= amount + fee {
            break;
        }
    }

    // Final fee calculation
    let est_size = selected.len() * 150 + 2 * 34 + 10;
    let fee = (est_size as u64) * 10;

    if total < amount + fee {
        return Err(format!(
            "Insufficient public balance. Have: {} sat, need: {} sat + {} sat fee",
            total, amount, fee
        ).into());
    }

    let change = total - amount - fee;

    // Build unsigned transaction
    let secp = secp256k1::Secp256k1::new();
    let sk = secp256k1::SecretKey::from_slice(&privkey_bytes)
        .map_err(|e| format!("Invalid private key: {e}"))?;

    // --- Construct raw transaction bytes ---
    let mut tx = Vec::new();

    // Version (1)
    tx.extend_from_slice(&1u32.to_le_bytes());

    // Input count
    write_varint(&mut tx, selected.len() as u64);

    // Inputs (unsigned — scriptSig will be filled after signing)
    for utxo in &selected {
        let mut txid_bytes = crate::simd::hex::hex_string_to_bytes(&utxo.txid);
        txid_bytes.reverse(); // Internal byte order
        tx.extend_from_slice(&txid_bytes);
        tx.extend_from_slice(&utxo.vout.to_le_bytes());
        // Placeholder scriptSig (will be replaced per-input during signing)
        tx.push(0x00); // scriptSig length = 0
        tx.extend_from_slice(&0xffffffffu32.to_le_bytes()); // sequence
    }

    // Output count
    let output_count = if change > 0 { 2u64 } else { 1u64 };
    write_varint(&mut tx, output_count);

    // Output 1: destination
    tx.extend_from_slice(&amount.to_le_bytes());
    write_varint(&mut tx, to_script.len() as u64);
    tx.extend_from_slice(&to_script);

    // Output 2: change (if any)
    if change > 0 {
        tx.extend_from_slice(&change.to_le_bytes());
        write_varint(&mut tx, own_script.len() as u64);
        tx.extend_from_slice(&own_script);
    }

    // Locktime
    tx.extend_from_slice(&0u32.to_le_bytes());

    // --- Sign each input (SIGHASH_ALL) ---
    let mut signed_tx = Vec::new();
    signed_tx.extend_from_slice(&1u32.to_le_bytes()); // version
    write_varint(&mut signed_tx, selected.len() as u64);

    for (input_idx, utxo) in selected.iter().enumerate() {
        let mut txid_bytes = crate::simd::hex::hex_string_to_bytes(&utxo.txid);
        txid_bytes.reverse();
        signed_tx.extend_from_slice(&txid_bytes);
        signed_tx.extend_from_slice(&utxo.vout.to_le_bytes());

        // Build sighash: serialize tx with THIS input's scriptPubKey and others blanked
        let sighash = compute_sighash(&selected, &own_script, input_idx, amount, change, &to_script);

        // Sign with ECDSA
        let msg = secp256k1::Message::from_digest(sighash);
        let sig = secp.sign_ecdsa(&msg, &sk);
        let mut sig_bytes = sig.serialize_der().to_vec();
        sig_bytes.push(0x01); // SIGHASH_ALL

        // ScriptSig: [sig_len][signature][pubkey_len][pubkey]
        let script_sig_len = sig_bytes.len() + pubkey_bytes.len() + 2;
        write_varint(&mut signed_tx, script_sig_len as u64);
        signed_tx.push(sig_bytes.len() as u8);
        signed_tx.extend_from_slice(&sig_bytes);
        signed_tx.push(pubkey_bytes.len() as u8);
        signed_tx.extend_from_slice(&pubkey_bytes);

        signed_tx.extend_from_slice(&0xffffffffu32.to_le_bytes());
    }

    // Outputs
    write_varint(&mut signed_tx, output_count);
    signed_tx.extend_from_slice(&amount.to_le_bytes());
    write_varint(&mut signed_tx, to_script.len() as u64);
    signed_tx.extend_from_slice(&to_script);
    if change > 0 {
        signed_tx.extend_from_slice(&change.to_le_bytes());
        write_varint(&mut signed_tx, own_script.len() as u64);
        signed_tx.extend_from_slice(&own_script);
    }

    // Locktime
    signed_tx.extend_from_slice(&0u32.to_le_bytes());

    let spent: Vec<(String, u32)> = selected.iter()
        .map(|u| (u.txid.clone(), u.vout))
        .collect();

    Ok(TransparentTransactionResult {
        txhex: crate::simd::hex::bytes_to_hex_string(&signed_tx),
        spent,
        amount,
        fee,
    })
}

/// Decode a PIVX transparent address to its P2PKH scriptPubKey
fn address_to_p2pkh_script(address: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let decoded = bs58::decode(address).into_vec()
        .map_err(|e| format!("Invalid base58 address: {e}"))?;
    if decoded.len() != 25 {
        return Err("Invalid address length".into());
    }
    // decoded: [prefix_byte][20-byte-hash][4-byte-checksum]
    let pkh = &decoded[1..21];
    // P2PKH: OP_DUP OP_HASH160 <20-byte-hash> OP_EQUALVERIFY OP_CHECKSIG
    let mut script = vec![0x76, 0xa9, 0x14];
    script.extend_from_slice(pkh);
    script.push(0x88); // OP_EQUALVERIFY
    script.push(0xac); // OP_CHECKSIG
    Ok(script)
}

/// Compute SIGHASH_ALL for a specific input
fn compute_sighash(
    inputs: &[crate::wallet::SerializedUTXO],
    own_script: &[u8],
    signing_index: usize,
    amount: u64,
    change: u64,
    to_script: &[u8],
) -> [u8; 32] {
    let mut preimage = Vec::new();

    // Version
    preimage.extend_from_slice(&1u32.to_le_bytes());

    // Inputs
    write_varint(&mut preimage, inputs.len() as u64);
    for (i, utxo) in inputs.iter().enumerate() {
        let mut txid_bytes = crate::simd::hex::hex_string_to_bytes(&utxo.txid);
        txid_bytes.reverse();
        preimage.extend_from_slice(&txid_bytes);
        preimage.extend_from_slice(&utxo.vout.to_le_bytes());

        if i == signing_index {
            // This input gets the scriptPubKey
            write_varint(&mut preimage, own_script.len() as u64);
            preimage.extend_from_slice(own_script);
        } else {
            // Other inputs get empty script
            preimage.push(0x00);
        }
        preimage.extend_from_slice(&0xffffffffu32.to_le_bytes());
    }

    // Outputs
    let output_count = if change > 0 { 2u64 } else { 1u64 };
    write_varint(&mut preimage, output_count);
    preimage.extend_from_slice(&amount.to_le_bytes());
    write_varint(&mut preimage, to_script.len() as u64);
    preimage.extend_from_slice(to_script);
    if change > 0 {
        preimage.extend_from_slice(&change.to_le_bytes());
        write_varint(&mut preimage, own_script.len() as u64);
        preimage.extend_from_slice(own_script);
    }

    // Locktime
    preimage.extend_from_slice(&0u32.to_le_bytes());

    // SIGHASH_ALL flag
    preimage.extend_from_slice(&1u32.to_le_bytes());

    // Double SHA256
    use sha2::{Sha256, Digest};
    let hash1 = Sha256::digest(&preimage);
    let hash2 = Sha256::digest(hash1);
    let mut result = [0u8; 32];
    result.copy_from_slice(&hash2);
    result
}

/// Write a Bitcoin-style variable-length integer
fn write_varint(buf: &mut Vec<u8>, val: u64) {
    if val < 0xfd {
        buf.push(val as u8);
    } else if val <= 0xffff {
        buf.push(0xfd);
        buf.extend_from_slice(&(val as u16).to_le_bytes());
    } else if val <= 0xffffffff {
        buf.push(0xfe);
        buf.extend_from_slice(&(val as u32).to_le_bytes());
    } else {
        buf.push(0xff);
        buf.extend_from_slice(&val.to_le_bytes());
    }
}
