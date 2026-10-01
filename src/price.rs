//! PIV/USD price: fetching, caching, and exact integer conversion.
//!
//! Prices are carried as `usd_e8` — US dollars per PIV × 10⁸ — and
//! dollar amounts the same way, so every conversion is integer math.
//! The only float is the price as the API reports it, rounded once.
//!
//! Spending uses a price accepted within the last minute. A new price
//! must be fresh at its source, not dated in the future, and within 25%
//! of an hour-long anchor; with no anchor, two sources must agree within
//! 10% first. A bad feed would otherwise turn a "$5" send into far more
//! PIV. Display callers (`balance`) get whatever is plausible, never
//! fail because of it, and never influence what spending trusts.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::error::Error;
use std::path::PathBuf;

pub const PRICE_FILE: &str = "price_cache.json";

/// Primary source. Override with `PIVX_PRICE_API` (same response shape).
pub const ORACLE_URL: &str = "https://pivxla.bz/oracle/api/v1/price/usd";
pub const COINGECKO_URL: &str =
    "https://api.coingecko.com/api/v3/simple/price?ids=pivx&vs_currencies=usd&include_last_updated_at=true";

/// A spend converts with a price at most this old (seconds).
pub const SPEND_MAX_AGE: u64 = 60;
/// `balance` and friends reuse a cached price up to this old.
pub const DISPLAY_MAX_AGE: u64 = 600;
/// A source price whose own `last_updated` is older than this is not
/// used for spending.
pub const SOURCE_STALE_AFTER: u64 = 1_800;
/// Sanity band: refuse a fresh price more than this many percent away
/// from the last accepted one, if that one is younger than BAND_WINDOW.
pub const BAND_PERCENT: u64 = 25;
pub const BAND_WINDOW: u64 = 3_600;
/// Without an anchor, two sources must agree within this many percent.
pub const AGREE_PERCENT: u64 = 10;
/// Source timestamps (and cache entries) further in the future than
/// this are rejected.
pub const MAX_CLOCK_SKEW: u64 = 300;
/// After a failed display fetch, don't try again for this long.
pub const DISPLAY_RETRY_AFTER: u64 = 60;
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub usd_e8: u64,
    /// When the kit fetched it (unix seconds).
    pub fetched_at: u64,
    /// When the source says it last updated (unix seconds).
    pub source_updated_at: u64,
    pub source: String,
}

/// A user-supplied amount: PIV, or US dollars to convert at send time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Amount {
    Piv(u64),
    Usd(u64),
}

/// Parse `"5"`, `"5 piv"`, `"$5"`, `"5usd"`, `"5 USD"`. Up to 8
/// decimal places; must be greater than zero.
pub fn parse_amount(s: &str) -> Result<Amount, String> {
    let bad = || format!("invalid amount {:?}: use PIV like '5' or dollars like '$5' / '5usd'", s);
    let t = s.trim().to_ascii_lowercase();
    let (number, usd) = if let Some(rest) = t.strip_prefix('$') {
        (rest.trim(), true)
    } else if let Some(rest) = t.strip_suffix("usd") {
        (rest.trim(), true)
    } else if let Some(rest) = t.strip_suffix("piv") {
        (rest.trim(), false)
    } else {
        (t.as_str(), false)
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err(bad());
    }
    let units = crate::core::parse_piv_to_sat(number).map_err(|e| format!("{}: {}", bad(), e))?;
    if units == 0 {
        return Err("Amount must be greater than zero".into());
    }
    Ok(if usd { Amount::Usd(units) } else { Amount::Piv(units) })
}

/// Round a reported price to `usd_e8`. Rejects zero, negative, NaN.
pub fn float_to_e8(v: f64) -> Result<u64, String> {
    let scaled = (v * 1e8).round();
    if !scaled.is_finite() || scaled < 1.0 || scaled > u64::MAX as f64 {
        return Err(format!("unusable price {}", v));
    }
    Ok(scaled as u64)
}

/// Dollars (`usd_e8`) → satoshis at `price_e8`, rounded half up.
pub fn usd_to_sat(usd_e8: u64, price_e8: u64) -> Result<u64, String> {
    if price_e8 == 0 {
        return Err("price is zero".into());
    }
    let p = price_e8 as u128;
    let sat = (usd_e8 as u128 * 100_000_000 + p / 2) / p;
    if sat == 0 {
        return Err("dollar amount is too small to send (rounds to 0 satoshis)".into());
    }
    u64::try_from(sat).map_err(|_| "dollar amount is too large".to_string())
}

/// Satoshis → dollars (`usd_e8`) at `price_e8`, rounded half up.
pub fn sat_to_usd_e8(sat: u64, price_e8: u64) -> u64 {
    let usd = (sat as u128 * price_e8 as u128 + 50_000_000) / 100_000_000;
    u64::try_from(usd).unwrap_or(u64::MAX)
}

/// `usd_e8` → `"5.00"` (cents, rounded half up).
pub fn format_usd(usd_e8: u64) -> String {
    let cents = (usd_e8 as u128 + 500_000) / 1_000_000;
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// `price_e8` → `"0.06888275"`.
pub fn format_price(price_e8: u64) -> String {
    pivx_wallet_kit::amount::format_sat_to_piv(price_e8)
}

/// Parse the PIVX Labs oracle: `{"currency":"usd","value":..,"last_updated":..}`
/// (or an array of those). Returns `(usd_e8, last_updated)`.
pub fn parse_oracle(body: &str) -> Result<(u64, u64), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("oracle: bad JSON: {}", e))?;
    let entry = match &v {
        Value::Array(items) => items.iter().find(|i| i["currency"] == "usd"),
        Value::Object(_) if v["currency"] == "usd" => Some(&v),
        _ => None,
    }
    .ok_or("oracle: no usd price in response")?;
    let value = entry["value"].as_f64().ok_or("oracle: usd value is not a number")?;
    let updated = entry["last_updated"].as_u64().ok_or("oracle: missing last_updated")?;
    Ok((float_to_e8(value)?, updated))
}

