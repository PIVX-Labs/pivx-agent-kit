//! Append-only spend ledger (`ledger.jsonl` in the data directory).
//!
//! Every outgoing payment writes a `send_intent` line *before* the
//! transaction is broadcast (with the locally computed txid), then a
//! `send_broadcast` or `send_failed` line afterwards. A crash between
//! the two leaves an intent with a known txid that can be settled
//! against the chain later.
//!
//! The ledger powers `history` and the double-pay guard: a retry of the
//! same logical payment (same non-`Send` [`Origin`]) finds the earlier
//! attempt and reuses its txid instead of paying again.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::spend::{Origin, Pool};

pub const LEDGER_FILE: &str = "ledger.jsonl";
const FORMAT_VERSION: u32 = 1;

/// An intent younger than this is assumed to still be broadcasting in
/// another process; guards refuse rather than risk a second payment.
/// Must exceed the longest a send can take between writing its intent
/// and recording the outcome (pinned against the network timeouts by a
/// test). A failed record this young is treated the same way: the tx
/// may still be propagating.
pub const IN_FLIGHT_SECS: u64 = 600;
const _: () = assert!(IN_FLIGHT_SECS > crate::network::WORST_CASE_SEND_SECS);

/// Seconds in the rolling "today" window used for totals.
pub const DAY_SECS: u64 = 86_400;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendIntent {
    pub id: String,
    pub ts: u64,
    /// Transparent address of the wallet that paid; scopes reads so a
    /// seed swap doesn't mix two wallets' histories.
    pub wallet: String,
    pub to: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub from: Pool,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub origin: Origin,
    pub txid: String,
    #[serde(default)]
    pub usd_price_e8: Option<u64>,
}

// Variant names mirror the on-disk `event` tags (`send_intent`, …);
// PR 3 adds non-send events alongside them.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    SendIntent(SendIntent),
    SendBroadcast { id: String, ts: u64, txid: String },
    SendFailed { id: String, ts: u64, error: String },
}

/// On-disk line: the event plus a format version.
#[derive(Serialize, Deserialize)]
struct Line {
    v: u32,
    #[serde(flatten)]
    event: Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Broadcast,
    Failed,
    /// Intent written, outcome unknown (crash or still in flight).
    Unresolved,
}

/// One payment, folded from its events.
#[derive(Clone, Debug, PartialEq)]
pub struct SendRecord {
    pub intent: SendIntent,
    pub status: Status,
    pub error: Option<String>,
}

/// What a retry-sensitive caller should do before paying for `origin`.
#[derive(Clone, Debug, PartialEq)]
pub enum Guard {
    /// No earlier attempt that could have landed — pay.
    Proceed,
    /// An earlier attempt is known to be on chain — reuse its txid.
    Reuse(String),
    /// An attempt started (or failed) moments ago; its outcome may not
    /// be visible yet. Retry after `retry_after` (unix seconds).
    InFlight { txid: String, retry_after: u64 },
    /// Earlier attempts with unknown or failed outcome: check these
    /// `(record id, txid)` pairs against the chain first.
    Verify(Vec<(String, String)>),
}

/// Lets the guard ask "is this txid known to the network (chain or
/// mempool)?". `Ok(false)` must mean a definite "not found";
/// unreachable explorers are an `Err`.
pub trait ChainLookup {
    fn tx_exists(&self, txid: &str) -> Result<bool, Box<dyn Error>>;
}

pub struct Ledger {
    path: PathBuf,
}

/// Held while a payment is being made; see [`Ledger::lock_spending`].
#[derive(Debug)]
pub struct SpendLock {
    _file: fs::File,
}

impl Ledger {
    /// The ledger in the kit's data directory.
    pub fn open_default() -> Self {
        Self::at(crate::wallet::get_data_dir().join(LEDGER_FILE))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one event as a single line. Creates the file (mode 600 on
    /// Unix) if needed, and starts a fresh line if a previous write was
    /// torn mid-line.
    pub fn append(&self, event: &Event) -> Result<(), Box<dyn Error>> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_string(&Line { v: FORMAT_VERSION, event: event.clone() })?;
        line.push('\n');

        let mut opts = OpenOptions::new();
        opts.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&self.path)?;
        // `mode` only applies on creation; tighten a pre-existing file too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }

        // If the previous write was torn mid-line, terminate it so this
        // event lands on its own line instead of being glued to garbage.
        let len = file.metadata()?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, '\n');
            }
        }
        // One write call per event keeps concurrent O_APPEND writers
        // from interleaving within a line.
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// All events in file order. A missing file is empty. Lines that
    /// aren't valid UTF-8 / JSON (torn writes) and unknown event types
    /// are skipped with a warning. Fails — so callers refuse to pay —
    /// on any other read error, a line from a newer format version, or
    /// a `send_intent` this version can't understand: skipping those
    /// would hide a payment from the double-pay guard.
    pub fn read_events(&self) -> Result<Vec<Event>, Box<dyn Error>> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read spend ledger {}: {}", self.path.display(), e).into()),
        };
        let mut events = Vec::new();
        for (n, raw) in bytes.split(|b| *b == b'\n').enumerate() {
            let line_no = n + 1;
            let Ok(text) = std::str::from_utf8(raw) else {
                eprintln!("ledger: skipping line {} (not UTF-8; torn write?)", line_no);
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                eprintln!("ledger: skipping line {} (not JSON; torn write?)", line_no);
                continue;
            };
            let version = value.get("v").and_then(|v| v.as_u64()).unwrap_or(0);
            if version > FORMAT_VERSION as u64 {
                return Err(format!(
                    "spend ledger line {} was written by a newer pivx-agent-kit (format v{}); update the kit before spending",
                    line_no, version
                )
                .into());
            }
            match serde_json::from_value::<Line>(value.clone()) {
                Ok(line) => events.push(line.event),
                Err(e) if value.get("event").and_then(|v| v.as_str()) == Some("send_intent") => {
                    return Err(format!(
                        "spend ledger line {} is a send_intent this version can't read ({}); refusing to spend until it's fixed",
                        line_no, e
                    )
                    .into());
                }
                Err(e) => eprintln!("ledger: skipping line {} ({})", line_no, e),
            }
        }
        Ok(events)
    }

    /// Payments made by `wallet`, oldest first.
    pub fn records(&self, wallet: &str) -> Result<Vec<SendRecord>, Box<dyn Error>> {
        Ok(reduce(&self.read_events()?, wallet))
    }

    /// Exclusive lock serializing payments (and wallet writes) from
    /// this data directory across processes. Waits up to `wait` while
    /// another payment is in progress, then gives up rather than hang
    /// behind a stuck process. Released on drop.
    pub fn lock_spending(&self, wait: std::time::Duration) -> Result<SpendLock, Box<dyn Error>> {
        let file = self.open_lock_file()?;
        let deadline = std::time::Instant::now() + wait;
        let mut announced = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(SpendLock { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "another payment from this wallet is still in progress after {}s; nothing was sent — retry later",
                    wait.as_secs()
                )
                .into());
            }
            if !announced {
                eprintln!("Waiting for another payment from this wallet to finish...");
                announced = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Like [`lock_spending`] but returns `None` instead of waiting.
    pub fn try_lock_spending(&self) -> Result<Option<SpendLock>, Box<dyn Error>> {
        let file = self.open_lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(SpendLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    fn open_lock_file(&self) -> Result<fs::File, Box<dyn Error>> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))?)
    }
}

