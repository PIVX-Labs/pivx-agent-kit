//! Operator spending policy: per-payment and daily caps, an approval
//! threshold, and an optional destination allowlist.
//!
//! "Max" means never: a payment over a hard cap is refused, and an
//! operator approval doesn't change that (they raise the cap instead).
//! "Approve above" and the allowlist mean ask first: the payment waits
//! in the pending queue until a human approves it.
//!
//! The policy lives in `policy.json` with an HMAC over its contents
//! keyed from the device. A hand edit breaks the MAC and blocks every
//! payment until the operator re-signs it with `policy set`. This is
//! tamper *evidence* against naive edits, not a security boundary: an
//! agent with full shell access could recompute the key.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::error::Error;
use std::path::PathBuf;

use crate::price;

pub const POLICY_FILE: &str = "policy.json";
const KNOWN_FIELDS: &[&str] = &["max_per_tx", "max_daily", "approve_above", "allowlist", "updated_at"];

/// A limit in PIV (satoshis) or US dollars (`usd_e8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Limit {
    Piv(u64),
    Usd(u64),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub max_per_tx: Option<Limit>,
    #[serde(default)]
    pub max_daily: Option<Limit>,
    #[serde(default)]
    pub approve_above: Option<Limit>,
    /// If non-empty, payments to any other address need approval.
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub updated_at: u64,
}

/// The payment being checked.
pub struct SpendCheck<'a> {
    pub amount_sat: u64,
    pub to: &'a str,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow,
    /// Human-readable reasons, e.g. "amount $12.00 exceeds approval threshold $10.00".
    NeedsApproval(Vec<String>),
    Deny(String),
}

/// Parse a limit as typed by the operator: `25` (PIV) or `$25` / `25usd`.
pub fn parse_limit(s: &str) -> Result<Limit, String> {
    match price::parse_amount(s).map_err(|e| format!("invalid limit {:?}: {}", s, e))? {
        price::Amount::Piv(sat) => Ok(Limit::Piv(sat)),
        price::Amount::Usd(usd) => Ok(Limit::Usd(usd)),
    }
}

/// `"$25.00"` or `"362.94 PIV"`, plus the other unit in parentheses when
/// a price is known.
pub fn describe_limit(limit: Limit, price_e8: Option<u64>) -> String {
    match (limit, price_e8) {
        (Limit::Usd(u), Some(p)) => match price::usd_to_sat(u, p) {
            Ok(sat) => format!("${} (≈ {} PIV)", price::format_usd(u), format_piv(sat)),
            Err(_) => format!("${}", price::format_usd(u)),
        },
        (Limit::Usd(u), None) => format!("${}", price::format_usd(u)),
        (Limit::Piv(sat), Some(p)) => {
            format!("{} PIV (≈ ${})", format_piv(sat), price::format_usd(price::sat_to_usd_e8(sat, p)))
        }
        (Limit::Piv(sat), None) => format!("{} PIV", format_piv(sat)),
    }
}

/// Satoshis as PIV with two decimals, rounded half up.
fn format_piv(sat: u64) -> String {
    let centi = (sat as u128 + 500_000) / 1_000_000;
    format!("{}.{:02}", centi / 100, centi % 100)
}

/// An amount in the limit's own unit, for messages.
fn describe_amount(sat: u64, like: Limit, price_e8: Option<u64>) -> String {
    match (like, price_e8) {
        (Limit::Usd(_), Some(p)) => format!("${}", price::format_usd(price::sat_to_usd_e8(sat, p))),
        _ => format!("{} PIV", format_piv(sat)),
    }
}

/// The limit in satoshis, or `None` for a dollar limit without a price.
fn limit_sat(limit: Limit, price_e8: Option<u64>) -> Option<u64> {
    match limit {
        Limit::Piv(sat) => Some(sat),
        // Saturating: too small to be worth a satoshi allows nothing,
        // too large to fit is effectively unlimited.
        Limit::Usd(u) => price_e8.map(|p| {
            if p == 0 {
                return 0;
            }
            let sat = (u as u128 * 100_000_000 + p as u128 / 2) / p as u128;
            u64::try_from(sat).unwrap_or(u64::MAX)
        }),
    }
}

