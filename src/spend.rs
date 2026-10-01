//! Types shared by every path that spends from the wallet.
//!
//! `core::send` is the single choke point for outgoing payments; plain
//! sends, `task approve` auto-pay and `cards order pay` all build a
//! [`SendRequest`] and hand it over. The [`Origin`] records *why* a
//! spend happened so the ledger can recognise a retry of the same
//! logical payment (the double-pay guard in `ledger`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// Which balance a payment is funded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pool {
    /// Shield (Sapling) balance.
    Private,
    /// Transparent (`D...`) balance.
    Public,
}

impl Pool {
    pub fn as_str(&self) -> &'static str {
        match self {
            Pool::Private => "private",
            Pool::Public => "public",
        }
    }
}

impl fmt::Display for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Pool {
    type Err = String;

    /// Strict: only the exact strings `private` and `public` are
    /// accepted. A typo must be an error, never a silent default —
    /// `--from pubic` used to spend the shield balance.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "private" => Ok(Pool::Private),
            "public" => Ok(Pool::Public),
            other => Err(format!(
                "invalid balance {:?}: must be 'private' (shield) or 'public' (transparent)",
                other
            )),
        }
    }
}

/// What triggered a spend. Persisted in the ledger; two spends with the
/// same non-`Send` origin are the same logical payment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Origin {
    /// A direct `send` / `pivx_send`. Never deduplicated — sending the
    /// same amount to the same address twice can be intentional.
    Send,
    /// Auto-pay of a PIVX Tasks bounty from `task approve`.
    TaskApprove { task_id: i64, worker: String },
    /// Payment of a PIVCards invoice from `cards order pay`.
    CardsPay { order_id: String },
}

/// Everything `core::send` needs to make (and record) one payment.
#[derive(Clone, Debug)]
pub struct SendRequest {
    pub address: String,
    pub amount_sat: u64,
    pub memo: String,
    pub from: Pool,
    /// Free text shown to the operator in `history`.
    pub reason: Option<String>,
    pub origin: Origin,
    /// The price a dollar amount was converted at. `None` for PIV
    /// amounts; `core::send` then records a best-effort price for history.
    pub price: Option<crate::price::Price>,
}

/// Characters that can forge or hide terminal text: controls, plus
/// Unicode bidi overrides/isolates and zero-width marks.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Longest `reason` accepted (characters).
pub const MAX_REASON_CHARS: usize = 280;

/// A payment reason is agent-written text that ends up in front of the
/// operator (history, approval prompt). Reject control characters —
/// newlines and escape sequences could forge or hide prompt lines —
/// and keep it short.
pub fn validate_reason(reason: &str) -> Result<(), String> {
    if reason.trim().is_empty() {
        return Err("reason must not be empty".into());
    }
    if reason.chars().any(is_unsafe_char) {
        return Err("reason must not contain control characters (newlines, tabs, escape codes, bidi or zero-width marks)".into());
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(format!("reason is too long (max {} characters)", MAX_REASON_CHARS));
    }
    Ok(())
}

/// Compute a transaction id from its serialized hex: double SHA-256 of
/// the raw bytes, byte-reversed. Holds for both v1 transparent and v3
/// Sapling PIVX transactions (pinned by mainnet fixtures in the tests).
pub fn compute_txid(txhex: &str) -> Result<String, Box<dyn Error>> {
    if txhex.is_empty() || !txhex.len().is_multiple_of(2) || !txhex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("transaction hex is empty or malformed".into());
    }
    let raw = hex_decode(txhex);
    let mut hash = Sha256::digest(Sha256::digest(&raw)).to_vec();
    hash.reverse();
    Ok(hash.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Decode hex already validated as even-length ASCII hex digits.
fn hex_decode(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16).unwrap_or(0);
            let lo = (pair[1] as char).to_digit(16).unwrap_or(0);
            (hi * 16 + lo) as u8
        })
        .collect()
}