/// Fold events into one record per intent belonging to `wallet`,
/// oldest first. The last settling event for an id wins, so a payment
/// first marked failed and later found on chain reads as broadcast.
pub fn reduce(events: &[Event], wallet: &str) -> Vec<SendRecord> {
    let mut records: Vec<SendRecord> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for event in events {
        match event {
            Event::SendIntent(intent) if intent.wallet == wallet => {
                index.insert(intent.id.clone(), records.len());
                records.push(SendRecord { intent: intent.clone(), status: Status::Unresolved, error: None });
            }
            Event::SendIntent(_) => {}
            Event::SendBroadcast { id, txid, .. } => {
                if let Some(&i) = index.get(id) {
                    records[i].status = Status::Broadcast;
                    records[i].error = None;
                    records[i].intent.txid = txid.clone();
                }
            }
            Event::SendFailed { id, error, .. } => {
                if let Some(&i) = index.get(id) {
                    records[i].status = Status::Failed;
                    records[i].error = Some(error.clone());
                }
            }
        }
    }
    records
}

/// Decide what a caller paying for `origin` should do, given what the
/// ledger already holds.
pub fn guard_decision(records: &[SendRecord], origin: &Origin, now: u64) -> Guard {
    if *origin == Origin::Send {
        return Guard::Proceed;
    }
    let same: Vec<&SendRecord> = records.iter().filter(|r| &r.intent.origin == origin).collect();

    if let Some(paid) = same.iter().rev().find(|r| r.status == Status::Broadcast) {
        return Guard::Reuse(paid.intent.txid.clone());
    }
    if let Some(fresh) = same
        .iter()
        .rev()
        .find(|r| r.status != Status::Broadcast && now.saturating_sub(r.intent.ts) < IN_FLIGHT_SECS)
    {
        return Guard::InFlight {
            txid: fresh.intent.txid.clone(),
            retry_after: fresh.intent.ts + IN_FLIGHT_SECS,
        };
    }
    let to_check: Vec<(String, String)> = same
        .iter()
        .map(|r| (r.intent.id.clone(), r.intent.txid.clone()))
        .collect();
    if to_check.is_empty() {
        Guard::Proceed
    } else {
        Guard::Verify(to_check)
    }
}

/// Run the guard and settle anything it can against the chain. Returns
/// `Some(txid)` if an earlier payment for `origin` went through (do not
/// pay again), `None` if it is safe to pay. Errors when the outcome of
/// an earlier attempt can't be established.
pub fn prior_payment(
    ledger: &Ledger,
    wallet: &str,
    origin: &Origin,
    chain: &dyn ChainLookup,
    now: u64,
) -> Result<Option<String>, Box<dyn Error>> {
    let records = ledger.records(wallet)?;
    match guard_decision(&records, origin, now) {
        Guard::Proceed => Ok(None),
        Guard::Reuse(txid) => Ok(Some(txid)),
        Guard::InFlight { txid, retry_after } => {
            let wait = retry_after.saturating_sub(now);
            let record = records.iter().rev().find(|r| r.intent.txid == txid);
            Err(match record {
                Some(SendRecord { status: Status::Failed, error, .. }) => format!(
                    "an earlier attempt to pay this failed moments ago ({}; txid {}). Its transaction may still \
                     surface, so the kit won't pay again for another {} seconds. Check `history` then retry.",
                    error.as_deref().unwrap_or("unknown error"),
                    txid,
                    wait
                ),
                _ => format!(
                    "a payment for this is already in progress or its outcome isn't settled yet (txid {}). \
                     Retry in {} seconds, or check `history`.",
                    txid, wait
                ),
            }
            .into())
        }
        Guard::Verify(pairs) => {
            for (id, txid) in pairs {
                let status = records.iter().find(|r| r.intent.id == id).map(|r| r.status);
                match chain.tx_exists(&txid) {
                    Ok(true) => {
                        ledger.append(&Event::SendBroadcast { id, ts: now, txid: txid.clone() })?;
                        return Ok(Some(txid));
                    }
                    Ok(false) => {
                        if status == Some(Status::Unresolved) {
                            ledger.append(&Event::SendFailed {
                                id,
                                ts: now,
                                error: "not found on chain".into(),
                            })?;
                        }
                    }
                    Err(e) => {
                        return Err(format!(
                            "could not check whether an earlier payment (txid {}) went through: {}; retry shortly",
                            txid, e
                        )
                        .into())
                    }
                }
            }
            Ok(None)
        }
    }
}