/// Reject policies that can't work: zero limits, an approval threshold
/// at or above the per-payment cap (approval would never trigger), and
/// malformed allowlist entries.
pub fn validate(p: &Policy) -> Result<(), String> {
    for (name, limit) in [("max per payment", p.max_per_tx), ("daily max", p.max_daily), ("approve above", p.approve_above)] {
        if let Some(Limit::Piv(0) | Limit::Usd(0)) = limit {
            return Err(format!("{} must be greater than zero", name));
        }
    }
    // Same-unit checks only: a mixed-unit pair depends on the price.
    for (cap, flag) in [(p.max_per_tx, "--max-tx"), (p.max_daily, "--daily")] {
        match (p.approve_above, cap) {
            (Some(Limit::Piv(a)), Some(Limit::Piv(m))) | (Some(Limit::Usd(a)), Some(Limit::Usd(m))) if a >= m => {
                return Err(format!("--approve-above must be below {}, or approval would never be asked for", flag));
            }
            _ => {}
        }
    }
    for addr in &p.allowlist {
        let plausible = (addr.starts_with('D') || addr.to_ascii_lowercase().starts_with("ps1"))
            && addr.len() >= 4
            && addr.chars().all(|c| c.is_ascii_alphanumeric());
        if !plausible {
            return Err(format!("allowlist entry {:?} doesn't look like a PIVX address (D… or ps1…)", addr));
        }
    }
    Ok(())
}

/// Decide whether a payment may go out. `spent_24h_sat` is what the
/// ledger says went out (or may have) in the last 24 hours, fees
/// included. `price_e8` is needed only for dollar limits; without it a
/// dollar limit can't be evaluated and the payment is refused.
/// `approved` is true when an operator approved this exact payment: it
/// clears the approval threshold and the allowlist, never the caps.
pub fn evaluate(
    policy: &Policy,
    check: &SpendCheck,
    spent_24h_sat: u64,
    price_e8: Option<u64>,
    approved: bool,
) -> Decision {
    if needs_price(policy) && price_e8.is_none() {
        return Decision::Deny(
            "can't evaluate a dollar spending limit: PIV price unavailable. Retry, or ask your operator to set PIV limits".into(),
        );
    }
    let amount = check.amount_sat;

    if let Some(limit) = policy.max_per_tx {
        if amount > limit_sat(limit, price_e8).unwrap_or(0) {
            return Decision::Deny(format!(
                "{} exceeds max per payment {}",
                describe_amount(amount, limit, price_e8),
                describe_limit(limit, price_e8)
            ));
        }
    }
    if let Some(limit) = policy.max_daily {
        let total = spent_24h_sat.saturating_add(amount);
        if total > limit_sat(limit, price_e8).unwrap_or(0) {
            return Decision::Deny(format!(
                "{} would bring the last 24h to {}, over the daily max {}",
                describe_amount(amount, limit, price_e8),
                describe_amount(total, limit, price_e8),
                describe_limit(limit, price_e8)
            ));
        }
    }
    if approved {
        return Decision::Allow;
    }

    let mut reasons = Vec::new();
    if let Some(limit) = policy.approve_above {
        if amount > limit_sat(limit, price_e8).unwrap_or(0) {
            reasons.push(format!(
                "amount {} exceeds approval threshold {}",
                describe_amount(amount, limit, price_e8),
                describe_limit(limit, price_e8)
            ));
        }
    }
    if !policy.allowlist.is_empty() && !policy.allowlist.iter().any(|a| a == check.to) {
        reasons.push(format!("destination {} isn't on the allowlist", check.to));
    }
    if reasons.is_empty() {
        Decision::Allow
    } else {
        Decision::NeedsApproval(reasons)
    }
}

/// True if any limit is in dollars (a price is needed to evaluate).
pub fn needs_price(p: &Policy) -> bool {
    [p.max_per_tx, p.max_daily, p.approve_above]
        .iter()
        .any(|l| matches!(l, Some(Limit::Usd(_))))
}

/// On disk: the policy as stored plus a MAC over exactly that value, so
/// a file written by a newer version (with extra fields) still verifies.
#[derive(Serialize, Deserialize)]
struct Stored {
    policy: serde_json::Value,
    mac: String,
}

