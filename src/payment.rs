//! The broadcast half of a payment: double-pay check, ledger intent,
//! broadcast, outcome. Split out of `core::send` (which owns wallet
//! loading, syncing and tx building) so the ordering and every error
//! branch can be tested against a fake network.
//!
//! Callers must hold the ledger's spend lock across [`commit`].

use std::error::Error;

use crate::ledger::{self, ChainLookup, Event, Ledger, SendIntent};
use crate::spend::{self, Origin, SendRequest};

/// Broadcasting plus lookup — what [`commit`] needs from the network.
pub trait Network: ChainLookup {
    /// Submit a raw transaction. `Ok` carries whatever the endpoint
    /// reported, which should be the txid but isn't guaranteed.
    fn broadcast(&self, txhex: &str) -> Result<String, Box<dyn Error>>;
}

/// A built, signed transaction ready to broadcast.
pub struct Built {
    pub txhex: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
}

#[derive(Debug, PartialEq)]
pub enum Committed {
    /// Broadcast and recorded. The caller should mark inputs spent.
    Sent { id: String, txid: String },
    /// An earlier attempt for the same origin already paid; nothing
    /// was broadcast.
    AlreadyPaid { txid: String },
}

/// Check for an earlier payment of the same origin, write the intent,
/// broadcast, and record the outcome. Every error after the intent is
/// written names the txid; an outcome that can't be established is
/// left `unresolved` in the ledger, never recorded as failed.
pub fn commit(
    ledger: &Ledger,
    net: &dyn Network,
    req: &SendRequest,
    built: &Built,
    wallet: &str,
    usd_price_e8: Option<u64>,
    now: &dyn Fn() -> u64,
) -> Result<Committed, Box<dyn Error>> {
    // Fail closed if the ledger can't be read, for every payment: a
    // spend the ledger can't see would also be invisible to history
    // and (with PR 3) to spending limits.
    ledger
        .records(wallet)
        .map_err(|e| format!("{}; payment NOT sent", e))?;

    // Re-check under the caller's spend lock: another process may have
    // paid the same task or order since the caller last looked.
    if req.origin != Origin::Send {
        if let Some(txid) = ledger::prior_payment(ledger, wallet, &req.origin, net, now())? {
            return Ok(Committed::AlreadyPaid { txid });
        }
    }

    let txid = spend::compute_txid(&built.txhex)?;
    let id = ledger::new_id();
    ledger
        .append(&Event::SendIntent(SendIntent {
            id: id.clone(),
            ts: now(),
            wallet: wallet.to_string(),
            to: req.address.clone(),
            amount_sat: built.amount_sat,
            fee_sat: built.fee_sat,
            from: req.from,
            memo: req.memo.clone(),
            reason: req.reason.clone(),
            origin: req.origin.clone(),
            txid: txid.clone(),
            usd_price_e8,
        }))
        .map_err(|e| format!("could not write the spend ledger; payment NOT sent: {}", e))?;

    let unknown = |why: String| -> Box<dyn Error> {
        format!(
            "{} — outcome unknown for transaction {}. Check `history` before retrying; it will show whether it went through.",
            why, txid
        )
        .into()
    };

    match net.broadcast(&built.txhex) {
        Ok(reported) if spend::is_txid(&reported) => {
            if !reported.eq_ignore_ascii_case(&txid) {
                eprintln!("warning: network reported txid {} but the kit computed {}", reported, txid);
            }
        }
        // Success status but no txid in the body: only believe it once
        // the network actually knows the transaction.
        Ok(reported) => match net.tx_exists(&txid) {
            Ok(true) => {}
            Ok(false) => return Err(unknown(format!("broadcast returned {:?} and the tx isn't visible yet", reported))),
            Err(e) => return Err(unknown(format!("broadcast returned {:?}; lookup failed: {}", reported, e))),
        },
        // The call can fail after the tx already reached the network
        // (timeouts, a flaky endpoint). Check before calling it failed.
        Err(e) => match net.tx_exists(&txid) {
            Ok(true) => eprintln!("broadcast reported an error but the transaction is on the network: {}", e),
            Ok(false) => {
                record(ledger, Event::SendFailed { id, ts: now(), error: e.to_string() });
                return Err(format!(
                    "{} — transaction {} isn't on the network. It's recorded as failed; `history` re-checks it \
                     later in case it surfaces. Don't resend until `history` still shows it failed.",
                    e, txid
                )
                .into());
            }
            Err(lookup) => return Err(unknown(format!("broadcast failed ({}) and lookup failed ({})", e, lookup))),
        },
    }

    record(ledger, Event::SendBroadcast { id: id.clone(), ts: now(), txid: txid.clone() });
    Ok(Committed::Sent { id, txid })
}