/// Returns whether anything was recorded.
///
/// Best-effort: settle payments whose outcome isn't final by looking
/// their txids up — unresolved intents and failed ones from the last
/// day (a broadcast that errored may still have landed), once they're
/// older than [`IN_FLIGHT_SECS`]. Stops at the first lookup that can't
/// get an answer so `history` doesn't stall when explorers are down.
pub fn resolve_unresolved(ledger: &Ledger, records: &[SendRecord], chain: &dyn ChainLookup, now: u64) -> bool {
    let mut changed = false;
    for r in records.iter().cloned() {
        let age = now.saturating_sub(r.intent.ts);
        let recheck = match r.status {
            Status::Unresolved => true,
            Status::Failed => age <= DAY_SECS,
            Status::Broadcast => false,
        };
        if !recheck || age < IN_FLIGHT_SECS {
            continue;
        }
        let event = match chain.tx_exists(&r.intent.txid) {
            Ok(true) => Event::SendBroadcast { id: r.intent.id, ts: now, txid: r.intent.txid },
            Ok(false) if r.status == Status::Unresolved => {
                Event::SendFailed { id: r.intent.id, ts: now, error: "not found on chain".into() }
            }
            Ok(false) => continue,
            Err(e) => {
                eprintln!("ledger: can't settle earlier payments right now: {}", e);
                return changed;
            }
        };
        match ledger.append(&event) {
            Ok(()) => changed = true,
            Err(e) => eprintln!("ledger: could not record resolution: {}", e),
        }
    }
    changed
}

/// Satoshis (amount + fee) of payments at or after `since` that went
/// out or might have. Failed payments don't count.
pub fn spent_since(records: &[SendRecord], since: u64) -> u64 {
    records
        .iter()
        .filter(|r| r.status != Status::Failed && r.intent.ts >= since)
        .map(|r| r.intent.amount_sat.saturating_add(r.intent.fee_sat))
        .fold(0u64, |acc, x| acc.saturating_add(x))
}

/// Records at or after `since`, newest first, at most `limit`.
pub fn select(records: &[SendRecord], limit: Option<usize>, since: Option<u64>) -> Vec<SendRecord> {
    records
        .iter()
        .rev()
        .filter(|r| since.is_none_or(|s| r.intent.ts >= s))
        .take(limit.unwrap_or(usize::MAX))
        .cloned()
        .collect()
}

/// JSON for `history`: newest first, filtered, with 24h totals.
pub fn history_value(records: &[SendRecord], limit: Option<usize>, since: Option<u64>, now: u64) -> Value {
    let sends: Vec<Value> = select(records, limit, since).iter().map(record_value).collect();
    let since_24h = now.saturating_sub(DAY_SECS);
    let last_24h = spent_since(records, since_24h);
    let mut totals = json!({
        "last_24h_sat": last_24h,
        "last_24h": last_24h as f64 / 1e8,
    });
    // A dollar total only when every counted payment recorded its price.
    let counted: Vec<&SendRecord> = records
        .iter()
        .filter(|r| r.status != Status::Failed && r.intent.ts >= since_24h)
        .collect();
    if counted.iter().all(|r| r.intent.usd_price_e8.is_some()) {
        let usd: u64 = counted
            .iter()
            .map(|r| usd_value(r.intent.amount_sat.saturating_add(r.intent.fee_sat), r.intent.usd_price_e8))
            .map(|v| v.unwrap_or(0))
            .fold(0u64, |a, b| a.saturating_add(b));
        totals["last_24h_usd"] = json!(crate::price::format_usd(usd));
    }
    json!({
        "count": sends.len(),
        "sends": sends,
        "totals": totals,
    })
}

fn usd_value(sat: u64, price_e8: Option<u64>) -> Option<u64> {
    price_e8.map(|p| crate::price::sat_to_usd_e8(sat, p))
}

fn record_value(r: &SendRecord) -> Value {
    let i = &r.intent;
    let mut v = json!({
        "id": i.id,
        "ts": i.ts,
        "time": crate::timefmt::format_rfc3339(i.ts),
        "status": r.status,
        "txid": i.txid,
        "to": i.to,
        "amount": i.amount_sat as f64 / 1e8,
        "amount_sat": i.amount_sat,
        "fee": i.fee_sat as f64 / 1e8,
        "fee_sat": i.fee_sat,
        "from": i.from,
        "reason": i.reason,
        "origin": i.origin,
    });
    if let Some(usd) = usd_value(i.amount_sat, i.usd_price_e8) {
        v["amount_usd"] = json!(crate::price::format_usd(usd));
    }
    if !i.memo.is_empty() {
        v["memo"] = json!(i.memo);
    }
    if let Some(e) = &r.error {
        v["error"] = json!(e);
    }
    v
}