pub struct PolicyStore {
    path: PathBuf,
    key: [u8; 32],
}

impl PolicyStore {
    pub fn open_default() -> Result<Self, Box<dyn Error>> {
        Ok(Self::at(
            crate::wallet::get_data_dir().join(POLICY_FILE),
            crate::wallet::device_subkey(b"pivx-agent-kit-policy-mac")?,
        ))
    }

    pub fn at(path: PathBuf, key: [u8; 32]) -> Self {
        Self { path, key }
    }

    /// `Ok(None)` when no policy is set. An error when the file exists
    /// but can't be read, parsed, or its MAC doesn't match — callers
    /// must refuse to pay in that case.
    pub fn load(&self) -> Result<Option<Policy>, Box<dyn Error>> {
        let Some(stored) = self.read()? else { return Ok(None) };
        // Constant-time compare via the hmac crate.
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key)?;
        m.update(&serde_json::to_vec(&stored.policy)?);
        let valid = hex_decode(&stored.mac).is_some_and(|given| m.verify_slice(&given).is_ok());
        if !valid {
            return Err(format!(
                "spending policy file {} was modified outside `pivx-agent-kit policy`; all payments are blocked. \
                 Run `pivx-agent-kit policy show` to inspect it and `pivx-agent-kit policy set` to re-sign it.",
                self.path.display()
            )
            .into());
        }
        // A field this version doesn't know may be a limit it can't
        // enforce (written by a newer kit): block rather than ignore it.
        if let Some(obj) = stored.policy.as_object() {
            let unknown: Vec<&str> = obj.keys().map(|k| k.as_str()).filter(|k| !KNOWN_FIELDS.contains(k)).collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "spending policy has settings from a newer pivx-agent-kit ({}); update the kit before spending",
                    unknown.join(", ")
                )
                .into());
            }
        }
        Ok(Some(parse_policy(stored.policy, &self.path)?))
    }

    fn read(&self) -> Result<Option<Stored>, Box<dyn Error>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read spending policy {}: {}; all payments are blocked", self.path.display(), e).into()),
        };
        serde_json::from_str::<Stored>(&text).map(Some).map_err(|e| {
            format!(
                "spending policy {} is corrupt ({}); all payments are blocked. Run `pivx-agent-kit policy set` to write a new one, or `pivx-agent-kit policy clear` to remove it.",
                self.path.display(),
                e
            )
            .into()
        })
    }

    /// Like [`load`] but ignores the MAC, for `policy show` to display a
    /// tampered file. Never use this to decide a payment.
    pub fn load_unverified(&self) -> Result<Option<Policy>, Box<dyn Error>> {
        match self.read()? {
            Some(s) => Ok(Some(parse_policy(s.policy, &self.path)?)),
            None => Ok(None),
        }
    }

    pub fn save(&self, p: &Policy) -> Result<(), Box<dyn Error>> {
        let value = serde_json::to_value(p)?;
        let stored = Stored { mac: self.mac_value(&value)?, policy: value };
        write_private(&self.path, serde_json::to_string_pretty(&stored)?.as_bytes())
    }

    pub fn clear(&self) -> Result<(), Box<dyn Error>> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn path_for_tests(&self) -> PathBuf {
        self.path.clone()
    }

    /// MAC over a stored policy value (canonical: serde_json sorts keys).
    pub(crate) fn mac_value(&self, policy: &serde_json::Value) -> Result<String, Box<dyn Error>> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key)?;
        m.update(&serde_json::to_vec(policy)?);
        Ok(m.finalize().into_bytes().iter().map(|b| format!("{:02x}", b)).collect())
    }
}

fn parse_policy(v: serde_json::Value, path: &std::path::Path) -> Result<Policy, Box<dyn Error>> {
    serde_json::from_value(v).map_err(|e| {
        format!(
            "spending policy {} is corrupt ({}); all payments are blocked. Run `pivx-agent-kit policy set` to write a new one, or `pivx-agent-kit policy clear` to remove it.",
            path.display(),
            e
        )
        .into()
    })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// A temp path next to `path`, unique per process and per call, so
/// concurrent writers never share (and truncate) each other's temp file.
pub(crate) fn temp_path_for(path: &std::path::Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), crate::ledger::new_id()))
}

