//! Local cache of PIVCards orders this wallet has created.
//!
//! Stored next to `wallet.json` in the kit's data dir. Saves the
//! agent from re-typing 32-byte hex IDs across invocations and lets
//! `cards order list` show what's outstanding without polling each
//! one. The cache is keyed on the wallet's transparent address — a
//! seed swap silently invalidates it (the same pattern `task::state`
//! uses for its handle cache).
//!
//! Cancelled / completed orders are kept in the cache: an agent that
//! purchased a card last week may still want to fetch the dispatch
//! payload (code/pin) on demand. Pruning is the user's job (or a
//! future `cards order forget` if it becomes annoying).

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use crate::wallet;

#[derive(Default, Serialize, Deserialize)]
struct CardsState {
    /// Address the cached entries were created under. Compared against
    /// the current wallet's address before reads — a seed swap
    /// silently invalidates the cache.
    #[serde(default)]
    address: Option<String>,
    /// Most-recent-first list of order IDs created by this wallet.
    #[serde(default)]
    orders: Vec<OrderEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OrderEntry {
    pub id: String,
    /// Slug of the card that was ordered. Useful for display in
    /// `cards order list` so the agent doesn't have to call
    /// `/order/check` on every entry just to see what it bought.
    #[serde(default)]
    pub item_slug: Option<String>,
    /// Amount + currency, shaped like `"50 USD"` for display.
    #[serde(default)]
    pub amount: Option<String>,
    /// Unix-seconds creation time. Sorting on this keeps the list
    /// stable even if a future kit version reorders.
    #[serde(default)]
    pub created_at: Option<u64>,
}

fn state_path() -> PathBuf {
    wallet::get_data_dir().join("cards_state.json")
}

fn load() -> CardsState {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(state: &CardsState) -> Result<(), Box<dyn Error>> {
    let dir = wallet::get_data_dir();
    fs::create_dir_all(&dir).ok();
    let json = serde_json::to_string_pretty(state)?;
    fs::write(state_path(), json)?;
    Ok(())
}

fn current_address() -> Option<String> {
    use pivx_wallet_kit::keys as kit_keys;
    let wallet_data = wallet::load_wallet().ok()?;
    let bip39_seed = wallet_data.get_bip39_seed();
    let (address, _pubkey, _privkey) =
        kit_keys::transparent_key_from_bip39_seed(&bip39_seed, 0, 0).ok()?;
    Some(address)
}

/// Append (or replace-by-id) a fresh order. Resets the cache if the
/// wallet address has changed since the last write.
pub fn record_order(entry: OrderEntry) -> Result<(), Box<dyn Error>> {
    let now_addr =
        current_address().ok_or("cannot resolve wallet address — is the wallet initialized?")?;
    let mut state = load();
    if state.address.as_deref() != Some(now_addr.as_str()) {
        state = CardsState::default();
        state.address = Some(now_addr);
    }
    // Replace if same id already present, else prepend
    state.orders.retain(|o| o.id != entry.id);
    state.orders.insert(0, entry);
    save(&state)
}

/// Returns the cache, but only when it belongs to the currently-loaded
/// wallet. Empty otherwise.
pub fn list_orders() -> Vec<OrderEntry> {
    let state = load();
    let cached_addr = match state.address.as_deref() {
        Some(a) => a,
        None => return vec![],
    };
    let now_addr = match current_address() {
        Some(a) => a,
        None => return vec![],
    };
    if cached_addr == now_addr {
        state.orders
    } else {
        vec![]
    }
}
