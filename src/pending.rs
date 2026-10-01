//! Payments waiting for an operator's approval (`pending.json`).
//!
//! When the spending policy says "ask first", `core::send` records the
//! full request here — including what triggered it, so approving a task
//! bounty also records the approval with the platform, and approving a
//! card payment re-checks the order — and returns `pending_approval`
//! instead of paying. `pivx-agent-kit pending approve <id>` (a human, at
//! a terminal) replays it. Entries expire after [`PENDING_TTL_SECS`].

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::PathBuf;

use crate::price::Price;
use crate::spend::{Origin, Pool, SendRequest};

pub const PENDING_FILE: &str = "pending.json";
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The entry can't be acted on (taken by another approval, expired,
/// another wallet's). Callers that didn't take the entry must not
/// record an outcome for it.
#[derive(Debug)]
pub struct NotActionable(pub String);

impl std::fmt::Display for NotActionable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for NotActionable {}

fn check_actionable(entry: &PendingEntry, wallet: &str, now: u64) -> Result<(), NotActionable> {
    let err = |m: String| Err(NotActionable(m));
    if entry.wallet != wallet {
        return err(format!("pending payment {} belongs to a different wallet", entry.id));
    }
    if entry.status != PendingStatus::Pending {
        let status = serde_json::to_value(entry.status).ok();
        return err(format!(
            "pending payment {} was already {}",
            entry.id,
            status.as_ref().and_then(|s| s.as_str()).unwrap_or("resolved")
        ));
    }
    if entry.is_expired(now) {
        return err(format!(
            "pending payment {} expired at {}; ask the agent to request it again",
            entry.id,
            crate::timefmt::format_rfc3339(entry.expires_at)
        ));
    }
    Ok(())
}
pub const PENDING_TTL_SECS: u64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingStatus {
    Pending,
    Approved,
    Denied,
    /// Approved, but the replay failed (e.g. card order expired).
    Failed,
    /// Replaced by a newer request for the same task/order whose
    /// amount, destination or balance changed.
    Superseded,
    /// Approved and being paid by the process in `claimed_by`. Left
    /// behind by a crash, it means "outcome unknown — check history".
    Sending,
}

/// The payment as requested, frozen at request time (the PIV amount
/// doesn't move with the price while it waits).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredRequest {
    pub address: String,
    pub amount_sat: u64,
    #[serde(default)]
    pub memo: String,
    pub from: Pool,
    #[serde(default)]
    pub reason: Option<String>,
    pub origin: Origin,
    #[serde(default)]
    pub price: Option<Price>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingEntry {
    pub id: String,
    pub wallet: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub request: StoredRequest,
    /// Why it needs approval, from the policy.
    pub reasons: Vec<String>,
    pub status: PendingStatus,
    #[serde(default)]
    pub resolved_at: Option<u64>,
    /// Outcome note: txid on success, error on failure.
    #[serde(default)]
    pub note: Option<String>,
    /// A checked price at request time, for showing the operator what
    /// the payment was worth then. Never used to evaluate limits.
    #[serde(default)]
    pub request_price: Option<Price>,
    /// Which process claimed it for sending (see [`run_token`]).
    #[serde(default)]
    pub claimed_by: Option<String>,
}

/// Identifies this process when it claims an entry, so it only ever
/// records the outcome of an entry it claimed itself.
pub fn run_token() -> &'static str {
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| format!("{}-{}", std::process::id(), crate::ledger::new_id()))
}

impl PendingEntry {
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

impl StoredRequest {
    pub fn from_request(req: &SendRequest) -> Self {
        Self {
            address: req.address.clone(),
            amount_sat: req.amount_sat,
            memo: req.memo.clone(),
            from: req.from,
            reason: req.reason.clone(),
            origin: req.origin.clone(),
            price: req.price.clone(),
        }
    }