/// Parse CoinGecko `simple/price`: `{"pivx":{"usd":..,"last_updated_at":..}}`.
pub fn parse_coingecko(body: &str) -> Result<(u64, u64), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("coingecko: bad JSON: {}", e))?;
    let value = v["pivx"]["usd"].as_f64().ok_or("coingecko: no pivx/usd price in response")?;
    let updated = v["pivx"]["last_updated_at"].as_u64().ok_or("coingecko: missing last_updated_at")?;
    Ok((float_to_e8(value)?, updated))
}

/// May `fresh` be used to convert a spend? Checks the source's own
/// timestamp (not stale, not from the future) and, when there is an
/// anchor, that the price is within the band of it.
pub fn check_for_spending(fresh: &Price, anchor: Option<&Price>, now: u64) -> Result<(), String> {
    plausible(fresh, now)?;
    if let Some(anchor) = anchor {
        if anchor.usd_e8 > 0 && !within(fresh.usd_e8, anchor.usd_e8, BAND_PERCENT) {
            return Err(format!(
                "PIV price moved more than {}% within the hour (${} → ${} from {}); refusing to convert dollars. \
                 Use a PIV amount or retry later",
                BAND_PERCENT,
                format_price(anchor.usd_e8),
                format_price(fresh.usd_e8),
                fresh.source
            ));
        }
    }
    Ok(())
}

/// The source's own timestamp is recent and not in the future.
fn plausible(p: &Price, now: u64) -> Result<(), String> {
    if p.source_updated_at > now + MAX_CLOCK_SKEW {
        return Err(format!(
            "price from {} is dated in the future ({}); refusing it",
            p.source,
            crate::timefmt::format_rfc3339(p.source_updated_at)
        ));
    }
    if now.saturating_sub(p.source_updated_at) > SOURCE_STALE_AFTER {
        return Err(format!(
            "price from {} is stale (last updated {}); use a PIV amount or retry later",
            p.source,
            crate::timefmt::format_rfc3339(p.source_updated_at)
        ));
    }
    Ok(())
}

/// `a` is within `percent`% of `reference`.
fn within(a: u64, reference: u64, percent: u64) -> bool {
    (a.abs_diff(reference) as u128) * 100 <= reference as u128 * percent as u128
}

/// A cached entry fetched at most `max_age` ago and not from the future
/// (a clock that was ahead and got corrected).
fn young(p: &Price, now: u64, max_age: u64) -> bool {
    p.fetched_at <= now + MAX_CLOCK_SKEW && now.saturating_sub(p.fetched_at) <= max_age
}

/// True if the amount text names its unit (`$`, `usd`, `piv`).
pub fn has_unit(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    t.starts_with('$') || t.ends_with("usd") || t.ends_with("piv")
}

/// Where a fresh price comes from. Implemented by the HTTP sources;
/// tests substitute fixed answers.
pub trait PriceSource {
    fn fetch(&self, now: u64) -> Result<Price, String>;
}

/// An HTTP price endpoint plus the parser for its response shape.
pub struct HttpSource {
    pub name: &'static str,
    pub url: String,
    pub parse: fn(&str) -> Result<(u64, u64), String>,
}

impl PriceSource for HttpSource {
    fn fetch(&self, now: u64) -> Result<Price, String> {
        // Request::timeout doesn't cover connecting in ureq 2; the agent's does.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(HTTP_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .build();
        let body = agent
            .get(&self.url)
            .call()
            .map_err(|e| format!("{}: {}", self.name, e))?
            .into_string()
            .map_err(|e| format!("{}: {}", self.name, e))?;
        let (usd_e8, source_updated_at) = (self.parse)(&body).map_err(|e| format!("{}: {}", self.name, e))?;
        Ok(Price { usd_e8, fetched_at: now, source_updated_at, source: self.name.to_string() })
    }
}

/// PIVX Labs oracle first (or `PIVX_PRICE_API`, labelled "custom"),
/// CoinGecko second.
pub fn default_sources() -> Vec<HttpSource> {
    let custom = std::env::var("PIVX_PRICE_API").ok().filter(|s| !s.is_empty());
    let (name, url) = match custom {
        Some(url) => ("custom", url),
        None => ("oracle", ORACLE_URL.to_string()),
    };
    vec![
        HttpSource { name, url, parse: parse_oracle },
        HttpSource { name: "coingecko", url: COINGECKO_URL.to_string(), parse: parse_coingecko },
    ]
}

fn with_default_sources<T>(f: impl FnOnce(&[&dyn PriceSource]) -> T) -> T {
    let sources = default_sources();
    let refs: Vec<&dyn PriceSource> = sources.iter().map(|s| s as &dyn PriceSource).collect();
    f(&refs)
}

/// [`for_spending_with`] against the real sources and cache.
pub fn for_spending() -> Result<Price, String> {
    with_default_sources(|s| for_spending_with(s, &PriceCache::open_default(), crate::core::now_secs()))
}

/// [`for_display_with`] against the real sources and cache.
pub fn for_display() -> Option<Price> {
    with_default_sources(|s| for_display_with(s, &PriceCache::open_default(), crate::core::now_secs()))
}