/// Append a settlement event. The payment's fate is already decided, so
/// a ledger write failure is reported, not returned as an error.
fn record(ledger: &Ledger, event: Event) {
    if let Err(e) = ledger.append(&event) {
        eprintln!("warning: could not record payment outcome in the ledger: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::temp_dir;
    use crate::ledger::{Status, LEDGER_FILE};
    use crate::spend::Pool;
    use std::cell::RefCell;

    const W: &str = "DWalletAddr";
    const NOW: u64 = 1_790_000_000;

    fn built() -> (Built, String) {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/mainnet_tx_v1_transparent.json")).unwrap();
        (
            Built { txhex: v["hex"].as_str().unwrap().to_string(), amount_sat: 50_000_000, fee_sat: 2_280 },
            v["txid"].as_str().unwrap().to_string(),
        )
    }

    fn req(origin: Origin) -> SendRequest {
        SendRequest {
            address: "DDest".into(),
            amount_sat: 50_000_000,
            memo: String::new(),
            from: Pool::Public,
            reason: Some("test".into()),
            origin,
            price: None,
        }
    }

    fn task() -> Origin {
        Origin::TaskApprove { task_id: 12, worker: "otter".into() }
    }

    type Answer<T> = Box<dyn Fn(&str) -> Result<T, String>>;

    /// Fake network. `broadcast` records how many intents the ledger
    /// held at the moment of broadcast, to prove intent-first ordering.
    struct FakeNet<'a> {
        ledger: &'a Ledger,
        broadcast: Answer<String>,
        lookup: Answer<bool>,
        intents_seen_at_broadcast: RefCell<Vec<usize>>,
        lookups: RefCell<u32>,
    }

    impl<'a> FakeNet<'a> {
        fn new(
            ledger: &'a Ledger,
            broadcast: impl Fn(&str) -> Result<String, String> + 'static,
            lookup: impl Fn(&str) -> Result<bool, String> + 'static,
        ) -> Self {
            Self {
                ledger,
                broadcast: Box::new(broadcast),
                lookup: Box::new(lookup),
                intents_seen_at_broadcast: RefCell::new(vec![]),
                lookups: RefCell::new(0),
            }
        }
    }

    impl ChainLookup for FakeNet<'_> {
        fn tx_exists(&self, txid: &str) -> Result<bool, Box<dyn Error>> {
            *self.lookups.borrow_mut() += 1;
            (self.lookup)(txid).map_err(|e| e.into())
        }
    }

    impl Network for FakeNet<'_> {
        fn broadcast(&self, txhex: &str) -> Result<String, Box<dyn Error>> {
            let n = self
                .ledger
                .read_events()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, Event::SendIntent(_)))
                .count();
            self.intents_seen_at_broadcast.borrow_mut().push(n);
            (self.broadcast)(txhex).map_err(|e| e.into())
        }
    }

    fn ledger(tag: &str) -> Ledger {
        Ledger::at(temp_dir(tag).join(LEDGER_FILE))
    }

    fn no_lookup(_: &str) -> Result<bool, String> {
        panic!("lookup not expected")
    }

    #[test]
    fn success_writes_intent_before_broadcast_and_records_it() {
        let l = ledger("pay-ok");
        let (b, txid) = built();
        let t = txid.clone();
        let net = FakeNet::new(&l, move |_| Ok(t.clone()), no_lookup);
        let out = commit(&l, &net, &req(Origin::Send), &b, W, Some(6_888_275), &|| NOW).unwrap();
        let Committed::Sent { id, txid: got } = out else { panic!("expected Sent") };
        assert_eq!(got, txid);
        assert_eq!(*net.intents_seen_at_broadcast.borrow(), vec![1], "intent must exist when broadcasting");
        let r = &l.records(W).unwrap()[0];
        assert_eq!(r.intent.id, id);
        assert_eq!(r.intent.txid, txid);
        assert_eq!(r.intent.amount_sat, 50_000_000);
        assert_eq!(r.intent.fee_sat, 2_280);
        assert_eq!(r.intent.reason.as_deref(), Some("test"));
        assert_eq!(r.intent.usd_price_e8, Some(6_888_275));
        assert_eq!(r.status, Status::Broadcast);
    }

    #[test]
    fn unwritable_ledger_means_nothing_is_broadcast() {
        let dir = temp_dir("pay-unwritable");
        std::fs::create_dir_all(dir.join(LEDGER_FILE)).unwrap(); // a directory, not a file
        let l = Ledger::at(dir.join(LEDGER_FILE));
        let (b, _) = built();
        let net = FakeNet::new(&l, |_| panic!("must not broadcast"), no_lookup);
        let err = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap_err().to_string();
        assert!(err.contains("NOT sent"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn failed_intent_write_means_nothing_is_broadcast() {
        use std::os::unix::fs::PermissionsExt;
        let l = ledger("pay-readonly");
        let path = crate::ledger::tests::ledger_path(&l);
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let (b, _) = built();
        let net = FakeNet::new(&l, |_| panic!("must not broadcast"), no_lookup);
        let err = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap_err().to_string();
        assert!(err.contains("could not write the spend ledger; payment NOT sent"), "{err}");
    }

    #[test]
    fn broadcast_error_but_tx_on_chain_counts_as_sent() {
        let l = ledger("pay-err-found");
        let (b, txid) = built();
        let net = FakeNet::new(&l, |_| Err("timeout".into()), |_| Ok(true));
        let out = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap();
        assert!(matches!(out, Committed::Sent { txid: ref t, .. } if *t == txid));
        assert_eq!(l.records(W).unwrap()[0].status, Status::Broadcast);
    }

    #[test]
    fn broadcast_error_and_tx_definitely_absent_is_failed() {
        let l = ledger("pay-err-absent");
        let (b, txid) = built();
        let net = FakeNet::new(&l, |_| Err("bad-txns-inputs-spent".into()), |_| Ok(false));
        let err = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap_err().to_string();
        assert!(err.contains("bad-txns-inputs-spent") && err.contains(&txid), "{err}");
        // Two sources not seeing it seconds after a timeout isn't proof it never will.
        assert!(!err.contains("was not sent"), "{err}");
        assert!(err.contains("history"), "{err}");
        let r = &l.records(W).unwrap()[0];
        assert_eq!(r.status, Status::Failed);
    }

    #[test]
    fn broadcast_error_and_lookup_unreachable_stays_unresolved() {
        let l = ledger("pay-err-unknown");
        let (b, txid) = built();
        let net = FakeNet::new(&l, |_| Err("timeout".into()), |_| Err("explorers down".into()));
        let err = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap_err().to_string();
        assert!(err.contains(&txid), "{err}");
        assert!(err.contains("history"), "should tell the caller to check history: {err}");
        assert_eq!(l.records(W).unwrap()[0].status, Status::Unresolved);
    }

    #[test]
    fn non_txid_success_body_is_confirmed_before_recording() {
        let l = ledger("pay-weird-ok");
        let (b, _) = built();
        let net = FakeNet::new(&l, |_| Ok("{\"result\":null}".into()), |_| Ok(true));
        assert!(matches!(commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap(), Committed::Sent { .. }));
        assert_eq!(*net.lookups.borrow(), 1);
        assert_eq!(l.records(W).unwrap()[0].status, Status::Broadcast);
    }

    #[test]
    fn non_txid_success_body_unconfirmed_is_unresolved_error() {
        let l = ledger("pay-weird-unknown");
        let (b, txid) = built();
        let net = FakeNet::new(&l, |_| Ok("<html>".into()), |_| Ok(false));
        let err = commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap_err().to_string();
        assert!(err.contains(&txid), "{err}");
        assert_eq!(l.records(W).unwrap()[0].status, Status::Unresolved);
    }

    #[test]
    fn earlier_payment_for_same_origin_is_reused_without_broadcast() {
        let l = ledger("pay-reuse");
        let (b, txid) = built();
        let first = FakeNet::new(&l, |_| Ok(String::new()), |_| Ok(true));
        commit(&l, &first, &req(task()), &b, W, None, &|| NOW).unwrap();
        let second = FakeNet::new(&l, |_| panic!("must not pay twice"), no_lookup);
        let out = commit(&l, &second, &req(task()), &b, W, None, &|| NOW + 10_000).unwrap();
        assert_eq!(out, Committed::AlreadyPaid { txid });
        assert_eq!(l.records(W).unwrap().len(), 1);
    }

    #[test]
    fn in_flight_attempt_for_same_origin_blocks() {
        let l = ledger("pay-inflight");
        let (b, txid) = built();
        let first = FakeNet::new(&l, |_| Err("timeout".into()), |_| Err("down".into()));
        assert!(commit(&l, &first, &req(task()), &b, W, None, &|| NOW).is_err());
        let second = FakeNet::new(&l, |_| panic!("must not pay"), no_lookup);
        let err = commit(&l, &second, &req(task()), &b, W, None, &|| NOW + 30).unwrap_err().to_string();
        assert!(err.contains(&txid) && err.contains("in progress"), "{err}");
    }

    #[test]
    fn plain_send_refuses_when_ledger_unreadable() {
        let l = ledger("pay-future-ledger");
        std::fs::write(crate::ledger::tests::ledger_path(&l), "{\"v\":2,\"event\":\"send_intent\"}\n").unwrap();
        let (b, _) = built();
        let net = FakeNet::new(&l, |_| panic!("must not broadcast"), no_lookup);
        assert!(commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).is_err());
    }

    #[test]
    fn timestamps_are_read_at_each_write() {
        // The guard's lookups can take minutes; the intent must not be "born old".
        let l = ledger("pay-clock");
        let (b, txid) = built();
        let t = txid.clone();
        let net = FakeNet::new(&l, move |_| Ok(t.clone()), no_lookup);
        let tick = std::cell::Cell::new(NOW);
        let clock = || {
            tick.set(tick.get() + 100);
            tick.get()
        };
        commit(&l, &net, &req(Origin::Send), &b, W, None, &clock).unwrap();
        let events = l.read_events().unwrap();
        let Event::SendIntent(i) = &events[0] else { panic!() };
        let Event::SendBroadcast { ts, .. } = &events[1] else { panic!() };
        assert!(*ts > i.ts, "outcome must be stamped after the intent");
    }

    #[test]
    fn plain_sends_are_never_deduplicated() {
        let l = ledger("pay-plain");
        let (b, txid) = built();
        let t = txid.clone();
        let net = FakeNet::new(&l, move |_| Ok(t.clone()), no_lookup);
        commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW).unwrap();
        commit(&l, &net, &req(Origin::Send), &b, W, None, &|| NOW + 1).unwrap();
        assert_eq!(net.intents_seen_at_broadcast.borrow().len(), 2);
    }
}
