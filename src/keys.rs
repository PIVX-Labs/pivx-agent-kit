use pivx_client_backend::encoding::{decode_payment_address, decode_transparent_address};
use pivx_client_backend::keys::sapling as sapling_keys;
use pivx_primitives::consensus::{NetworkConstants, MAIN_NETWORK};
use pivx_primitives::legacy::TransparentAddress;
use pivx_primitives::zip32::AccountId;
use ::sapling::zip32::{ExtendedFullViewingKey, ExtendedSpendingKey};
use ::sapling::PaymentAddress;
use std::error::Error;
use zcash_keys::encoding;

/// Shield or transparent address
pub enum GenericAddress {
    Shield(PaymentAddress),
    Transparent(TransparentAddress),
}

/// Derive an extended spending key from a 32-byte seed
pub fn spending_key_from_seed(
    seed: &[u8; 32],
    coin_type: u32,
    account_index: u32,
) -> Result<ExtendedSpendingKey, Box<dyn Error>> {
    let account_id =
        AccountId::try_from(account_index).map_err(|_| "Invalid account index")?;
    Ok(sapling_keys::spending_key(seed, coin_type, account_id))
}

/// Derive the extended full viewing key from an extended spending key
#[allow(deprecated)]
pub fn full_viewing_key(extsk: &ExtendedSpendingKey) -> ExtendedFullViewingKey {
    extsk.to_extended_full_viewing_key()
}

/// Get the default payment address from an encoded extfvk
pub fn get_default_address(enc_extfvk: &str) -> Result<String, Box<dyn Error>> {
    let extfvk = decode_extfvk(enc_extfvk)?;
    let (_index, address) = extfvk
        .to_diversifiable_full_viewing_key()
        .default_address();
    Ok(encode_payment_address(&address))
}

// ---------------------------------------------------------------------------
// Encoding / decoding helpers
// ---------------------------------------------------------------------------

pub fn encode_extsk(extsk: &ExtendedSpendingKey) -> String {
    encoding::encode_extended_spending_key(
        MAIN_NETWORK.hrp_sapling_extended_spending_key(),
        extsk,
    )
}

pub fn decode_extsk(enc: &str) -> Result<ExtendedSpendingKey, Box<dyn Error>> {
    Ok(encoding::decode_extended_spending_key(
        MAIN_NETWORK.hrp_sapling_extended_spending_key(),
        enc,
    )?)
}

pub fn encode_extfvk(extfvk: &ExtendedFullViewingKey) -> String {
    encoding::encode_extended_full_viewing_key(
        MAIN_NETWORK.hrp_sapling_extended_full_viewing_key(),
        extfvk,
    )
}

pub fn decode_extfvk(enc: &str) -> Result<ExtendedFullViewingKey, Box<dyn Error>> {
    Ok(encoding::decode_extended_full_viewing_key(
        MAIN_NETWORK.hrp_sapling_extended_full_viewing_key(),
        enc,
    )?)
}

pub fn encode_payment_address(addr: &PaymentAddress) -> String {
    encoding::encode_payment_address(
        MAIN_NETWORK.hrp_sapling_payment_address(),
        addr,
    )
}

pub fn decode_generic_address(address: &str) -> Result<GenericAddress, Box<dyn Error>> {
    if address.starts_with(MAIN_NETWORK.hrp_sapling_payment_address()) {
        let addr =
            decode_payment_address(MAIN_NETWORK.hrp_sapling_payment_address(), address)
                .map_err(|_| "Failed to decode shield address")?;
        Ok(GenericAddress::Shield(addr))
    } else {
        let addr = decode_transparent_address(
            &MAIN_NETWORK.b58_pubkey_address_prefix(),
            &MAIN_NETWORK.b58_script_address_prefix(),
            address,
        )
        .map_err(|_| "Failed to decode transparent address")?
        .ok_or("Invalid transparent address")?;
        Ok(GenericAddress::Transparent(addr))
    }
}
