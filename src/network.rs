use std::error::Error;
use std::io::Read;
use std::time::Duration;

const RPC_NODES: &[&str] = &[
    "https://rpc.pivxla.bz/mainnet",
    "https://rpc.duddino.com/mainnet",
    "https://rpc2.duddino.com/mainnet",
];

const EXPLORERS: &[&str] = &[
    "https://explorer.pivxla.bz",
    "https://explorer.duddino.com",
    "https://explorer2.duddino.com",
];

/// Sapling params download URLs
const SAPLING_PARAM_HOSTS: &[&str] = &["https://pivxla.bz", "https://duddino.com"];

/// Timeout for standard HTTP requests
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle read timeout for streaming connections (no data for this long = dead)
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PivxNetwork {
    rpc_nodes: &'static [&'static str],
    explorers: &'static [&'static str],
}

impl PivxNetwork {
    pub fn new() -> Self {
        Self {
            rpc_nodes: RPC_NODES,
            explorers: EXPLORERS,
        }
    }

    /// Try an RPC call across all configured nodes, returning the first success
    fn rpc_get(&self, path: &str) -> Result<String, Box<dyn Error>> {
        let mut last_err = String::from("No RPC nodes configured");
        for node in self.rpc_nodes {
            let url = format!("{}{}", node, path);
            match ureq::get(&url).timeout(REQUEST_TIMEOUT).call() {
                Ok(resp) => return Ok(resp.into_string()?),
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err.into())
    }

    /// Get the current block count
    pub fn get_block_count(&self) -> Result<u32, Box<dyn Error>> {
        let resp = self.rpc_get("/getblockcount")?;
        let count: u32 = resp.trim().parse()?;
        Ok(count)
    }

    /// Get block data by height (used for sapling root validation)
    pub fn get_block(&self, height: u32) -> Result<serde_json::Value, Box<dyn Error>> {
        let hash_resp = self.rpc_get(&format!("/getblockhash?params={}", height))?;
        let hash: String = serde_json::from_str(&hash_resp)?;
        let block_resp = self.rpc_get(&format!("/getblock?params={},1", hash))?;
        let block: serde_json::Value = serde_json::from_str(&block_resp)?;
        Ok(block)
    }

    /// Get binary shield data stream starting from a block height
    pub fn get_shield_data(
        &self,
        start_block: u32,
    ) -> Result<Box<dyn Read + Send>, Box<dyn Error>> {
        let mut last_err = String::from("No RPC nodes configured");
        for node in self.rpc_nodes {
            let url = format!("{}/getshielddata?startBlock={}&format=compact", node, start_block);
            let agent = ureq::AgentBuilder::new()
                .timeout_read(STREAM_READ_TIMEOUT)
                .build();
            match agent.get(&url).call() {
                Ok(resp) => return Ok(Box::new(resp.into_reader())),
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err.into())
    }

    /// Fetch confirmed UTXOs for a transparent address via Blockbook API
    pub fn get_utxos(&self, address: &str) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
        let mut last_err = String::from("No explorers configured");
        for explorer in self.explorers {
            let url = format!("{}/api/v2/utxo/{}?confirmed=true", explorer, address);
            match ureq::get(&url).timeout(REQUEST_TIMEOUT).call() {
                Ok(resp) => {
                    let body = resp.into_string()?;
                    let utxos: Vec<serde_json::Value> = serde_json::from_str(&body)?;
                    return Ok(utxos);
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err.into())
    }

    /// Is `txid` known to the network (confirmed or in the mempool)?
    ///
    /// Asks Blockbook explorers and PIVX Core RPC nodes alternately and
    /// stops as soon as the answer is settled (see [`decide_tx_exists`]):
    /// one "found", or two independent "not found"s.
    pub fn tx_exists(&self, txid: &str) -> Result<bool, Box<dyn Error>> {
        let mut answers = Vec::new();
        let n = self.explorers.len().max(self.rpc_nodes.len());
        for i in 0..n {
            if let Some(explorer) = self.explorers.get(i) {
                let url = format!("{}/api/v2/tx/{}", explorer, txid);
                answers.push(lookup(&url, |code, body| classify_explorer_tx(code, body, txid)));
            }
            if let Ok(found) = decide_tx_exists(&answers) {
                return Ok(found);
            }
            if let Some(node) = self.rpc_nodes.get(i) {
                let url = format!("{}/getrawtransaction?params={}", node, txid);
                answers.push(lookup(&url, |code, body| classify_rpc_tx(code, body, txid)));
            }
            if let Ok(found) = decide_tx_exists(&answers) {
                return Ok(found);
            }
        }
        decide_tx_exists(&answers).map_err(|e| e.into())
    }

    /// Broadcast a raw transaction hex
    pub fn send_transaction(&self, tx_hex: &str) -> Result<String, Box<dyn Error>> {
        let mut last_err = String::from("Failed to broadcast");

        // Try explorers first (POST)
        for explorer in self.explorers {
            let url = format!("{}/api/v2/sendtx/", explorer);
            match ureq::post(&url).timeout(REQUEST_TIMEOUT).send_string(tx_hex) {
                Ok(resp) => {
                    let body_str = resp.into_string()?;
                    let body: serde_json::Value = serde_json::from_str(&body_str)?;
                    if let Some(err) = body.get("error").and_then(|e| e.as_str()) {
                        last_err = err.to_string();
                        continue;
                    }
                    if let Some(result) =
                        body.get("result").and_then(|r: &serde_json::Value| r.as_str())
                    {
                        return Ok(result.to_string());
                    }
                    // HTTP 200 but unexpected body — tx likely accepted, return raw
                    return Ok(body_str);
                }
                Err(ureq::Error::Status(_, resp)) => {
                    // Non-2xx — read the error body
                    last_err = resp.into_string().unwrap_or_else(|_| "Unknown error".into());
                }
                Err(e) => last_err = e.to_string(),
            }
        }

        // Fallback to RPC (POST for large payloads)
        let mut rpc_err = String::new();
        for node in self.rpc_nodes {
            let url = format!("{}/sendrawtransaction", node);
            let body = serde_json::json!({ "params": [tx_hex] }).to_string();
            match ureq::post(&url)
                .timeout(REQUEST_TIMEOUT)
                .set("Content-Type", "application/json")
                .send_string(&body)
            {
                Ok(resp) => {
                    let body_str = resp.into_string()?;
                    let parsed: serde_json::Value = serde_json::from_str(&body_str)?;
                    if let Some(result) = parsed.get("result").and_then(|r| r.as_str()) {
                        return Ok(result.to_string());
                    }
                    return Ok(body_str);
                }
                Err(e) => rpc_err = e.to_string(),
            }
        }
        Err(format!("Broadcast failed — explorers: {}, RPC: {}", last_err, rpc_err).into())
    }
}

/// Outcome of one source's answer to "do you know this txid?".
#[derive(Debug, PartialEq)]
pub enum TxLookup {
    Found,
    NotFound,
    /// Anything else: transport error, 5xx, unexpected body.
    Unknown(String),
}

/// Upper bound on how long a send can run between writing its ledger
/// intent and recording the outcome: every broadcast endpoint timing
/// out, then every lookup endpoint timing out. The ledger's in-flight
/// window must exceed this.
pub const WORST_CASE_SEND_SECS: u64 =
    ((EXPLORERS.len() + RPC_NODES.len()) * 2) as u64 * REQUEST_TIMEOUT.as_secs();

/// Classify Blockbook `GET /api/v2/tx/<txid>`. Found only if the body
/// is that transaction — a proxy or captive portal answering 200 must
/// not count. Unknown txids get 400 `{"error":"Transaction '…' not found"}`.
pub fn classify_explorer_tx(status: u16, body: &str, txid: &str) -> TxLookup {
    match status {
        200 => {
            let reported = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("txid").and_then(|t| t.as_str()).map(|t| t.to_ascii_lowercase()));
            if reported.as_deref() == Some(txid.to_ascii_lowercase().as_str()) {
                TxLookup::Found
            } else {
                TxLookup::Unknown(format!("explorer answered 200 without transaction {}", txid))
            }
        }
        400 if body.contains("not found") => TxLookup::NotFound,
        _ => TxLookup::Unknown(format!("HTTP {}: {}", status, snippet(body))),
    }
}

/// Classify PIVX Core `getrawtransaction <txid>` via the RPC proxy.
/// Found only if the returned hex hashes to `txid`. Unknown txids get
/// an error body "No such mempool or blockchain transaction".
pub fn classify_rpc_tx(status: u16, body: &str, txid: &str) -> TxLookup {
    if body.contains("No such mempool or blockchain transaction") {
        return TxLookup::NotFound;
    }
    if status == 200 {
        let hashes_to_txid = serde_json::from_str::<String>(body)
            .ok()
            .and_then(|hex| crate::spend::compute_txid(&hex).ok())
            .is_some_and(|t| t.eq_ignore_ascii_case(txid));
        if hashes_to_txid {
            return TxLookup::Found;
        }
        return TxLookup::Unknown(format!("RPC answered 200 without transaction {}", txid));
    }
    TxLookup::Unknown(format!("HTTP {}: {}", status, snippet(body)))
}

fn snippet(body: &str) -> String {
    body.chars().take(200).collect()
}

/// Combine answers: any `Found` → true. `false` needs at least two
/// definite "not found" answers — callers use it to decide it's safe to
/// pay again, so one lagging source must not decide alone. Otherwise
/// the outcome is unknown.
pub fn decide_tx_exists(answers: &[TxLookup]) -> Result<bool, String> {
    if answers.contains(&TxLookup::Found) {
        return Ok(true);
    }
    let not_found = answers.iter().filter(|a| **a == TxLookup::NotFound).count();
    if not_found >= 2 {
        return Ok(false);
    }
    let errors: Vec<&str> = answers
        .iter()
        .filter_map(|a| match a {
            TxLookup::Unknown(e) => Some(e.as_str()),
            _ => None,
        })
        .collect();
    Err(format!(
        "can't confirm whether the transaction exists ({} of 2 needed sources said not found; errors: {})",
        not_found,
        if errors.is_empty() { "none".to_string() } else { errors.join("; ") }
    ))
}

/// GET `url` and classify the response with `classify(status, body)`.
fn lookup(url: &str, classify: impl Fn(u16, &str) -> TxLookup) -> TxLookup {
    match ureq::get(url).timeout(REQUEST_TIMEOUT).call() {
        Ok(resp) => {
            let code = resp.status();
            classify(code, &resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) => classify(code, &resp.into_string().unwrap_or_default()),
        Err(e) => TxLookup::Unknown(e.to_string()),
    }
}

impl crate::payment::Network for PivxNetwork {
    fn broadcast(&self, txhex: &str) -> Result<String, Box<dyn Error>> {
        self.send_transaction(txhex)
    }
}

impl crate::ledger::ChainLookup for PivxNetwork {
    fn tx_exists(&self, txid: &str) -> Result<bool, Box<dyn Error>> {
        PivxNetwork::tx_exists(self, txid)
    }
}

/// Download sapling parameters from CDN, returning (output_bytes, spend_bytes)
pub fn download_sapling_params(
    on_progress: impl Fn(f32),
) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    for host in SAPLING_PARAM_HOSTS {
        match try_download_params(host, &on_progress) {
            Ok(params) => return Ok(params),
            Err(_) => continue,
        }
    }
    Err("Failed to download sapling parameters from all sources".into())
}

fn try_download_params(
    host: &str,
    on_progress: &impl Fn(f32),
) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let output_url = format!("{}/sapling-output.params", host);
    let output_bytes = download_with_progress(&output_url, 0.0, 0.1, on_progress)?;

    let spend_url = format!("{}/sapling-spend.params", host);
    let spend_bytes = download_with_progress(&spend_url, 0.1, 1.0, on_progress)?;

    Ok((output_bytes, spend_bytes))
}

fn download_with_progress(
    url: &str,
    start_pct: f32,
    end_pct: f32,
    on_progress: &impl Fn(f32),
) -> Result<Vec<u8>, Box<dyn Error>> {
    // No timeout for large downloads — they can take minutes
    let resp = ureq::get(url).call()?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut bytes = Vec::with_capacity(total);
    let mut buf = [0u8; 65536];
    let mut downloaded = 0usize;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        downloaded += n;
        if total > 0 {
            let ratio = downloaded as f32 / total as f32;
            on_progress(start_pct + ratio * (end_pct - start_pct));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: &str = "0ef9a5928d3d15fbf1a39d0b4339cd70180194db78f90cbe6bc3e63268d578c2";

    fn fixture_hex() -> String {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/mainnet_tx_v1_transparent.json")).unwrap();
        v["hex"].as_str().unwrap().to_string()
    }

    // -------- explorer --------------------------------------------

    #[test]
    fn explorer_found_requires_matching_txid_in_body() {
        let body = format!("{{\"txid\":\"{}\",\"version\":1}}", TXID);
        assert_eq!(classify_explorer_tx(200, &body, TXID), TxLookup::Found);
        let other = format!("{{\"txid\":\"{}\"}}", "ab".repeat(32));
        assert!(matches!(classify_explorer_tx(200, &other, TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_explorer_tx(200, "<html>captive portal</html>", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_explorer_tx(200, "", TXID), TxLookup::Unknown(_)));
    }

    #[test]
    fn explorer_not_found_on_blockbook_400() {
        let body = format!("{{\"error\":\"Transaction '{}' not found\"}}", TXID);
        assert_eq!(classify_explorer_tx(400, &body, TXID), TxLookup::NotFound);
    }

    #[test]
    fn explorer_other_errors_are_unknown() {
        assert!(matches!(classify_explorer_tx(403, "Just a moment...", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_explorer_tx(500, "not found", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_explorer_tx(429, "rate limited", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_explorer_tx(400, "{\"error\":\"bad request\"}", TXID), TxLookup::Unknown(_)));
    }

    // -------- rpc -------------------------------------------------

    #[test]
    fn rpc_found_requires_hex_that_hashes_to_txid() {
        let body = format!("\"{}\"", fixture_hex());
        assert_eq!(classify_rpc_tx(200, &body, TXID), TxLookup::Found);
        assert!(matches!(classify_rpc_tx(200, &body, &"ab".repeat(32)), TxLookup::Unknown(_)));
        assert!(matches!(classify_rpc_tx(200, "\"zz\"", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_rpc_tx(200, "null", TXID), TxLookup::Unknown(_)));
    }

    #[test]
    fn rpc_not_found_message() {
        let body = "No such mempool or blockchain transaction. Use gettransaction for wallet transactions.";
        assert_eq!(classify_rpc_tx(502, body, TXID), TxLookup::NotFound);
        assert_eq!(classify_rpc_tx(500, body, TXID), TxLookup::NotFound);
    }

    #[test]
    fn rpc_other_errors_are_unknown() {
        assert!(matches!(classify_rpc_tx(403, "Invalid RPC", TXID), TxLookup::Unknown(_)));
        assert!(matches!(classify_rpc_tx(502, "Bad Gateway", TXID), TxLookup::Unknown(_)));
    }

    // -------- combining -------------------------------------------

    #[test]
    fn any_found_wins() {
        use TxLookup::*;
        assert_eq!(decide_tx_exists(&[NotFound, NotFound, Found]), Ok(true));
        assert_eq!(decide_tx_exists(&[Unknown("x".into()), Found]), Ok(true));
    }

    #[test]
    fn not_found_needs_two_sources() {
        use TxLookup::*;
        assert_eq!(decide_tx_exists(&[NotFound, NotFound]), Ok(false));
        assert_eq!(decide_tx_exists(&[NotFound, Unknown("down".into()), NotFound]), Ok(false));
        assert!(decide_tx_exists(&[NotFound]).is_err());
        assert!(decide_tx_exists(&[NotFound, Unknown("down".into())]).is_err());
        assert!(decide_tx_exists(&[]).is_err());
    }

    #[test]
    fn worst_case_send_counts_every_endpoint_twice() {
        assert_eq!(WORST_CASE_SEND_SECS, 6 * 2 * 30);
    }
}
