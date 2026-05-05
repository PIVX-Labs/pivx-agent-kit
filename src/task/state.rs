//! Persisted kit-side state for the tasks platform — currently just
//! the agent's auto-generated handle. Lives next to `wallet.json` in
//! the kit's data dir so it follows the same install boundary.
//!
//! The cached entry is keyed by the wallet's transparent address: if
//! the agent imports a different seed (different address), the old
//! cache entry is ignored rather than returning a wrong handle for
//! the wrong wallet.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use crate::wallet;

#[derive(Default, Serialize, Deserialize)]
struct TasksState {
    /// Address the cached handle was issued to. Compared against the
    /// current wallet's address before the handle is returned, so a
    /// seed swap silently invalidates the cache.
    #[serde(default)]
    address: Option<String>,
    /// The handle the platform assigned to `address`.
    #[serde(default)]
    handle: Option<String>,
}

fn state_path() -> PathBuf {
    wallet::get_data_dir().join("tasks_state.json")
}

fn load() -> TasksState {
    let path = state_path();
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(state: &TasksState) -> Result<(), Box<dyn Error>> {
    let dir = wallet::get_data_dir();
    fs::create_dir_all(&dir).ok();
    let path = state_path();
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Resolve the kit's current transparent address. Used to scope
/// cache reads/writes — wallets that haven't been initialized yet
/// short-circuit cache hits.
fn current_address() -> Option<String> {
    use pivx_wallet_kit::keys as kit_keys;
    let wallet_data = wallet::load_wallet().ok()?;
    let bip39_seed = wallet_data.get_bip39_seed().ok()?;
    let (address, _pubkey, _privkey) =
        kit_keys::transparent_key_from_bip39_seed(&bip39_seed, 0, 0).ok()?;
    Some(address)
}

/// Returns the cached handle iff the cached entry was issued to the
/// wallet that's currently loaded. A wallet swap (or no wallet) is
/// treated as a cache miss.
pub fn cached_handle() -> Result<Option<String>, Box<dyn Error>> {
    let state = load();
    let cached_addr = match state.address.as_deref() {
        Some(a) => a,
        None => return Ok(None),
    };
    let now_addr = match current_address() {
        Some(a) => a,
        None => return Ok(None),
    };
    if cached_addr == now_addr {
        Ok(state.handle)
    } else {
        Ok(None)
    }
}

pub fn set_cached_handle(handle: &str) -> Result<(), Box<dyn Error>> {
    let address = current_address().ok_or("could not resolve current wallet address for cache")?;
    let state = TasksState {
        address: Some(address),
        handle: Some(handle.to_string()),
    };
    save(&state)
}

/// Best-effort: extract the worker handle from any platform response
/// shape that includes it (signup response, register response, etc.).
/// Silent on miss — the cache is opportunistic.
pub fn cache_handle_from_task(value: &Value) -> Result<(), Box<dyn Error>> {
    if let Some(h) = value
        .get("worker_handle")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("handle").and_then(|v| v.as_str()))
    {
        return set_cached_handle(h);
    }
    Ok(())
}
