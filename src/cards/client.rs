//! HTTP client for PIVCards (`cards.pivxla.bz`).
//!
//! No authentication: PIVCards keeps order privacy by making the
//! 32-byte random order ID itself the only handle a caller needs.
//! That ID is what's persisted in `cards_state.json` so the agent
//! can resume an in-flight purchase across kit invocations.

use serde_json::Value;
use std::error::Error;

/// Production endpoint. Override with `PIVCARDS_API` for staging /
/// local-dev / forks.
const DEFAULT_BASE: &str = "https://cards.pivxla.bz";

pub struct Client {
    base: String,
}

impl Client {
    pub fn new() -> Self {
        let base = std::env::var("PIVCARDS_API")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        Self { base }
    }

    /// GET an upstream path. Search / details endpoints reach Bitrefill
    /// through PIVCards' regional proxies and can take 10-30s on a cold
    /// `cf_clearance`; routine cached endpoints respond instantly.
    pub fn get(&self, path: &str) -> Result<Value, Box<dyn Error>> {
        let url = format!("{}{}", self.base, path);
        // 60s ceiling — enough for a Turnstile solve plus the round-trip
        // to Bitrefill, with a small safety margin.
        let resp = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .get(&url)
            .call();
        decode_json(resp)
    }
}

/// Convert a ureq response into either the parsed JSON body or a
/// readable error. PIVCards uses `200 + plain string body` for some
/// human-error paths (e.g. `/order/check` returning `"Order not
/// found!"` as text/plain on a missing ID), so a JSON-parse failure
/// is surfaced verbatim rather than swallowed.
fn decode_json(resp: Result<ureq::Response, ureq::Error>) -> Result<Value, Box<dyn Error>> {
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.into_string().unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if status >= 400 {
                    return Err(format!("HTTP {}: {}", status, body).into());
                }
                Ok(v)
            } else if status >= 400 {
                Err(format!("HTTP {}: {}", status, body).into())
            } else {
                // Non-JSON 2xx — treat as message string. PIVCards
                // returns plain text for some error-shaped successes
                // (e.g. cancel-on-waiting). Surface it cleanly.
                Err(body.into())
            }
        }
        Err(ureq::Error::Status(status, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(format!("HTTP {}: {}", status, body).into())
        }
        Err(e) => Err(format!("network error: {}", e).into()),
    }
}

/// URL-escape a query parameter value. Keep it dependency-free —
/// search/details accept enough characters that hand-rolled escaping
/// covers the realistic set (alphanum + `-_.~` are safe; everything
/// else gets percent-encoded).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