/// What the price cache file holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheFile {
    /// Last price accepted for converting a spend.
    #[serde(default)]
    pub spend: Option<Price>,
    /// Reference the sanity band is measured from; replaced only once
    /// it's older than [`BAND_WINDOW`], and then only by a price two
    /// sources agree on.
    #[serde(default)]
    pub anchor: Option<Price>,
    /// Last price fetched for display. Never used to convert a spend.
    #[serde(default)]
    pub display: Option<Price>,
    #[serde(default)]
    pub display_failed_at: Option<u64>,
}

/// A spending-grade price on disk, MAC'd with a device-bound key so a
/// hand-written or edited entry is ignored rather than trusted.
#[derive(Serialize, Deserialize)]
struct Signed {
    price: Price,
    mac: String,
}

#[derive(Default, Serialize, Deserialize)]
struct DiskCache {
    #[serde(default)]
    spend: Option<Signed>,
    #[serde(default)]
    anchor: Option<Signed>,
    #[serde(default)]
    display: Option<Price>,
    #[serde(default)]
    display_failed_at: Option<u64>,
}

pub struct PriceCache {
    path: PathBuf,
    key: [u8; 32],
}

impl PriceCache {
    pub fn open_default() -> Self {
        // Without a device key (unreadable machine id), use a random
        // per-process key: nothing on disk verifies, so cached prices are
        // never trusted across invocations.
        let key = crate::wallet::device_subkey(b"pivx-agent-kit-price-mac").unwrap_or_else(|_| {
            use rand_core::{OsRng, RngCore};
            let mut k = [0u8; 32];
            OsRng.fill_bytes(&mut k);
            k
        });
        Self::at(crate::wallet::get_data_dir().join(PRICE_FILE), key)
    }

    pub fn at(path: PathBuf, key: [u8; 32]) -> Self {
        Self { path, key }
    }

    /// The cache, or an empty one if it's missing or unreadable. Spend
    /// and anchor entries whose MAC doesn't verify are dropped.
    pub fn load(&self) -> CacheFile {
        let disk: DiskCache = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        CacheFile {
            spend: disk.spend.and_then(|s| self.verify(s)),
            anchor: disk.anchor.and_then(|s| self.verify(s)),
            display: disk.display,
            display_failed_at: disk.display_failed_at,
        }
    }

    pub fn save(&self, f: &CacheFile) -> Result<(), Box<dyn Error>> {
        let disk = DiskCache {
            spend: f.spend.as_ref().map(|p| self.sign(p)).transpose()?,
            anchor: f.anchor.as_ref().map(|p| self.sign(p)).transpose()?,
            display: f.display.clone(),
            display_failed_at: f.display_failed_at,
        };
        crate::policy::write_private(&self.path, serde_json::to_string_pretty(&disk)?.as_bytes())
    }

    fn mac(&self, p: &Price) -> Result<Hmac<Sha256>, Box<dyn Error>> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key)?;
        m.update(&serde_json::to_vec(p)?);
        Ok(m)
    }

    fn sign(&self, p: &Price) -> Result<Signed, Box<dyn Error>> {
        let tag = self.mac(p)?.finalize().into_bytes();
        Ok(Signed { price: p.clone(), mac: tag.iter().map(|b| format!("{:02x}", b)).collect() })
    }

    fn verify(&self, s: Signed) -> Option<Price> {
        let given: Option<Vec<u8>> = (0..s.mac.len())
            .step_by(2)
            .map(|i| s.mac.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect();
        let m = self.mac(&s.price).ok()?;
        m.verify_slice(&given?).ok()?;
        Some(s.price)
    }
}