/// Atomic write (temp file + rename) with mode 600 on Unix.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = temp_path_for(path);
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 6_888_275; // $0.06888275 / PIV
    const PIV: u64 = 100_000_000;

    fn usd(d: u64) -> Limit {
        Limit::Usd(d * 100_000_000)
    }

    fn piv(n: u64) -> Limit {
        Limit::Piv(n * PIV)
    }

    fn check(amount_sat: u64) -> SpendCheck<'static> {
        SpendCheck { amount_sat, to: "DDest" }
    }

    /// PIV amount worth `d` dollars at P (rounded).
    fn sat_for_usd(cents: u64) -> u64 {
        price::usd_to_sat(cents * 1_000_000, P).unwrap()
    }

    // -------- parse / describe ------------------------------------

    #[test]
    fn parses_limits() {
        assert_eq!(parse_limit("25").unwrap(), piv(25));
        assert_eq!(parse_limit("$25").unwrap(), usd(25));
        assert_eq!(parse_limit("25usd").unwrap(), usd(25));
        assert_eq!(parse_limit("$0.50").unwrap(), Limit::Usd(50_000_000));
        for bad in ["", "$", "0", "$0", "-5", "abc"] {
            assert!(parse_limit(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn describes_limits_in_both_units() {
        assert_eq!(describe_limit(usd(25), Some(P)), "$25.00 (≈ 362.94 PIV)");
        assert_eq!(describe_limit(usd(25), None), "$25.00");
        assert_eq!(describe_limit(piv(100), Some(P)), "100.00 PIV (≈ $6.89)");
        assert_eq!(describe_limit(piv(100), None), "100.00 PIV");
    }

    // -------- validate --------------------------------------------

    #[test]
    fn validate_rejects_threshold_at_or_above_cap() {
        let p = Policy { max_per_tx: Some(usd(25)), approve_above: Some(usd(25)), ..Default::default() };
        assert!(validate(&p).unwrap_err().contains("approve"));
        let p = Policy { max_per_tx: Some(piv(10)), approve_above: Some(piv(11)), ..Default::default() };
        assert!(validate(&p).is_err());
        let ok = Policy { max_per_tx: Some(usd(25)), approve_above: Some(usd(10)), ..Default::default() };
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn validate_rejects_threshold_at_or_above_daily_cap() {
        let p = Policy { max_daily: Some(usd(20)), approve_above: Some(usd(20)), ..Default::default() };
        assert!(validate(&p).is_err());
        let ok = Policy { max_daily: Some(usd(20)), approve_above: Some(usd(19)), ..Default::default() };
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn huge_dollar_limit_does_not_deny_everything() {
        let p = Policy { max_per_tx: Some(Limit::Usd(u64::MAX)), ..Default::default() };
        assert_eq!(evaluate(&p, &check(1_000 * PIV), 0, Some(1), false), Decision::Allow);
    }

    #[test]
    fn validate_rejects_bad_allowlist_entries() {
        for bad in ["", "D has space", "not-an-address"] {
            let p = Policy { allowlist: vec![bad.into()], ..Default::default() };
            assert!(validate(&p).is_err(), "{bad:?}");
        }
        let ok = Policy { allowlist: vec!["DLabsktzGMnsK5K9uRTMCF6NoYNY6ET4Bb".into(), "ps1abc".into()], ..Default::default() };
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn needs_price_only_for_dollar_limits() {
        assert!(!needs_price(&Policy { max_per_tx: Some(piv(1)), ..Default::default() }));
        assert!(needs_price(&Policy { max_daily: Some(usd(1)), ..Default::default() }));
        assert!(!needs_price(&Policy::default()));
    }

    // -------- evaluate --------------------------------------------

    #[test]
    fn empty_policy_allows_everything() {
        assert_eq!(evaluate(&Policy::default(), &check(u64::MAX / 2), u64::MAX / 2, None, false), Decision::Allow);
    }

    #[test]
    fn per_payment_cap_is_hard() {
        let p = Policy { max_per_tx: Some(usd(25)), ..Default::default() };
        assert_eq!(evaluate(&p, &check(sat_for_usd(2_500)), 0, Some(P), false), Decision::Allow);
        let d = evaluate(&p, &check(sat_for_usd(2_501)), 0, Some(P), false);
        assert!(matches!(&d, Decision::Deny(m) if m.contains("$25.01") && m.contains("max per payment")), "{d:?}");
        // Operator approval doesn't lift a hard cap.
        assert!(matches!(evaluate(&p, &check(sat_for_usd(2_501)), 0, Some(P), true), Decision::Deny(_)));
    }

    #[test]
    fn daily_cap_counts_what_was_already_spent() {
        let p = Policy { max_daily: Some(piv(100)), ..Default::default() };
        assert_eq!(evaluate(&p, &check(40 * PIV), 60 * PIV, None, false), Decision::Allow);
        let d = evaluate(&p, &check(40 * PIV + 1), 60 * PIV, None, false);
        assert!(matches!(&d, Decision::Deny(m) if m.contains("daily")), "{d:?}");
        assert!(matches!(evaluate(&p, &check(1), 100 * PIV, None, true), Decision::Deny(_)));
    }

    #[test]
    fn approval_threshold_asks_first() {
        let p = Policy { approve_above: Some(usd(10)), ..Default::default() };
        assert_eq!(evaluate(&p, &check(sat_for_usd(1_000)), 0, Some(P), false), Decision::Allow);
        let d = evaluate(&p, &check(sat_for_usd(1_200)), 0, Some(P), false);
        match d {
            Decision::NeedsApproval(r) => {
                assert_eq!(r.len(), 1);
                assert!(r[0].contains("$12.00") && r[0].contains("$10.00"), "{r:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(evaluate(&p, &check(sat_for_usd(1_200)), 0, Some(P), true), Decision::Allow);
    }

    #[test]
    fn allowlist_asks_for_unlisted_destinations() {
        let p = Policy { allowlist: vec!["DFriend".into()], ..Default::default() };
        assert_eq!(evaluate(&p, &SpendCheck { amount_sat: 1, to: "DFriend" }, 0, None, false), Decision::Allow);
        let d = evaluate(&p, &SpendCheck { amount_sat: 1, to: "DStranger" }, 0, None, false);
        assert!(matches!(&d, Decision::NeedsApproval(r) if r[0].contains("DStranger")), "{d:?}");
        assert_eq!(evaluate(&p, &SpendCheck { amount_sat: 1, to: "DStranger" }, 0, None, true), Decision::Allow);
    }

    #[test]
    fn approval_reasons_accumulate() {
        let p = Policy { approve_above: Some(piv(1)), allowlist: vec!["DFriend".into()], ..Default::default() };
        let d = evaluate(&p, &SpendCheck { amount_sat: 2 * PIV, to: "DStranger" }, 0, None, false);
        assert!(matches!(&d, Decision::NeedsApproval(r) if r.len() == 2), "{d:?}");
    }

    #[test]
    fn deny_wins_over_needs_approval() {
        let p = Policy { max_per_tx: Some(piv(5)), approve_above: Some(piv(1)), ..Default::default() };
        assert!(matches!(evaluate(&p, &check(6 * PIV), 0, None, false), Decision::Deny(_)));
    }

    #[test]
    fn dollar_limit_without_price_is_refused() {
        let p = Policy { max_daily: Some(usd(50)), ..Default::default() };
        let d = evaluate(&p, &check(1), 0, None, false);
        assert!(matches!(&d, Decision::Deny(m) if m.contains("price")), "{d:?}");
        // Even with approval.
        assert!(matches!(evaluate(&p, &check(1), 0, None, true), Decision::Deny(_)));
    }

    #[test]
    fn mixed_units_evaluate_each_limit_in_its_own_unit() {
        let p = Policy { max_per_tx: Some(piv(1_000)), max_daily: Some(usd(50)), ..Default::default() };
        // 500 PIV ≈ $34.44: under both.
        assert_eq!(evaluate(&p, &check(500 * PIV), 0, Some(P), false), Decision::Allow);
        // Already spent 300 PIV (≈ $20.66): 500 more ≈ $55.11 total > $50.
        assert!(matches!(evaluate(&p, &check(500 * PIV), 300 * PIV, Some(P), false), Decision::Deny(_)));
    }

    // -------- store -----------------------------------------------

    fn store(tag: &str) -> PolicyStore {
        PolicyStore::at(crate::ledger::tests::temp_dir(tag).join(POLICY_FILE), [7u8; 32])
    }

    fn sample() -> Policy {
        Policy {
            max_per_tx: Some(usd(25)),
            max_daily: Some(usd(50)),
            approve_above: Some(usd(10)),
            allowlist: vec!["DFriend".into()],
            updated_at: 1_790_000_000,
        }
    }

    #[test]
    fn missing_file_is_no_policy() {
        assert_eq!(store("pol-missing").load().unwrap(), None);
    }

    #[test]
    fn save_load_round_trip() {
        let s = store("pol-rt");
        s.save(&sample()).unwrap();
        assert_eq!(s.load().unwrap(), Some(sample()));
    }

    #[test]
    fn any_edit_breaks_the_mac() {
        let s = store("pol-tamper");
        s.save(&sample()).unwrap();
        let text = std::fs::read_to_string(&s.path).unwrap();
        let edited = text.replace("5000000000", "500000000000");
        assert_ne!(text, edited, "fixture must actually change");
        std::fs::write(&s.path, edited).unwrap();
        let err = s.load().unwrap_err().to_string();
        assert!(err.contains("modified outside"), "{err}");
        // `policy show` can still display it.
        assert!(s.load_unverified().unwrap().is_some());
    }

    #[test]
    fn deleting_fields_breaks_the_mac() {
        let s = store("pol-strip");
        s.save(&sample()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&s.path).unwrap()).unwrap();
        v["policy"]["max_daily"] = serde_json::Value::Null;
        std::fs::write(&s.path, v.to_string()).unwrap();
        assert!(s.load().is_err());
    }

    #[test]
    fn mac_is_keyed() {
        let s = store("pol-key");
        s.save(&sample()).unwrap();
        let other = PolicyStore::at(s.path.clone(), [8u8; 32]);
        assert!(other.load().is_err());
    }

    #[test]
    fn whitespace_reformatting_keeps_the_mac_valid() {
        let s = store("pol-ws");
        s.save(&sample()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&s.path).unwrap()).unwrap();
        std::fs::write(&s.path, v.to_string()).unwrap(); // compact form
        assert_eq!(s.load().unwrap(), Some(sample()));
    }

    #[test]
    fn garbage_file_is_an_error_not_no_policy() {
        let s = store("pol-garbage");
        std::fs::write(&s.path, "{}").unwrap();
        assert!(s.load().is_err());
        std::fs::write(&s.path, "garbage").unwrap();
        let err = s.load().unwrap_err().to_string();
        assert!(err.contains("policy clear") && err.contains("policy set"), "{err}");
    }

    #[test]
    fn policy_from_a_newer_version_blocks_instead_of_weakening() {
        // A later version adds a (stricter) limit this version can't
        // enforce: refuse to spend rather than silently ignore it.
        let s = store("pol-forward");
        s.save(&sample()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&s.path).unwrap()).unwrap();
        v["policy"]["max_per_week"] = serde_json::json!({"usd": 100});
        v["mac"] = serde_json::json!(s.mac_value(&v["policy"]).unwrap());
        std::fs::write(&s.path, v.to_string()).unwrap();
        let err = s.load().unwrap_err().to_string();
        assert!(err.contains("newer") && err.contains("max_per_week"), "{err}");
    }

    #[test]
    fn allowlist_accepts_uppercase_shield_addresses() {
        let ok = Policy { allowlist: vec!["PS1ABC".into()], ..Default::default() };
        assert!(validate(&ok).is_ok());
    }

    #[test]
    fn clear_removes_policy() {
        let s = store("pol-clear");
        s.save(&sample()).unwrap();
        s.clear().unwrap();
        assert_eq!(s.load().unwrap(), None);
        s.clear().unwrap(); // idempotent
    }

    #[cfg(unix)]
    #[test]
    fn policy_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let s = store("pol-perm");
        s.save(&sample()).unwrap();
        assert_eq!(std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
