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