/// CSV for `history --csv`, newest first. Text cells that a spreadsheet
/// would evaluate as a formula are prefixed with `'`.
pub fn to_csv(records: &[SendRecord]) -> String {
    use pivx_wallet_kit::amount::format_sat_to_piv;
    let mut out = String::from("time,status,amount_piv,amount_usd,fee_piv,from,to,reason,origin,txid,id\n");
    for r in records.iter().rev() {
        let i = &r.intent;
        let status = match r.status {
            Status::Broadcast => "broadcast",
            Status::Failed => "failed",
            Status::Unresolved => "unresolved",
        };
        let cells = [
            crate::timefmt::format_rfc3339(i.ts),
            status.to_string(),
            format_sat_to_piv(i.amount_sat),
            usd_value(i.amount_sat, i.usd_price_e8).map(crate::price::format_usd).unwrap_or_default(),
            format_sat_to_piv(i.fee_sat),
            i.from.to_string(),
            csv_text(&i.to),
            csv_text(i.reason.as_deref().unwrap_or("")),
            csv_text(&origin_label(&i.origin)),
            csv_text(&i.txid),
            csv_text(&i.id),
        ];
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

fn origin_label(o: &Origin) -> String {
    match o {
        Origin::Send => "send".into(),
        Origin::TaskApprove { task_id, worker } => format!("task_approve:{}:{}", task_id, worker),
        Origin::CardsPay { order_id } => format!("cards_pay:{}", order_id),
    }
}

/// Quote a free-text CSV cell when needed, and defuse cells a
/// spreadsheet would run as a formula (reasons are agent-written).
fn csv_text(s: &str) -> String {
    let mut cell = s.to_string();
    if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        cell.insert(0, '\'');
    }
    if cell.contains([',', '"', '\n', '\r']) {
        cell = format!("\"{}\"", cell.replace('"', "\"\""));
    }
    cell
}

/// Random 16-hex-char id for a new intent.
pub fn new_id() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    const W: &str = "DWalletAddr";
    const T0: u64 = 1_790_000_000;

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pak-test-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn ledger_path(l: &Ledger) -> PathBuf {
        l.path.clone()
    }

    fn temp_ledger(tag: &str) -> Ledger {
        Ledger::at(temp_dir(tag).join(LEDGER_FILE))
    }

    fn txid(n: u8) -> String {
        format!("{:02x}", n).repeat(32)
    }

    fn intent(id: &str, ts: u64, origin: Origin, amount_sat: u64) -> SendIntent {
        SendIntent {
            id: id.into(),
            ts,
            wallet: W.into(),
            to: "DDest".into(),
            amount_sat,
            fee_sat: 1_000,
            from: Pool::Public,
            memo: String::new(),
            reason: Some(format!("reason {id}")),
            origin,
            txid: txid(id.as_bytes()[0]),
            usd_price_e8: None,
        }
    }

    fn task(n: i64) -> Origin {
        Origin::TaskApprove { task_id: n, worker: "otter".into() }
    }

    struct MockChain {
        answers: HashMap<String, Result<bool, String>>,
        calls: RefCell<Vec<String>>,
    }

    impl MockChain {
        fn new(answers: &[(&str, Result<bool, &str>)]) -> Self {
            Self {
                answers: answers
                    .iter()
                    .map(|(t, r)| (t.to_string(), r.map_err(|e| e.to_string())))
                    .collect(),
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl ChainLookup for MockChain {
        fn tx_exists(&self, txid: &str) -> Result<bool, Box<dyn Error>> {
            self.calls.borrow_mut().push(txid.to_string());
            match self.answers.get(txid) {
                Some(Ok(b)) => Ok(*b),
                Some(Err(e)) => Err(e.clone().into()),
                None => panic!("unexpected lookup of {txid}"),
            }
        }
    }

    // -------- serialization ---------------------------------------

    #[test]
    fn events_serialize_as_tagged_versioned_lines() {
        let l = temp_ledger("ser");
        l.append(&Event::SendIntent(intent("a", T0, task(12), 5))).unwrap();
        l.append(&Event::SendBroadcast { id: "a".into(), ts: T0 + 1, txid: txid(b'a') }).unwrap();
        let text = fs::read_to_string(&l.path).unwrap();
        let lines: Vec<Value> = text.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["v"], json!(1));
        assert_eq!(lines[0]["event"], json!("send_intent"));
        assert_eq!(lines[0]["origin"], json!({"type": "task_approve", "task_id": 12, "worker": "otter"}));
        assert_eq!(lines[0]["from"], json!("public"));
        assert_eq!(lines[1]["event"], json!("send_broadcast"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn append_then_read_round_trips() {
        let l = temp_ledger("rt");
        let events = vec![
            Event::SendIntent(intent("a", T0, Origin::Send, 5)),
            Event::SendFailed { id: "a".into(), ts: T0 + 1, error: "boom".into() },
        ];
        for e in &events {
            l.append(e).unwrap();
        }
        assert_eq!(l.read_events().unwrap(), events);
    }

    #[test]
    fn missing_file_reads_empty() {
        let l = temp_ledger("missing");
        assert!(l.read_events().unwrap().is_empty());
        assert!(l.records(W).unwrap().is_empty());
    }

    #[test]
    fn malformed_and_unknown_lines_are_skipped() {
        let l = temp_ledger("malformed");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 5))).unwrap();
        let mut text = fs::read_to_string(&l.path).unwrap();
        text.push_str("not json\n");
        text.push_str("{\"v\":1,\"event\":\"from_the_future\",\"x\":1}\n");
        text.push('\n');
        fs::write(&l.path, text).unwrap();
        l.append(&Event::SendBroadcast { id: "a".into(), ts: T0 + 1, txid: txid(b'a') }).unwrap();
        assert_eq!(l.read_events().unwrap().len(), 2);
        assert_eq!(l.records(W).unwrap()[0].status, Status::Broadcast);
    }

    #[test]
    fn append_after_torn_line_starts_a_new_line() {
        let l = temp_ledger("torn");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 5))).unwrap();
        // Simulate a crash mid-write: partial line, no trailing newline.
        let mut text = fs::read_to_string(&l.path).unwrap();
        text.push_str("{\"v\":1,\"event\":\"send_bro");
        fs::write(&l.path, text).unwrap();
        l.append(&Event::SendBroadcast { id: "a".into(), ts: T0 + 1, txid: txid(b'a') }).unwrap();
        assert_eq!(l.read_events().unwrap().len(), 2, "the event after the torn line must survive");
    }

    #[cfg(unix)]
    #[test]
    fn ledger_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let l = temp_ledger("perm");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 5))).unwrap();
        let mode = fs::metadata(&l.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn append_tightens_permissions_of_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let l = temp_ledger("perm-existing");
        fs::write(&l.path, "").unwrap();
        fs::set_permissions(&l.path, fs::Permissions::from_mode(0o644)).unwrap();
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 5))).unwrap();
        let mode = fs::metadata(&l.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = temp_dir("parent").join("nested");
        let l = Ledger::at(dir.join(LEDGER_FILE));
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 5))).unwrap();
        assert_eq!(l.read_events().unwrap().len(), 1);
    }

    // -------- reduce ----------------------------------------------

    #[test]
    fn reduce_statuses() {
        let events = vec![
            Event::SendIntent(intent("a", T0, Origin::Send, 1)),
            Event::SendIntent(intent("b", T0 + 1, Origin::Send, 2)),
            Event::SendIntent(intent("c", T0 + 2, Origin::Send, 3)),
            Event::SendBroadcast { id: "a".into(), ts: T0 + 5, txid: txid(b'a') },
            Event::SendFailed { id: "b".into(), ts: T0 + 5, error: "rejected".into() },
        ];
        let r = reduce(&events, W);
        assert_eq!(r.iter().map(|x| x.intent.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(r[0].status, Status::Broadcast);
        assert_eq!(r[1].status, Status::Failed);
        assert_eq!(r[1].error.as_deref(), Some("rejected"));
        assert_eq!(r[2].status, Status::Unresolved);
    }

    #[test]
    fn reduce_last_settlement_wins() {
        let events = vec![
            Event::SendIntent(intent("a", T0, Origin::Send, 1)),
            Event::SendFailed { id: "a".into(), ts: T0 + 1, error: "timeout".into() },
            Event::SendBroadcast { id: "a".into(), ts: T0 + 200, txid: txid(b'a') },
        ];
        let r = reduce(&events, W);
        assert_eq!(r[0].status, Status::Broadcast);
        assert_eq!(r[0].error, None);
    }

    #[test]
    fn reduce_scopes_to_wallet_and_ignores_orphans() {
        let mut other = intent("x", T0, Origin::Send, 9);
        other.wallet = "DOtherWallet".into();
        let events = vec![
            Event::SendIntent(other),
            Event::SendIntent(intent("a", T0, Origin::Send, 1)),
            Event::SendBroadcast { id: "x".into(), ts: T0 + 1, txid: txid(b'x') },
            Event::SendBroadcast { id: "nope".into(), ts: T0 + 1, txid: txid(b'n') },
        ];
        let r = reduce(&events, W);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].intent.id, "a");
    }

    // -------- guard_decision --------------------------------------

    fn rec(id: &str, ts: u64, origin: Origin, status: Status) -> SendRecord {
        SendRecord { intent: intent(id, ts, origin, 10), status, error: None }
    }

    #[test]
    fn guard_never_dedupes_plain_sends() {
        let r = vec![rec("a", T0, Origin::Send, Status::Broadcast)];
        assert_eq!(guard_decision(&r, &Origin::Send, T0 + 9_999), Guard::Proceed);
    }

    #[test]
    fn guard_proceeds_with_no_history_for_origin() {
        let r = vec![rec("a", T0, task(1), Status::Broadcast)];
        assert_eq!(guard_decision(&r, &task(2), T0 + 9_999), Guard::Proceed);
        let other_worker = Origin::TaskApprove { task_id: 1, worker: "someone-else".into() };
        assert_eq!(guard_decision(&r, &other_worker, T0 + 9_999), Guard::Proceed);
    }

    #[test]
    fn guard_reuses_broadcast_payment() {
        let r = vec![
            rec("a", T0, task(1), Status::Failed),
            rec("b", T0 + 10, task(1), Status::Broadcast),
        ];
        assert_eq!(guard_decision(&r, &task(1), T0 + 9_999), Guard::Reuse(txid(b'b')));
    }

    #[test]
    fn guard_reports_in_flight_for_fresh_unresolved() {
        let r = vec![rec("a", T0, task(1), Status::Unresolved)];
        assert_eq!(
            guard_decision(&r, &task(1), T0 + IN_FLIGHT_SECS - 1),
            Guard::InFlight { txid: txid(b'a'), retry_after: T0 + IN_FLIGHT_SECS }
        );
    }

    #[test]
    fn guard_treats_young_failed_as_in_flight() {
        // The broadcast call errored seconds ago; the tx may still be propagating.
        let r = vec![rec("a", T0, task(1), Status::Failed)];
        assert_eq!(
            guard_decision(&r, &task(1), T0 + 30),
            Guard::InFlight { txid: txid(b'a'), retry_after: T0 + IN_FLIGHT_SECS }
        );
    }

    #[test]
    fn guard_verifies_old_unresolved_and_failed() {
        let r = vec![
            rec("a", T0, task(1), Status::Failed),
            rec("b", T0 + 1, task(1), Status::Unresolved),
        ];
        assert_eq!(
            guard_decision(&r, &task(1), T0 + IN_FLIGHT_SECS + 1),
            Guard::Verify(vec![("a".into(), txid(b'a')), ("b".into(), txid(b'b'))])
        );
    }

    #[test]
    fn guard_matches_cards_orders_by_id() {
        let o = Origin::CardsPay { order_id: "ord1".into() };
        let r = vec![rec("a", T0, o.clone(), Status::Broadcast)];
        assert_eq!(guard_decision(&r, &o, T0 + 9_999), Guard::Reuse(txid(b'a')));
        assert_eq!(
            guard_decision(&r, &Origin::CardsPay { order_id: "ord2".into() }, T0 + 9_999),
            Guard::Proceed
        );
    }

    // -------- prior_payment (guard + chain) -----------------------

    #[test]
    fn prior_payment_none_when_nothing_recorded() {
        let l = temp_ledger("pp-none");
        let chain = MockChain::new(&[]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0).unwrap(), None);
    }

    #[test]
    fn prior_payment_reuses_without_chain_lookup() {
        let l = temp_ledger("pp-reuse");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        l.append(&Event::SendBroadcast { id: "a".into(), ts: T0, txid: txid(b'a') }).unwrap();
        let chain = MockChain::new(&[]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap(), Some(txid(b'a')));
        assert!(chain.calls.borrow().is_empty());
    }

    #[test]
    fn prior_payment_found_on_chain_is_recorded_and_reused() {
        let l = temp_ledger("pp-found");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        let ta = txid(b'a');
        let chain = MockChain::new(&[(ta.as_str(), Ok(true))]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap(), Some(ta));
        assert_eq!(l.records(W).unwrap()[0].status, Status::Broadcast);
    }

    #[test]
    fn prior_payment_not_on_chain_marks_failed_and_proceeds() {
        let l = temp_ledger("pp-notfound");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        let ta = txid(b'a');
        let chain = MockChain::new(&[(ta.as_str(), Ok(false))]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap(), None);
        assert_eq!(l.records(W).unwrap()[0].status, Status::Failed);
    }

    #[test]
    fn prior_payment_failed_record_found_on_chain_is_reused() {
        // Broadcast call errored (e.g. timeout) but the tx propagated.
        let l = temp_ledger("pp-failed-found");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        l.append(&Event::SendFailed { id: "a".into(), ts: T0, error: "timeout".into() }).unwrap();
        let ta = txid(b'a');
        let chain = MockChain::new(&[(ta.as_str(), Ok(true))]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap(), Some(ta));
    }

    #[test]
    fn prior_payment_errors_when_chain_unreachable() {
        let l = temp_ledger("pp-unreachable");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        let ta = txid(b'a');
        let chain = MockChain::new(&[(ta.as_str(), Err("explorers down"))]);
        let err = prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap_err().to_string();
        assert!(err.contains(&ta), "error should name the txid: {err}");
        assert_eq!(l.records(W).unwrap()[0].status, Status::Unresolved, "must not settle on a lookup error");
    }

    #[test]
    fn prior_payment_errors_while_in_flight() {
        let l = temp_ledger("pp-inflight");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        let chain = MockChain::new(&[]);
        let err = prior_payment(&l, W, &task(1), &chain, T0 + 5).unwrap_err().to_string();
        assert!(err.contains("in progress") && err.contains(&txid(b'a')), "{err}");
        // States the real remaining wait, not "a couple of minutes".
        assert!(err.contains(&format!("{} seconds", IN_FLIGHT_SECS - 5)), "{err}");
    }

    #[test]
    fn prior_payment_young_failure_says_failed_not_in_flight() {
        let l = temp_ledger("pp-young-failed");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        l.append(&Event::SendFailed { id: "a".into(), ts: T0, error: "min relay fee not met".into() }).unwrap();
        let chain = MockChain::new(&[]);
        let err = prior_payment(&l, W, &task(1), &chain, T0 + 60).unwrap_err().to_string();
        assert!(err.contains("min relay fee not met"), "{err}");
        assert!(!err.contains("in progress"), "{err}");
        assert!(err.contains(&format!("{} seconds", IN_FLIGHT_SECS - 60)), "{err}");
    }

    // -------- resolve_unresolved ----------------------------------

    #[test]
    fn resolve_settles_only_old_unresolved() {
        let l = temp_ledger("resolve");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 1))).unwrap(); // old, on chain
        l.append(&Event::SendIntent(intent("b", T0, Origin::Send, 1))).unwrap(); // old, missing
        l.append(&Event::SendIntent(intent("c", T0, Origin::Send, 1))).unwrap(); // old, lookup error
        l.append(&Event::SendIntent(intent("d", T0 + 990, Origin::Send, 1))).unwrap(); // fresh at resolve time
        let (ta, tb, tc) = (txid(b'a'), txid(b'b'), txid(b'c'));
        let chain = MockChain::new(&[(ta.as_str(), Ok(true)), (tb.as_str(), Ok(false)), (tc.as_str(), Err("down"))]);
        resolve_unresolved(&l, &l.records(W).unwrap(), &chain, T0 + 990 + IN_FLIGHT_SECS - 1);
        let st: Vec<Status> = l.records(W).unwrap().iter().map(|r| r.status).collect();
        assert_eq!(st, [Status::Broadcast, Status::Failed, Status::Unresolved, Status::Unresolved]);
    }

    #[test]
    fn resolve_stops_after_first_lookup_error() {
        let l = temp_ledger("resolve-stop");
        for id in ["a", "b", "c"] {
            l.append(&Event::SendIntent(intent(id, T0, Origin::Send, 1))).unwrap();
        }
        let (ta, tb, tc) = (txid(b'a'), txid(b'b'), txid(b'c'));
        let chain = MockChain::new(&[(ta.as_str(), Err("down")), (tb.as_str(), Ok(true)), (tc.as_str(), Ok(true))]);
        resolve_unresolved(&l, &l.records(W).unwrap(), &chain, T0 + 9_999);
        assert_eq!(chain.calls.borrow().len(), 1, "should give up after the first unreachable lookup");
    }

    #[test]
    fn resolve_rechecks_recent_failed_payments() {
        let l = temp_ledger("resolve-failed");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 1))).unwrap();
        l.append(&Event::SendFailed { id: "a".into(), ts: T0, error: "timeout".into() }).unwrap();
        l.append(&Event::SendIntent(intent("b", T0, Origin::Send, 1))).unwrap();
        l.append(&Event::SendFailed { id: "b".into(), ts: T0, error: "rejected".into() }).unwrap();
        // An old failure (> 1 day) is not re-checked.
        l.append(&Event::SendIntent(intent("c", T0 - 2 * DAY_SECS, Origin::Send, 1))).unwrap();
        l.append(&Event::SendFailed { id: "c".into(), ts: T0 - 2 * DAY_SECS, error: "x".into() }).unwrap();
        let (ta, tb) = (txid(b'a'), txid(b'b'));
        let chain = MockChain::new(&[(ta.as_str(), Ok(true)), (tb.as_str(), Ok(false))]);
        resolve_unresolved(&l, &l.records(W).unwrap(), &chain, T0 + IN_FLIGHT_SECS + 1);
        let st: Vec<Status> = l.records(W).unwrap().iter().map(|r| r.status).collect();
        assert_eq!(st, [Status::Broadcast, Status::Failed, Status::Failed]);
    }

    #[test]
    fn resolve_leaves_young_failed_alone() {
        let l = temp_ledger("resolve-young-failed");
        l.append(&Event::SendIntent(intent("a", T0, Origin::Send, 1))).unwrap();
        l.append(&Event::SendFailed { id: "a".into(), ts: T0, error: "timeout".into() }).unwrap();
        let chain = MockChain::new(&[]);
        resolve_unresolved(&l, &l.records(W).unwrap(), &chain, T0 + 5);
        assert!(chain.calls.borrow().is_empty());
    }

    // -------- read failures fail closed ---------------------------

    #[test]
    fn invalid_utf8_line_is_skipped_not_whole_file() {
        let l = temp_ledger("utf8");
        l.append(&Event::SendIntent(intent("a", T0, task(1), 10))).unwrap();
        l.append(&Event::SendBroadcast { id: "a".into(), ts: T0, txid: txid(b'a') }).unwrap();
        // A torn write that cut a multi-byte character (em dash = e2 80 94).
        let mut bytes = fs::read(&l.path).unwrap();
        bytes.extend_from_slice(b"{\"v\":1,\"event\":\"send_failed\",\"error\":\"Broadcast failed \xe2\x80");
        fs::write(&l.path, bytes).unwrap();
        let r = l.records(W).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Broadcast);
        let chain = MockChain::new(&[]);
        assert_eq!(prior_payment(&l, W, &task(1), &chain, T0 + 9_999).unwrap(), Some(txid(b'a')));
    }

    #[test]
    fn unreadable_ledger_is_an_error_and_blocks_the_guard() {
        // A directory where the file should be: read fails with something other than NotFound.
        let dir = temp_dir("unreadable");
        fs::create_dir_all(dir.join(LEDGER_FILE)).unwrap();
        let l = Ledger::at(dir.join(LEDGER_FILE));
        assert!(l.read_events().is_err());
        let chain = MockChain::new(&[]);
        assert!(prior_payment(&l, W, &task(1), &chain, T0).is_err());
    }

    #[test]
    fn newer_format_version_blocks() {
        let l = temp_ledger("future-v");
        fs::write(&l.path, "{\"v\":2,\"event\":\"send_intent\",\"id\":\"z\"}\n").unwrap();
        let err = l.read_events().unwrap_err().to_string();
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn unparseable_send_intent_blocks() {
        let l = temp_ledger("bad-intent");
        let mut v = serde_json::to_value(Line { v: 1, event: Event::SendIntent(intent("a", T0, Origin::Send, 1)) }).unwrap();
        v["origin"] = json!({"type": "gift_link", "code": "x"});
        fs::write(&l.path, format!("{}\n", v)).unwrap();
        let err = l.read_events().unwrap_err().to_string();
        assert!(err.contains("send_intent"), "{err}");
    }

    // -------- spend lock ------------------------------------------

    #[test]
    fn spend_lock_wait_is_bounded() {
        let l = temp_ledger("lock-timeout");
        let _held = l.lock_spending(std::time::Duration::from_secs(1)).unwrap();
        let started = std::time::Instant::now();
        let err = l.lock_spending(std::time::Duration::from_millis(300)).unwrap_err().to_string();
        assert!(err.contains("another payment"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn spend_lock_is_exclusive_until_dropped() {
        let l = temp_ledger("lock");
        let first = l.lock_spending(std::time::Duration::from_secs(1)).unwrap();
        assert!(l.try_lock_spending().unwrap().is_none(), "second lock must not be granted");
        drop(first);
        // Release can lag by a few ms: a concurrent test that forks a
        // child (machine-uid runs `ioreg` on macOS) briefly shares the
        // open lock file until the child execs. Production waits loop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while l.try_lock_spending().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "lock not released after drop");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // -------- totals ----------------------------------------------

    #[test]
    fn spent_since_counts_broadcast_and_unresolved_with_fees() {
        let r = vec![
            rec("a", T0, Origin::Send, Status::Broadcast),
            rec("b", T0 + 10, Origin::Send, Status::Unresolved),
            rec("c", T0 + 10, Origin::Send, Status::Failed),
            rec("d", T0 - 1, Origin::Send, Status::Broadcast),
        ];
        // a + b, each 10 sat + 1000 sat fee. `since` is inclusive.
        assert_eq!(spent_since(&r, T0), 2 * (10 + 1_000));
    }

    // -------- history_value ---------------------------------------

    #[test]
    fn history_is_newest_first_with_totals() {
        let mut r = vec![
            rec("a", T0, Origin::Send, Status::Broadcast),
            rec("b", T0 + DAY_SECS, task(3), Status::Failed),
            rec("c", T0 + DAY_SECS + 5, Origin::Send, Status::Broadcast),
        ];
        r[1].error = Some("rejected".into());
        let v = history_value(&r, None, None, T0 + DAY_SECS + 10);
        assert_eq!(v["count"], json!(3));
        let ids: Vec<&str> = v["sends"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
        let c = &v["sends"][0];
        assert_eq!(c["status"], json!("broadcast"));
        assert_eq!(c["txid"], json!(txid(b'c')));
        assert_eq!(c["amount_sat"], json!(10));
        assert_eq!(c["fee_sat"], json!(1_000));
        assert_eq!(c["from"], json!("public"));
        assert_eq!(c["reason"], json!("reason c"));
        assert_eq!(c["origin"], json!({"type": "send"}));
        assert_eq!(c["time"], json!(crate::timefmt::format_rfc3339(T0 + DAY_SECS + 5)));
        assert_eq!(v["sends"][1]["error"], json!("rejected"));
        // Only "c" is inside the last 24h window and not failed.
        assert_eq!(v["totals"]["last_24h_sat"], json!(1_010));
    }

    #[test]
    fn history_24h_total_is_inclusive_at_the_boundary() {
        let r = vec![
            rec("edge", T0, Origin::Send, Status::Broadcast),
            rec("old", T0 - 1, Origin::Send, Status::Broadcast),
        ];
        let v = history_value(&r, None, None, T0 + DAY_SECS);
        assert_eq!(v["totals"]["last_24h_sat"], json!(1_010));
    }

    #[test]
    fn history_applies_since_then_limit() {
        let r: Vec<SendRecord> = (0..5)
            .map(|i| rec(&format!("{i}"), T0 + i as u64, Origin::Send, Status::Broadcast))
            .collect();
        let v = history_value(&r, Some(2), Some(T0 + 1), T0 + 10);
        let ids: Vec<&str> = v["sends"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["4", "3"]);
        assert_eq!(v["count"], json!(2));
    }

    #[test]
    fn select_filters_then_limits_newest_first() {
        let r: Vec<SendRecord> = (0..5)
            .map(|i| rec(&format!("{i}"), T0 + i as u64, Origin::Send, Status::Broadcast))
            .collect();
        let ids: Vec<String> = select(&r, Some(2), Some(T0 + 1)).into_iter().map(|x| x.intent.id).collect();
        assert_eq!(ids, ["4", "3"]);
        assert_eq!(select(&r, None, None).len(), 5);
        assert_eq!(select(&r, None, Some(T0 + 4)).len(), 1);
    }

    // -------- CSV -------------------------------------------------

    #[test]
    fn csv_has_header_and_rows_newest_first() {
        let r = vec![
            rec("a", T0, Origin::Send, Status::Broadcast),
            rec("b", T0 + 1, task(7), Status::Failed),
        ];
        let csv = to_csv(&r);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "time,status,amount_piv,amount_usd,fee_piv,from,to,reason,origin,txid,id");
        assert!(lines[1].starts_with(&crate::timefmt::format_rfc3339(T0 + 1)));
        assert!(lines[1].contains(",failed,0.00000010,,0.00001000,public,DDest,reason b,task_approve:7:otter,"));
        assert!(lines[2].ends_with(&format!(",{},a", txid(b'a'))));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn usd_value_uses_price_recorded_at_send_time() {
        let mut r = vec![rec("a", T0, Origin::Send, Status::Broadcast), rec("b", T0 + 1, Origin::Send, Status::Broadcast)];
        r[0].intent.amount_sat = 7_258_711_361;
        r[0].intent.usd_price_e8 = Some(6_888_275);
        let v = history_value(&r, None, None, T0 + 10);
        assert_eq!(v["sends"][1]["amount_usd"], json!("5.00"));
        assert!(v["sends"][0].get("amount_usd").is_none(), "no price recorded → no USD value");
        let csv = to_csv(&r);
        assert!(csv.lines().nth(2).unwrap().contains(",72.58711361,5.00,"), "{csv}");
    }

    #[test]
    fn history_usd_total_only_when_every_send_has_a_price() {
        let mut r = vec![rec("a", T0, Origin::Send, Status::Broadcast)];
        r[0].intent.usd_price_e8 = Some(100_000_000); // $1/PIV, 5 PIV, no fee → $5.00
        r[0].intent.amount_sat = 500_000_000;
        r[0].intent.fee_sat = 0;
        let v = history_value(&r, None, None, T0 + 10);
        assert_eq!(v["totals"]["last_24h_usd"], json!("5.00"));
        r.push(rec("b", T0 + 1, Origin::Send, Status::Broadcast)); // no price
        let v = history_value(&r, None, None, T0 + 10);
        assert!(v["totals"].get("last_24h_usd").is_none());
    }

    #[test]
    fn csv_escapes_commas_quotes_and_newlines() {
        let mut r = vec![rec("a", T0, Origin::Send, Status::Broadcast)];
        r[0].intent.reason = Some("logo, \"v2\"\nfinal".into());
        let csv = to_csv(&r);
        assert!(csv.contains(",\"logo, \"\"v2\"\"\nfinal\","), "{csv}");
    }

    #[test]
    fn csv_neutralizes_spreadsheet_formulas() {
        for evil in ["=HYPERLINK(\"x\")", "+1", "-2", "@SUM(A1)", "\tx"] {
            let mut r = vec![rec("a", T0, Origin::Send, Status::Broadcast)];
            r[0].intent.reason = Some(evil.into());
            let csv = to_csv(&r);
            let row = csv.lines().nth(1).unwrap();
            assert!(!row.contains(&format!(",{evil}")), "unescaped formula in {row:?}");
            assert!(row.contains(",'") || row.contains(",\"'"), "missing ' prefix in {row:?}");
        }
    }

    #[test]
    fn csv_handles_missing_reason() {
        let mut r = vec![rec("a", T0, Origin::Send, Status::Broadcast)];
        r[0].intent.reason = None;
        assert!(to_csv(&r).lines().nth(1).unwrap().contains(",DDest,,send,"));
    }

    // -------- ids -------------------------------------------------

    #[test]
    fn ids_are_16_hex_and_distinct() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