    pub fn to_request(&self) -> SendRequest {
        SendRequest {
            address: self.address.clone(),
            amount_sat: self.amount_sat,
            memo: self.memo.clone(),
            from: self.from,
            reason: self.reason.clone(),
            origin: self.origin.clone(),
            price: self.price.clone(),
        }
    }
}

/// An approval covers exactly the payment that was held. Replaying it
/// with a different destination, amount, balance or trigger (say the
/// bounty changed on the platform) must be refused.
pub fn check_matches(entry: &PendingEntry, req: &SendRequest) -> Result<(), String> {
    let held = &entry.request;
    let mismatch = |what: &str, was: String, now: String| {
        Err(format!(
            "pending payment {} was approved for {} {}, but the payment is now {}; nothing was sent. \
             Have the agent request it again.",
            entry.id, what, was, now
        ))
    };
    if held.address != req.address {
        return mismatch("destination", held.address.clone(), req.address.clone());
    }
    if held.amount_sat != req.amount_sat {
        return mismatch("amount", format!("{} sat", held.amount_sat), format!("{} sat", req.amount_sat));
    }
    if held.from != req.from {
        return mismatch("balance", held.from.to_string(), req.from.to_string());
    }
    if held.origin != req.origin {
        return mismatch("trigger", format!("{:?}", held.origin), format!("{:?}", req.origin));
    }
    Ok(())
}

/// New pending entry for `req`, id `p_` + 12 hex chars.
pub fn new_entry(
    req: &SendRequest,
    wallet: &str,
    reasons: Vec<String>,
    request_price: Option<Price>,
    now: u64,
) -> PendingEntry {
    PendingEntry {
        id: format!("p_{}", &crate::ledger::new_id()[..12]),
        wallet: wallet.to_string(),
        created_at: now,
        expires_at: now + PENDING_TTL_SECS,
        request: StoredRequest::from_request(req),
        reasons,
        status: PendingStatus::Pending,
        resolved_at: None,
        note: None,
        request_price,
        claimed_by: None,
    }
}

pub struct PendingStore {
    path: PathBuf,
}

/// Held while `pending.json` is being rewritten.
#[derive(Debug)]
pub struct PendingLock {
    _file: std::fs::File,
}

impl PendingStore {
    pub fn open_default() -> Self {
        Self::at(crate::wallet::get_data_dir().join(PENDING_FILE))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path_for_tests(&self) -> PathBuf {
        self.path.clone()
    }

    /// All entries, newest first. Missing file is empty; an unreadable
    /// or corrupt file is an error.
    pub fn list(&self) -> Result<Vec<PendingEntry>, Box<dyn Error>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read pending payments {}: {}", self.path.display(), e).into()),
        };
        serde_json::from_str(&text)
            .map_err(|e| format!("pending payments file {} is corrupt: {}", self.path.display(), e).into())
    }

    pub(crate) fn write(&self, entries: &[PendingEntry]) -> Result<(), Box<dyn Error>> {
        crate::policy::write_private(&self.path, serde_json::to_string_pretty(entries)?.as_bytes())
    }

    pub fn add(&self, entry: &PendingEntry) -> Result<(), Box<dyn Error>> {
        let _lock = self.lock(LOCK_WAIT)?;
        let mut entries = self.list()?;
        entries.insert(0, entry.clone());
        self.write(&entries)
    }

    /// The entry, if it exists, belongs to `wallet`, is still `Pending`
    /// and hasn't expired. Errors say which of those failed.
    pub fn get_actionable(&self, id: &str, wallet: &str, now: u64) -> Result<PendingEntry, Box<dyn Error>> {
        let entry = self
            .list()?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no pending payment with id {}", id))?;
        check_actionable(&entry, wallet, now)?;
        Ok(entry)
    }