/// True if `s` looks like a txid: exactly 64 hex characters.
pub fn is_txid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn fixture(name: &str) -> (String, String) {
        let raw = match name {
            "v1" => include_str!("../testdata/mainnet_tx_v1_transparent.json"),
            "v3" => include_str!("../testdata/mainnet_tx_v3_shield.json"),
            _ => unreachable!(),
        };
        let v: Value = serde_json::from_str(raw).unwrap();
        (
            v["hex"].as_str().unwrap().to_string(),
            v["txid"].as_str().unwrap().to_string(),
        )
    }

    // -------- Pool ------------------------------------------------

    #[test]
    fn pool_parses_exact_names() {
        assert_eq!("private".parse::<Pool>().unwrap(), Pool::Private);
        assert_eq!("public".parse::<Pool>().unwrap(), Pool::Public);
    }

    #[test]
    fn pool_rejects_typos_and_variants() {
        for bad in ["pubic", "privat", "PRIVATE", "Public", "private ", "", "shield", "transparent"] {
            let err = bad.parse::<Pool>().unwrap_err();
            assert!(err.contains("private") && err.contains("public"), "error should name valid values: {err}");
        }
    }

    #[test]
    fn pool_round_trips_through_str_and_serde() {
        for p in [Pool::Private, Pool::Public] {
            assert_eq!(p.as_str().parse::<Pool>().unwrap(), p);
            assert_eq!(p.to_string(), p.as_str());
            let j = serde_json::to_value(p).unwrap();
            assert_eq!(j, json!(p.as_str()));
            assert_eq!(serde_json::from_value::<Pool>(j).unwrap(), p);
        }
    }

    // -------- Origin ----------------------------------------------

    #[test]
    fn origin_serializes_with_type_tag() {
        assert_eq!(serde_json::to_value(Origin::Send).unwrap(), json!({"type": "send"}));
        assert_eq!(
            serde_json::to_value(Origin::TaskApprove { task_id: 12, worker: "frosted-otter-417".into() }).unwrap(),
            json!({"type": "task_approve", "task_id": 12, "worker": "frosted-otter-417"})
        );
        assert_eq!(
            serde_json::to_value(Origin::CardsPay { order_id: "abc".into() }).unwrap(),
            json!({"type": "cards_pay", "order_id": "abc"})
        );
    }

    #[test]
    fn origin_round_trips() {
        for o in [
            Origin::Send,
            Origin::TaskApprove { task_id: 5, worker: "w".into() },
            Origin::CardsPay { order_id: "x".into() },
        ] {
            let j = serde_json::to_string(&o).unwrap();
            assert_eq!(serde_json::from_str::<Origin>(&j).unwrap(), o);
        }
    }

    // -------- validate_reason -------------------------------------

    #[test]
    fn reason_accepts_ordinary_text() {
        assert!(validate_reason("Bounty for task #12 to frosted-otter-417 — thanks! ☕").is_ok());
        assert!(validate_reason(&"x".repeat(MAX_REASON_CHARS)).is_ok());
    }

    #[test]
    fn reason_rejects_control_characters_and_length() {
        for bad in [
            "line1\nline2", "a\rb", "hide\x1b[8m", "bell\x07", "tab\there", "nul\0", "\u{9b}31m",
            "flip\u{202E}txt", "zero\u{200B}width", "iso\u{2066}late", "bom\u{FEFF}",
        ] {
            let err = validate_reason(bad).unwrap_err();
            assert!(err.contains("control"), "{bad:?}: {err}");
        }
        assert!(validate_reason(&"x".repeat(MAX_REASON_CHARS + 1)).unwrap_err().contains("280"));
        assert!(validate_reason("   ").is_err());
    }

    // -------- compute_txid ----------------------------------------

    #[test]
    fn txid_matches_mainnet_transparent_v1() {
        let (hex, txid) = fixture("v1");
        assert_eq!(compute_txid(&hex).unwrap(), txid);
    }

    #[test]
    fn txid_matches_mainnet_shield_v3() {
        let (hex, txid) = fixture("v3");
        assert_eq!(compute_txid(&hex).unwrap(), txid);
    }

    #[test]
    fn txid_accepts_uppercase_hex() {
        let (hex, txid) = fixture("v1");
        assert_eq!(compute_txid(&hex.to_uppercase()).unwrap(), txid);
    }

    #[test]
    fn txid_rejects_bad_hex() {
        assert!(compute_txid("").is_err());
        assert!(compute_txid("abc").is_err()); // odd length
        assert!(compute_txid("zz").is_err());
    }

    #[test]
    fn is_txid_checks_shape() {
        assert!(is_txid(&"a".repeat(64)));
        assert!(is_txid(&"0123456789ABCDEFabcdef".repeat(3)[..64]));
        assert!(!is_txid(&"a".repeat(63)));
        assert!(!is_txid(&"a".repeat(65)));
        assert!(!is_txid(&"g".repeat(64)));
        assert!(!is_txid("{\"result\":\"x\"}"));
    }
}