/// A price fit to convert a spend.
///
/// - A spend price accepted in the last [`SPEND_MAX_AGE`] is reused.
/// - With an anchor younger than [`BAND_WINDOW`]: the first source whose
///   price is plausible and within the band of the anchor wins.
/// - Without one: at least two sources must answer plausibly and the
///   first must agree with another within [`AGREE_PERCENT`]; that price
///   becomes the new anchor.
///
/// Nothing is cached unless a price is accepted.
pub fn for_spending_with(sources: &[&dyn PriceSource], cache: &PriceCache, now: u64) -> Result<Price, String> {
    let mut file = cache.load();
    if let Some(p) = file.spend.as_ref().filter(|p| young(p, now, SPEND_MAX_AGE)) {
        return Ok(p.clone());
    }
    let anchor = file.anchor.clone().filter(|a| young(a, now, BAND_WINDOW));

    let mut errors = Vec::new();
    let accepted = match &anchor {
        Some(anchor) => {
            let mut chosen: Option<(usize, Price)> = None;
            for (i, src) in sources.iter().enumerate() {
                match src.fetch(now).and_then(|p| check_for_spending(&p, Some(anchor), now).map(|_| p)) {
                    Ok(p) => {
                        chosen = Some((i, p));
                        break;
                    }
                    Err(e) => errors.push(e),
                }
            }
            // Cross-check with the remaining sources: one that answers
            // plausibly and disagrees means a feed is off. One that's
            // down doesn't block.
            if let Some((i, p)) = &chosen {
                for src in &sources[i + 1..] {
                    if let Ok(other) = src.fetch(now).and_then(|o| plausible(&o, now).map(|_| o)) {
                        if !within(other.usd_e8, p.usd_e8, AGREE_PERCENT) {
                            return Err(format!(
                                "PIV price sources disagree by more than {}% ({} ${} vs {} ${}); refusing to convert dollars. \
                                 Use a PIV amount or retry later",
                                AGREE_PERCENT,
                                p.source,
                                format_price(p.usd_e8),
                                other.source,
                                format_price(other.usd_e8)
                            ));
                        }
                    }
                }
            }
            chosen.map(|(_, p)| p)
        }
        None => {
            let mut answers = Vec::new();
            for src in sources {
                match src.fetch(now).and_then(|p| plausible(&p, now).map(|_| p)) {
                    Ok(p) => answers.push(p),
                    Err(e) => errors.push(e),
                }
            }
            match answers.split_first() {
                Some((first, rest)) if !rest.is_empty() => {
                    if rest.iter().any(|o| within(o.usd_e8, first.usd_e8, AGREE_PERCENT)) {
                        Some(first.clone())
                    } else {
                        let all: Vec<String> =
                            answers.iter().map(|p| format!("{} ${}", p.source, format_price(p.usd_e8))).collect();
                        return Err(format!(
                            "PIV price sources disagree by more than {}% ({}); refusing to convert dollars. \
                             Use a PIV amount or retry later",
                            AGREE_PERCENT,
                            all.join(" vs ")
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "can't cross-check the PIV price: need two sources, got {} ({}). Use a PIV amount or retry later",
                        answers.len(),
                        if errors.is_empty() { "no errors".to_string() } else { errors.join("; ") }
                    ))
                }
            }
        }
    };
    let Some(price) = accepted else {
        return Err(format!("PIV price unavailable: {}", errors.join("; ")));
    };
    file.spend = Some(price.clone());
    if anchor.is_none() {
        file.anchor = Some(price.clone());
    }
    if let Err(e) = cache.save(&file) {
        eprintln!("warning: could not cache price: {}", e);
    }
    Ok(price)
}

/// Best-effort price for display: a young display or spend price, else
/// the first plausible source. A failed fetch is remembered for
/// [`DISPLAY_RETRY_AFTER`] so a dead feed doesn't slow every call.
/// Never touches the spend price or the anchor.
pub fn for_display_with(sources: &[&dyn PriceSource], cache: &PriceCache, now: u64) -> Option<Price> {
    let file = cache.load();
    let cached = [file.spend.as_ref(), file.display.as_ref()]
        .into_iter()
        .flatten()
        .filter(|p| young(p, now, DISPLAY_MAX_AGE))
        .max_by_key(|p| p.fetched_at)
        .cloned();
    if cached.is_some() {
        return cached;
    }
    if file.display_failed_at.is_some_and(|t| t <= now && now - t < DISPLAY_RETRY_AFTER) {
        return None;
    }
    let fetched = sources
        .iter()
        .find_map(|src| src.fetch(now).ok().filter(|p| plausible(p, now).is_ok()));
    // Re-read before saving and touch only the display fields: another
    // process may have accepted a spend price during the fetch.
    let mut latest = cache.load();
    match &fetched {
        Some(p) => {
            latest.display = Some(p.clone());
            latest.display_failed_at = None;
        }
        None => latest.display_failed_at = Some(now),
    }
    let _ = cache.save(&latest);
    fetched
}

/// The price a payment uses for its record and its dollar limits, plus
/// why there is none. A fresh dollar request uses the price it was
/// converted at. An approved replay (`replay`) and PIV requests use a
/// freshly checked price — a stored request-time price may be a day old
/// and must not decide whether a cap is exceeded.
pub fn price_for_payment(
    conversion: Option<&Price>,
    replay: bool,
    fresh: impl FnOnce() -> Result<Price, String>,
) -> (Option<Price>, Option<String>) {
    if let (Some(p), false) = (conversion, replay) {
        return (Some(p.clone()), None);
    }
    match fresh() {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e)),
    }
}

/// `balance` additions: USD value of each pool and the price used.
/// Empty when no price is available (fields are omitted, not null).
pub fn balance_usd_fields(private_sat: u64, public_sat: u64, price: Option<&Price>) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    if let Some(p) = price {
        let usd = |sat: u64| Value::from(format_usd(sat_to_usd_e8(sat, p.usd_e8)));
        m.insert("private_balance_usd".into(), usd(private_sat));
        m.insert("public_balance_usd".into(), usd(public_sat));
        m.insert("total_balance_usd".into(), usd(private_sat.saturating_add(public_sat)));
        m.insert("price_usd".into(), Value::from(format_price(p.usd_e8)));
    }
    m
}