    /// Exclusive lock for read-modify-write of `pending.json`.
    pub fn lock(&self, wait: std::time::Duration) -> Result<PendingLock, Box<dyn Error>> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(PendingLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
            if std::time::Instant::now() >= deadline {
                return Err("pending payments file is busy (another command is updating it); retry".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Compare-and-set: record `status` only if the entry is currently
    /// in `from` (and, for `Sending`, claimed by this process). Returns
    /// whether it was recorded; never overwrites someone else's outcome.
    pub fn resolve_from(
        &self,
        id: &str,
        from: PendingStatus,
        status: PendingStatus,
        note: Option<String>,
        now: u64,
    ) -> Result<bool, Box<dyn Error>> {
        let _lock = self.lock(LOCK_WAIT)?;
        let mut entries = self.list()?;
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no pending payment with id {}", id))?;
        let ours = from != PendingStatus::Sending || entry.claimed_by.as_deref() == Some(run_token());
        if entry.status != from || !ours {
            return Ok(false);
        }
        entry.status = status;
        entry.resolved_at = Some(now);
        entry.note = note;
        self.write(&entries)?;
        Ok(true)
    }

    /// Unconditionally record an outcome. Test setup only: production
    /// code goes through [`resolve_from`] so it never overwrites another
    /// command's outcome.
    #[cfg(test)]
    pub(crate) fn resolve(&self, id: &str, status: PendingStatus, note: Option<String>, now: u64) -> Result<(), Box<dyn Error>> {
        let _lock = self.lock(LOCK_WAIT)?;
        let mut entries = self.list()?;
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no pending payment with id {}", id))?;
        entry.status = status;
        entry.resolved_at = Some(now);
        entry.note = note;
        self.write(&entries)
    }

    /// An existing open entry for the same task/order (never plain
    /// sends) so agent retries don't pile up duplicates. If the open
    /// entry's amount, destination or balance differ from `req`, it's
    /// marked superseded and `None` is returned so a fresh entry is made.
    pub fn find_open_for(&self, req: &SendRequest, wallet: &str, now: u64) -> Result<Option<PendingEntry>, Box<dyn Error>> {
        if req.origin == Origin::Send {
            return Ok(None);
        }
        let open = self.list()?.into_iter().find(|e| {
            e.wallet == wallet && e.request.origin == req.origin && e.status == PendingStatus::Pending && !e.is_expired(now)
        });
        match open {
            Some(e) if check_matches(&e, req).is_ok() => Ok(Some(e)),
            Some(e) => {
                self.resolve_from(
                    &e.id,
                    PendingStatus::Pending,
                    PendingStatus::Superseded,
                    Some("replaced by a newer request with different details".into()),
                    now,
                )?;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Mark an approved entry as being paid, under the spend lock, so a
    /// second concurrent approval of the same id can't pay again.
    pub fn consume(&self, id: &str, wallet: &str, now: u64) -> Result<PendingEntry, Box<dyn Error>> {
        let _lock = self.lock(LOCK_WAIT)?;
        let mut entries = self.list()?;
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no pending payment with id {}", id))?;
        check_actionable(entry, wallet, now)?;
        entry.status = PendingStatus::Sending;
        entry.resolved_at = Some(now);
        entry.note = Some("approved; sending (if this stays, check `history` for the outcome)".into());
        entry.claimed_by = Some(run_token().to_string());
        let taken = entry.clone();
        self.write(&entries)?;
        Ok(taken)
    }

    /// Other open entries that would pay the same destination the same
    /// amount (for the approval prompt's duplicate warning).
    pub fn open_duplicates_of(&self, entry: &PendingEntry, now: u64) -> Result<Vec<PendingEntry>, Box<dyn Error>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|e| {
                e.id != entry.id
                    && e.wallet == entry.wallet
                    && e.status == PendingStatus::Pending
                    && !e.is_expired(now)
                    && e.request.address == entry.request.address
                    && e.request.amount_sat == entry.request.amount_sat
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "DWallet";
    const NOW: u64 = 1_790_000_000;

    fn req(origin: Origin) -> SendRequest {
        SendRequest {
            address: "DDest".into(),
            amount_sat: 14_517_000_000,
            memo: "m".into(),
            from: Pool::Private,
            reason: Some("Steam card".into()),
            origin,
            price: None,
        }
    }

    fn store(tag: &str) -> PendingStore {
        PendingStore::at(crate::ledger::tests::temp_dir(tag).join(PENDING_FILE))
    }

    fn cards(o: &str) -> Origin {
        Origin::CardsPay { order_id: o.into() }
    }

    #[test]
    fn new_entry_shape() {
        let e = new_entry(&req(Origin::Send), W, vec!["too big".into()], None, NOW);
        assert!(e.id.starts_with("p_") && e.id.len() == 14, "{}", e.id);
        assert!(e.id[2..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(e.expires_at, NOW + PENDING_TTL_SECS);
        assert_eq!(e.status, PendingStatus::Pending);
        assert_eq!(e.request.to_request().amount_sat, 14_517_000_000);
        assert_ne!(new_entry(&req(Origin::Send), W, vec![], None, NOW).id, e.id);
    }

    #[test]
    fn stored_request_round_trips() {
        let r = req(Origin::TaskApprove { task_id: 3, worker: "otter".into() });
        let back = StoredRequest::from_request(&r).to_request();
        assert_eq!(back.address, r.address);
        assert_eq!(back.amount_sat, r.amount_sat);
        assert_eq!(back.memo, r.memo);
        assert_eq!(back.from, r.from);
        assert_eq!(back.reason, r.reason);
        assert_eq!(back.origin, r.origin);
    }

    #[test]
    fn approval_binds_to_the_exact_payment() {
        let r = req(cards("ord1"));
        let e = new_entry(&r, W, vec![], None, NOW);
        assert!(check_matches(&e, &r).is_ok());
        let mut changed = r.clone();
        changed.amount_sat += 1;
        assert!(check_matches(&e, &changed).unwrap_err().contains("amount"));
        let mut changed = r.clone();
        changed.address = "DElsewhere".into();
        assert!(check_matches(&e, &changed).unwrap_err().contains("destination"));
        let mut changed = r.clone();
        changed.from = Pool::Public;
        assert!(check_matches(&e, &changed).is_err());
        let mut changed = r.clone();
        changed.origin = cards("ord2");
        assert!(check_matches(&e, &changed).is_err());
        // Reason and memo text may differ (e.g. regenerated reason).
        let mut changed = r;
        changed.reason = Some("other words".into());
        assert!(check_matches(&e, &changed).is_ok());
    }

    #[test]
    fn add_and_list_newest_first() {
        let s = store("pend-list");
        assert!(s.list().unwrap().is_empty());
        let a = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        let b = new_entry(&req(Origin::Send), W, vec![], None, NOW + 5);
        s.add(&a).unwrap();
        s.add(&b).unwrap();
        let ids: Vec<String> = s.list().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, [b.id, a.id]);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let s = store("pend-corrupt");
        std::fs::write(&s.path, "not json").unwrap();
        assert!(s.list().is_err());
    }

    #[test]
    fn actionable_checks() {
        let s = store("pend-actionable");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        assert_eq!(s.get_actionable(&e.id, W, NOW + 10).unwrap().id, e.id);

        let err = s.get_actionable("p_000000000000", W, NOW).unwrap_err().to_string();
        assert!(err.contains("no pending payment"), "{err}");
        let err = s.get_actionable(&e.id, "DOtherWallet", NOW).unwrap_err().to_string();
        assert!(err.contains("different wallet"), "{err}");
        let err = s.get_actionable(&e.id, W, NOW + PENDING_TTL_SECS).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");

        s.resolve(&e.id, PendingStatus::Denied, None, NOW + 20).unwrap();
        let err = s.get_actionable(&e.id, W, NOW + 30).unwrap_err().to_string();
        assert!(err.contains("denied"), "{err}");
    }

    #[test]
    fn resolve_records_outcome() {
        let s = store("pend-resolve");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        s.resolve(&e.id, PendingStatus::Approved, Some("txid abc".into()), NOW + 7).unwrap();
        let got = &s.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved);
        assert_eq!(got.resolved_at, Some(NOW + 7));
        assert_eq!(got.note.as_deref(), Some("txid abc"));
        assert!(s.resolve("p_nope", PendingStatus::Denied, None, NOW).is_err());
    }

    #[test]
    fn open_entry_for_same_origin_is_found() {
        let s = store("pend-dedupe");
        let r = req(cards("ord1"));
        let e = new_entry(&r, W, vec![], None, NOW);
        s.add(&e).unwrap();
        assert_eq!(s.find_open_for(&r, W, NOW + 1).unwrap().map(|x| x.id), Some(e.id.clone()));
        assert_eq!(s.find_open_for(&req(cards("ord2")), W, NOW + 1).unwrap(), None);
        assert_eq!(s.find_open_for(&r, "DOther", NOW + 1).unwrap(), None);
        // Expired or resolved entries don't count.
        assert_eq!(s.find_open_for(&r, W, NOW + PENDING_TTL_SECS).unwrap(), None);
        s.resolve(&e.id, PendingStatus::Denied, None, NOW + 2).unwrap();
        assert_eq!(s.find_open_for(&r, W, NOW + 3).unwrap(), None);
    }

    #[test]
    fn changed_retry_supersedes_the_old_entry() {
        let s = store("pend-supersede");
        let r = req(cards("ord1"));
        let e = new_entry(&r, W, vec![], None, NOW);
        s.add(&e).unwrap();
        let mut bigger = r.clone();
        bigger.amount_sat += 1;
        assert_eq!(s.find_open_for(&bigger, W, NOW + 1).unwrap(), None);
        assert_eq!(s.list().unwrap()[0].status, PendingStatus::Superseded);
    }

    #[test]
    fn plain_sends_are_never_deduplicated() {
        let s = store("pend-plain");
        let r = req(Origin::Send);
        s.add(&new_entry(&r, W, vec![], None, NOW)).unwrap();
        assert_eq!(s.find_open_for(&r, W, NOW + 1).unwrap(), None);
    }

    #[test]
    fn consume_is_single_use_and_marks_sending() {
        let s = store("pend-consume");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        assert_eq!(s.consume(&e.id, W, NOW + 1).unwrap().id, e.id);
        let got = &s.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Sending);
        assert_eq!(got.claimed_by.as_deref(), Some(run_token()));
        let err = s.consume(&e.id, W, NOW + 2).unwrap_err().to_string();
        assert!(err.contains("already sending"), "{err}");
    }

    #[test]
    fn resolve_from_is_compare_and_set() {
        let s = store("pend-cas");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        // Deny only from pending.
        assert!(s.resolve_from(&e.id, PendingStatus::Pending, PendingStatus::Denied, None, NOW).unwrap());
        assert!(!s.resolve_from(&e.id, PendingStatus::Pending, PendingStatus::Denied, None, NOW).unwrap());
    }

    #[test]
    fn deny_cannot_overwrite_a_paid_entry() {
        let s = store("pend-deny-paid");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        s.resolve(&e.id, PendingStatus::Approved, Some("sent txid abc".into()), NOW + 1).unwrap();
        assert!(!s.resolve_from(&e.id, PendingStatus::Pending, PendingStatus::Denied, None, NOW + 2).unwrap());
        let got = &s.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved);
        assert_eq!(got.note.as_deref(), Some("sent txid abc"));
    }

    #[test]
    fn only_the_claiming_process_settles_a_sending_entry() {
        let s = store("pend-claim");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        s.consume(&e.id, W, NOW).unwrap();
        // Another process's claim (simulate by rewriting claimed_by).
        let mut all = s.list().unwrap();
        all[0].claimed_by = Some("someone-else".into());
        s.write(&all).unwrap();
        assert!(!s.resolve_from(&e.id, PendingStatus::Sending, PendingStatus::Failed, Some("x".into()), NOW).unwrap());
        assert_eq!(s.list().unwrap()[0].status, PendingStatus::Sending);
    }

    #[test]
    fn consume_checks_expiry_and_wallet() {
        let s = store("pend-consume-checks");
        let e = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        s.add(&e).unwrap();
        assert!(s.consume(&e.id, "DOther", NOW).is_err());
        assert!(s.consume(&e.id, W, e.expires_at).is_err());
    }

    #[test]
    fn duplicates_are_open_entries_with_same_destination_and_amount() {
        let s = store("pend-dupes");
        let a = new_entry(&req(Origin::Send), W, vec![], None, NOW);
        let b = new_entry(&req(Origin::Send), W, vec![], None, NOW + 1);
        let mut other = req(Origin::Send);
        other.amount_sat += 1;
        let c = new_entry(&other, W, vec![], None, NOW + 2);
        for e in [&a, &b, &c] {
            s.add(e).unwrap();
        }
        let d = s.open_duplicates_of(&a, NOW + 3).unwrap();
        assert_eq!(d.iter().map(|x| x.id.clone()).collect::<Vec<_>>(), vec![b.id.clone()]);
    }

    #[test]
    fn writes_are_serialized_by_a_lock() {
        let s = store("pend-lock");
        let held = s.lock(std::time::Duration::from_secs(1)).unwrap();
        let err = s.lock(std::time::Duration::from_millis(200)).unwrap_err().to_string();
        assert!(err.contains("busy"), "{err}");
        drop(held);
        // Release can lag briefly if a concurrent test forks; see the
        // ledger lock test.
        assert!(s.lock(std::time::Duration::from_secs(2)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn pending_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let s = store("pend-perm");
        s.add(&new_entry(&req(Origin::Send), W, vec![], None, NOW)).unwrap();
        assert_eq!(std::fs::metadata(&s.path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