/// Send-output additions: `amount_usd`, `price_usd`, `price_source`.
pub fn amount_usd_fields(amount_sat: u64, price: Option<&Price>) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    if let Some(p) = price {
        m.insert("amount_usd".into(), Value::from(format_usd(sat_to_usd_e8(amount_sat, p.usd_e8))));
        m.insert("price_usd".into(), Value::from(format_price(p.usd_e8)));
        m.insert("price_source".into(), Value::from(p.source.clone()));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const NOW: u64 = 1_790_880_000;
    const P: u64 = 6_888_275; // $0.06888275 per PIV

    fn price(usd_e8: u64, fetched_at: u64) -> Price {
        Price { usd_e8, fetched_at, source_updated_at: fetched_at, source: "test".into() }
    }

    struct Fixed {
        answer: Result<Price, String>,
        calls: Cell<u32>,
    }

    impl Fixed {
        fn ok(p: Price) -> Self {
            Self { answer: Ok(p), calls: Cell::new(0) }
        }
        fn err(e: &str) -> Self {
            Self { answer: Err(e.into()), calls: Cell::new(0) }
        }
    }

    impl PriceSource for Fixed {
        fn fetch(&self, _now: u64) -> Result<Price, String> {
            self.calls.set(self.calls.get() + 1);
            self.answer.clone()
        }
    }

    fn cache(tag: &str) -> PriceCache {
        PriceCache::at(crate::ledger::tests::temp_dir(tag).join(PRICE_FILE), [9u8; 32])
    }

    // -------- parse_amount ----------------------------------------

    #[test]
    fn amount_plain_and_piv_suffix_are_piv() {
        assert_eq!(parse_amount("5").unwrap(), Amount::Piv(500_000_000));
        assert_eq!(parse_amount("0.5").unwrap(), Amount::Piv(50_000_000));
        assert_eq!(parse_amount("5piv").unwrap(), Amount::Piv(500_000_000));
        assert_eq!(parse_amount("5 PIV").unwrap(), Amount::Piv(500_000_000));
    }

    #[test]
    fn amount_dollar_forms() {
        for s in ["$5", "$ 5", "5usd", "5 USD", "5Usd", " $5.00 "] {
            assert_eq!(parse_amount(s).unwrap(), Amount::Usd(500_000_000), "{s}");
        }
        assert_eq!(parse_amount("$0.10").unwrap(), Amount::Usd(10_000_000));
        assert_eq!(parse_amount("$0.00000001").unwrap(), Amount::Usd(1));
    }

    #[test]
    fn amount_rejects_garbage() {
        for s in ["", "$", "usd", "piv", "$-5", "-5", "5 usd usd", "$5usd", "5 eur", "five", "$1.123456789", "0", "$0", "0usd", "5 $"] {
            assert!(parse_amount(s).is_err(), "should reject {s:?}");
        }
    }

    // -------- conversions -----------------------------------------

    #[test]
    fn float_to_e8_rounds_once() {
        assert_eq!(float_to_e8(0.06888275).unwrap(), P);
        assert_eq!(float_to_e8(1.0).unwrap(), 100_000_000);
        assert_eq!(float_to_e8(0.069013).unwrap(), 6_901_300);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e-10] {
            assert!(float_to_e8(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn usd_to_sat_known_values() {
        // $5 at $0.06888275 = 72.58711361 PIV (rounded half up at the sat).
        assert_eq!(usd_to_sat(500_000_000, P).unwrap(), 7_258_711_361);
        // $1 at $1 = 1 PIV exactly.
        assert_eq!(usd_to_sat(100_000_000, 100_000_000).unwrap(), 100_000_000);
        // Half-up: $0.000000015 at $1 → 1.5 sat → 2.
        assert_eq!(usd_to_sat(3, 200_000_000).unwrap(), 2);
    }

    #[test]
    fn usd_to_sat_rejects_zero_results_and_bad_price() {
        // $0.00000001 at $1000/PIV → 0.001 sat → rounds to 0.
        assert!(usd_to_sat(1, 100_000_000_000).is_err());
        assert!(usd_to_sat(500_000_000, 0).is_err());
    }

    #[test]
    fn usd_to_sat_rejects_overflow() {
        // Absurd dollar amount at a near-zero price overflows u64 sat.
        assert!(usd_to_sat(u64::MAX, 1).is_err());
    }

    #[test]
    fn sat_to_usd_round_trip_is_close() {
        let sat = usd_to_sat(500_000_000, P).unwrap();
        let back = sat_to_usd_e8(sat, P);
        assert!(back.abs_diff(500_000_000) <= 1, "{back}");
        assert_eq!(sat_to_usd_e8(100_000_000, P), P);
        assert_eq!(sat_to_usd_e8(0, P), 0);
        // No overflow at the extremes.
        let _ = sat_to_usd_e8(u64::MAX, u64::MAX);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_usd(500_000_000), "5.00");
        assert_eq!(format_usd(8_265_900_000), "82.66"); // 82.659 rounds up
        assert_eq!(format_usd(499_999), "0.00"); // just under half a cent
        assert_eq!(format_usd(500_000), "0.01"); // exactly half a cent rounds up
        assert_eq!(format_usd(0), "0.00");
        assert_eq!(format_price(P), "0.06888275");
        assert_eq!(format_price(100_000_000), "1.00000000");
    }

    // -------- output fields ---------------------------------------

    #[test]
    fn balance_fields_with_price() {
        let p = price(P, NOW);
        let f = balance_usd_fields(115_000_000_000, 5_000_000_000, Some(&p));
        assert_eq!(f["private_balance_usd"], "79.22");
        assert_eq!(f["public_balance_usd"], "3.44");
        assert_eq!(f["total_balance_usd"], "82.66");
        assert_eq!(f["price_usd"], "0.06888275");
        assert_eq!(f.len(), 4);
    }

    #[test]
    fn balance_fields_absent_without_price() {
        assert!(balance_usd_fields(1, 2, None).is_empty());
    }

    #[test]
    fn amount_fields() {
        let p = price(P, NOW);
        let f = amount_usd_fields(7_258_711_361, Some(&p));
        assert_eq!(f["amount_usd"], "5.00");
        assert_eq!(f["price_usd"], "0.06888275");
        assert_eq!(f["price_source"], "test");
        assert!(amount_usd_fields(1, None).is_empty());
    }

    // -------- source parsing --------------------------------------

    #[test]
    fn parses_oracle_object_and_array() {
        let obj = r#"{"currency":"usd","value":0.06890275,"last_updated":1790880333}"#;
        assert_eq!(parse_oracle(obj).unwrap(), (6_890_275, 1_790_880_333));
        let arr = r#"[{"currency":"btc","value":8.1e-7,"last_updated":1},{"currency":"usd","value":0.06888275,"last_updated":1790879206}]"#;
        assert_eq!(parse_oracle(arr).unwrap(), (P, 1_790_879_206));
    }

    #[test]
    fn oracle_rejects_wrong_currency_or_shape() {
        assert!(parse_oracle(r#"{"currency":"eur","value":0.06,"last_updated":1}"#).is_err());
        assert!(parse_oracle(r#"[{"currency":"btc","value":1,"last_updated":1}]"#).is_err());
        assert!(parse_oracle(r#"{"currency":"usd","value":0,"last_updated":1}"#).is_err());
        assert!(parse_oracle(r#"{"currency":"usd","value":"0.06","last_updated":1}"#).is_err());
        assert!(parse_oracle("<html>").is_err());
    }

    #[test]
    fn parses_coingecko() {
        let body = r#"{"pivx":{"usd":0.069013,"last_updated_at":1790880230}}"#;
        assert_eq!(parse_coingecko(body).unwrap(), (6_901_300, 1_790_880_230));
        assert!(parse_coingecko(r#"{"pivx":{}}"#).is_err());
        assert!(parse_coingecko(r#"{}"#).is_err());
    }

    // -------- spending checks -------------------------------------

    #[test]
    fn spending_accepts_fresh_price_without_anchor() {
        assert!(check_for_spending(&price(P, NOW), None, NOW).is_ok());
    }

    #[test]
    fn spending_rejects_stale_source() {
        let mut p = price(P, NOW);
        p.source_updated_at = NOW - SOURCE_STALE_AFTER - 1;
        let err = check_for_spending(&p, None, NOW).unwrap_err();
        assert!(err.contains("stale"), "{err}");
        p.source_updated_at = NOW - SOURCE_STALE_AFTER;
        assert!(check_for_spending(&p, None, NOW).is_ok());
    }

    #[test]
    fn spending_rejects_future_source_timestamps() {
        let mut p = price(P, NOW);
        p.source_updated_at = NOW + MAX_CLOCK_SKEW + 1;
        assert!(check_for_spending(&p, None, NOW).unwrap_err().contains("future"));
        p.source_updated_at = 9_999_999_999;
        assert!(check_for_spending(&p, None, NOW).is_err());
        p.source_updated_at = NOW + MAX_CLOCK_SKEW;
        assert!(check_for_spending(&p, None, NOW).is_ok());
    }

    #[test]
    fn spending_rejects_jumps_beyond_band_of_anchor() {
        let anchor = price(100_000_000, NOW - 600);
        assert!(check_for_spending(&price(125_000_000, NOW), Some(&anchor), NOW).is_ok());
        assert!(check_for_spending(&price(75_000_000, NOW), Some(&anchor), NOW).is_ok());
        let err = check_for_spending(&price(125_000_001, NOW), Some(&anchor), NOW).unwrap_err();
        assert!(err.contains("25%"), "{err}");
        assert!(check_for_spending(&price(74_999_999, NOW), Some(&anchor), NOW).is_err());
        assert!(check_for_spending(&price(4_000_000, NOW), Some(&anchor), NOW).is_err());
    }

    #[test]
    fn has_unit_detects_explicit_units() {
        for s in ["$5", "5usd", "5 USD", "5piv", "5 PIV"] {
            assert!(has_unit(s), "{s}");
        }
        for s in ["5", "0.5", " 5 "] {
            assert!(!has_unit(s), "{s}");
        }
    }

    // -------- cache -----------------------------------------------

    #[test]
    fn cache_round_trips_and_tolerates_missing_or_corrupt() {
        let c = cache("price-rt");
        assert_eq!(c.load(), CacheFile::default());
        let f = CacheFile { spend: Some(price(P, NOW)), anchor: Some(price(P, NOW)), ..Default::default() };
        c.save(&f).unwrap();
        assert_eq!(c.load(), f);
        std::fs::write(&c.path, "garbage").unwrap();
        assert_eq!(c.load(), CacheFile::default());
    }

    #[test]
    fn temp_paths_are_unique_per_write() {
        // Two processes saving at once must not share a temp path.
        let p = std::path::Path::new("/x/price_cache.json");
        let (a, b) = (crate::policy::temp_path_for(p), crate::policy::temp_path_for(p));
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains(&std::process::id().to_string()), "{}", a.display());
        assert_eq!(a.parent(), p.parent(), "same directory, so the rename is atomic");
    }

    #[test]
    fn cache_write_leaves_no_temp_files() {
        let c = cache("price-tmp");
        c.save(&CacheFile::default()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(c.path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    // -------- for_spending_with -----------------------------------

    fn seeded(tag: &str, f: CacheFile) -> PriceCache {
        let c = cache(tag);
        c.save(&f).unwrap();
        c
    }

    #[test]
    fn spending_uses_young_spend_price_without_fetching() {
        let c = seeded("sp-young", CacheFile { spend: Some(price(P, NOW - SPEND_MAX_AGE)), ..Default::default() });
        let src = Fixed::err("should not be called");
        assert_eq!(for_spending_with(&[&src], &c, NOW).unwrap().usd_e8, P);
        assert_eq!(src.calls.get(), 0);
    }

    #[test]
    fn spending_ignores_cache_from_the_future() {
        // Clock was ahead, then corrected: a "young" entry from the future isn't trusted.
        let c = seeded(
            "sp-future-cache",
            CacheFile { spend: Some(price(P, NOW + 3_600)), anchor: Some(price(P, NOW + 3_600)), ..Default::default() },
        );
        let a = Fixed::ok(price(P + 1_000, NOW));
        let b = Fixed::ok(price(P + 2_000, NOW));
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().usd_e8, P + 1_000);
        assert_eq!(a.calls.get(), 1);
    }

    #[test]
    fn without_anchor_two_sources_must_agree() {
        // The reviewer's repro: first run, oracle ~100x too low.
        let c = cache("sp-no-anchor-disagree");
        let bad = Fixed::ok(price(67_000, NOW));
        let good = Fixed::ok(price(P, NOW));
        let err = for_spending_with(&[&bad, &good], &c, NOW).unwrap_err();
        assert!(err.contains("disagree"), "{err}");
        assert_eq!(c.load(), CacheFile::default(), "nothing may be cached");
    }

    #[test]
    fn without_anchor_a_single_source_is_not_enough() {
        let c = cache("sp-no-anchor-single");
        let only = Fixed::ok(price(P, NOW));
        let down = Fixed::err("coingecko down");
        let err = for_spending_with(&[&only, &down], &c, NOW).unwrap_err();
        assert!(err.contains("cross-check"), "{err}");
    }

    #[test]
    fn without_anchor_agreeing_sources_set_the_anchor() {
        let c = cache("sp-no-anchor-agree");
        let a = Fixed::ok(price(P, NOW));
        let b = Fixed::ok(price(P * 105 / 100, NOW)); // within 10%
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().usd_e8, P);
        let f = c.load();
        assert_eq!(f.spend.unwrap().usd_e8, P);
        assert_eq!(f.anchor.unwrap().usd_e8, P);
    }

    #[test]
    fn with_anchor_one_source_in_band_is_enough() {
        let c = seeded("sp-anchor", CacheFile { anchor: Some(price(P, NOW - 600)), ..Default::default() });
        let a = Fixed::ok(price(P + 10_000, NOW));
        let b = Fixed::err("coingecko down");
        // The second source is asked; it being down doesn't block.
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().usd_e8, P + 10_000);
        assert_eq!(b.calls.get(), 1);
        let f = c.load();
        assert_eq!(f.anchor.unwrap().usd_e8, P, "the anchor stays put for its window");
        assert_eq!(f.spend.unwrap().usd_e8, P + 10_000);
    }

    #[test]
    fn with_anchor_a_disagreeing_second_source_blocks() {
        // A biased primary within the band is caught when the other source answers.
        let c = seeded("sp-anchor-bias", CacheFile { anchor: Some(price(P, NOW - 600)), ..Default::default() });
        let biased = Fixed::ok(price(P * 76 / 100, NOW)); // -24%: inside the band
        let honest = Fixed::ok(price(P, NOW));
        let err = for_spending_with(&[&biased, &honest], &c, NOW).unwrap_err();
        assert!(err.contains("disagree"), "{err}");
        assert_eq!(c.load().spend, None);
    }

    // -------- signed cache entries ---------------------------------

    #[test]
    fn forged_spend_price_is_ignored() {
        // The reviewer's repro: hand-write a near-zero spend price into the cache.
        let c = cache("sp-forged");
        let forged = serde_json::json!({
            "spend": {"price": {"usd_e8": 1, "fetched_at": NOW, "source_updated_at": NOW, "source": "oracle"}, "mac": "00"},
            "anchor": {"price": {"usd_e8": 1, "fetched_at": NOW, "source_updated_at": NOW, "source": "oracle"}, "mac": "00"}
        });
        std::fs::write(&c.path, forged.to_string()).unwrap();
        assert_eq!(c.load().spend, None);
        assert_eq!(c.load().anchor, None);
        let a = Fixed::ok(price(P, NOW));
        let b = Fixed::ok(price(P, NOW));
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().usd_e8, P);
    }

    #[test]
    fn edited_signed_entry_is_ignored() {
        let c = seeded("sp-edited", CacheFile { spend: Some(price(P, NOW)), anchor: Some(price(P, NOW)), ..Default::default() });
        let text = std::fs::read_to_string(&c.path).unwrap().replace(&P.to_string(), "1");
        std::fs::write(&c.path, text).unwrap();
        assert_eq!(c.load().spend, None);
        assert_eq!(c.load().anchor, None);
    }

    #[test]
    fn entries_signed_with_another_key_are_ignored() {
        let c = seeded("sp-otherkey", CacheFile { spend: Some(price(P, NOW)), ..Default::default() });
        let other = PriceCache::at(c.path.clone(), [1u8; 32]);
        assert_eq!(other.load().spend, None);
        assert_eq!(c.load().spend.unwrap().usd_e8, P);
    }

    // -------- display path doesn't clobber spending state ------------

    struct WritesSpend<'a> {
        cache: &'a PriceCache,
    }

    impl PriceSource for WritesSpend<'_> {
        fn fetch(&self, now: u64) -> Result<Price, String> {
            // Meanwhile another process accepts a spend price.
            let mut f = self.cache.load();
            f.spend = Some(price(P, now));
            f.anchor = Some(price(P, now));
            self.cache.save(&f).unwrap();
            Ok(price(P + 7, now))
        }
    }

    #[test]
    fn display_save_keeps_concurrent_spend_updates() {
        let c = cache("disp-merge");
        let src = WritesSpend { cache: &c };
        assert_eq!(for_display_with(&[&src], &c, NOW).unwrap().usd_e8, P + 7);
        let f = c.load();
        assert_eq!(f.spend.unwrap().usd_e8, P, "display must not overwrite the spend entry");
        assert_eq!(f.anchor.unwrap().usd_e8, P);
        assert_eq!(f.display.unwrap().usd_e8, P + 7);
    }

    #[test]
    fn a_source_failing_checks_falls_through_to_the_next() {
        let c = seeded("sp-fallthrough", CacheFile { anchor: Some(price(P, NOW - 600)), ..Default::default() });
        let mut stale = price(P, NOW);
        stale.source_updated_at = NOW - 10 * 86_400;
        let a = Fixed::ok(stale);
        let b = Fixed::ok(price(P, NOW));
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().source, "test");
        assert_eq!(b.calls.get(), 1);
    }

    #[test]
    fn band_is_measured_from_the_anchor_not_the_last_step() {
        // A feed walking the price down 24% per fetch must be stopped.
        let c = seeded("sp-walk", CacheFile { anchor: Some(price(100_000_000, NOW - 600)), ..Default::default() });
        let step1 = Fixed::ok(price(76_000_000, NOW));
        assert!(for_spending_with(&[&step1], &c, NOW).is_ok());
        let step2 = Fixed::ok(price(57_760_000, NOW + 61)); // another -24%
        assert!(for_spending_with(&[&step2], &c, NOW + 61).is_err());
    }

    #[test]
    fn anchor_expires_after_the_window_then_needs_agreement_again() {
        let c = seeded("sp-anchor-expiry", CacheFile { anchor: Some(price(100_000_000, NOW - BAND_WINDOW - 1)), ..Default::default() });
        let a = Fixed::ok(price(10_000_000, NOW));
        let b = Fixed::ok(price(10_100_000, NOW));
        assert_eq!(for_spending_with(&[&a, &b], &c, NOW).unwrap().usd_e8, 10_000_000);
        assert_eq!(c.load().anchor.unwrap().usd_e8, 10_000_000);
    }

    #[test]
    fn band_rejection_caches_nothing() {
        let c = seeded("sp-band", CacheFile { anchor: Some(price(100_000_000, NOW - 300)), ..Default::default() });
        let src = Fixed::ok(price(10_000_000, NOW));
        assert!(for_spending_with(&[&src], &c, NOW).is_err());
        let f = c.load();
        assert_eq!(f.anchor.unwrap().usd_e8, 100_000_000);
        assert_eq!(f.spend, None);
    }

    #[test]
    fn spending_errors_when_all_sources_fail() {
        let c = cache("sp-down");
        let a = Fixed::err("oracle down");
        let b = Fixed::err("coingecko down");
        let err = for_spending_with(&[&a, &b], &c, NOW).unwrap_err();
        assert!(err.contains("oracle down") && err.contains("coingecko down"), "{err}");
    }

    // -------- for_display_with ------------------------------------

    #[test]
    fn display_uses_cached_display_price_up_to_ten_minutes() {
        let c = seeded("disp-cache", CacheFile { display: Some(price(P, NOW - DISPLAY_MAX_AGE)), ..Default::default() });
        let src = Fixed::err("unused");
        assert_eq!(for_display_with(&[&src], &c, NOW).unwrap().usd_e8, P);
        assert_eq!(src.calls.get(), 0);
    }

    #[test]
    fn display_can_reuse_a_recent_spend_price() {
        let c = seeded("disp-spend", CacheFile { spend: Some(price(P, NOW - 30)), ..Default::default() });
        let src = Fixed::err("unused");
        assert_eq!(for_display_with(&[&src], &c, NOW).unwrap().usd_e8, P);
    }

    #[test]
    fn display_never_touches_the_spending_cache() {
        let c = seeded("disp-isolated", CacheFile { anchor: Some(price(P, NOW - 300)), ..Default::default() });
        let src = Fixed::ok(price(67_000, NOW));
        assert_eq!(for_display_with(&[&src], &c, NOW).unwrap().usd_e8, 67_000);
        let f = c.load();
        assert_eq!(f.anchor.unwrap().usd_e8, P);
        assert_eq!(f.spend, None);
        assert_eq!(f.display.unwrap().usd_e8, 67_000);
    }

    #[test]
    fn display_skips_stale_or_future_prices() {
        let c = cache("disp-stale");
        let mut stale = price(P, NOW);
        stale.source_updated_at = NOW - 10 * 86_400;
        let a = Fixed::ok(stale);
        let b = Fixed::ok(price(P + 5, NOW));
        assert_eq!(for_display_with(&[&a, &b], &c, NOW).unwrap().usd_e8, P + 5);
    }

    #[test]
    fn display_remembers_failure_for_a_minute() {
        let c = cache("disp-negative");
        let src = Fixed::err("down");
        assert_eq!(for_display_with(&[&src], &c, NOW), None);
        assert_eq!(for_display_with(&[&src], &c, NOW + DISPLAY_RETRY_AFTER - 1), None);
        assert_eq!(src.calls.get(), 1, "second call within the minute must not fetch");
        assert_eq!(for_display_with(&[&src], &c, NOW + DISPLAY_RETRY_AFTER), None);
        assert_eq!(src.calls.get(), 2);
    }

    // -------- price for a payment ----------------------------------

    #[test]
    fn dollar_request_uses_its_conversion_price() {
        let conv = price(P, NOW);
        let (p, why) = price_for_payment(Some(&conv), false, || panic!("no fetch needed"));
        assert_eq!((p, why), (Some(conv), None));
    }

    #[test]
    fn replay_ignores_the_stored_price() {
        let stale = price(P, NOW - 86_000);
        let (p, _) = price_for_payment(Some(&stale), true, || Ok(price(P * 3 / 2, NOW)));
        assert_eq!(p.unwrap().usd_e8, P * 3 / 2);
        let (p, why) = price_for_payment(Some(&stale), true, || Err("need two sources".into()));
        assert_eq!(p, None, "fail closed: no stale fallback");
        assert!(why.unwrap().contains("two sources"));
    }

    #[test]
    fn piv_request_uses_a_fresh_checked_price_and_keeps_the_error() {
        let (p, why) = price_for_payment(None, false, || Ok(price(P, NOW)));
        assert_eq!((p.map(|x| x.usd_e8), why), (Some(P), None));
        let (p, why) = price_for_payment(None, false, || Err("coingecko: 429".into()));
        assert_eq!(p, None);
        assert!(why.unwrap().contains("429"));
    }
}
