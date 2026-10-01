//! The spending-policy gate in front of every payment.
//!
//! `core::send` calls [`authorize`] under the spend lock, after loading
//! (and MAC-checking) the policy and before building the transaction.
//! It returns whether to pay now, or the pending entry the payment is
//! now waiting in; a payment the policy refuses outright is an error.

use std::error::Error;

use crate::ledger::{self, SendRecord};
use crate::pending::{self, PendingEntry, PendingStore};
use crate::policy::{self, Decision, Policy, SpendCheck};
use crate::price::Price;
use crate::spend::SendRequest;

#[derive(Debug, PartialEq)]
pub enum Authorized {
    Proceed,
    /// Waiting for an operator. Either newly created or, for a task or
    /// order the agent already asked about, the existing entry.
    Pending(Box<PendingEntry>),
}

/// Apply `policy` (if any) to `req`. `records` are this wallet's ledger
/// records (for the rolling 24h total). `approved` is true when replaying
/// an operator-approved pending entry.
#[allow(clippy::too_many_arguments)]
pub fn authorize(
    policy: Option<&Policy>,
    records: &[SendRecord],
    pending: &PendingStore,
    req: &SendRequest,
    wallet: &str,
    price: Option<&Price>,
    approved: bool,
    now: u64,
) -> Result<Authorized, Box<dyn Error>> {
    let price_e8 = price.map(|p| p.usd_e8);
    let Some(policy) = policy else { return Ok(Authorized::Proceed) };
    let spent_24h = ledger::spent_since(records, now.saturating_sub(ledger::DAY_SECS));
    let check = SpendCheck { amount_sat: req.amount_sat, to: &req.address };

    match policy::evaluate(policy, &check, spent_24h, price_e8, approved) {
        Decision::Allow => Ok(Authorized::Proceed),
        Decision::Deny(why) => Err(format!("blocked by spending policy: {}", why).into()),
        Decision::NeedsApproval(reasons) => {
            if let Some(open) = pending.find_open_for(req, wallet, now)? {
                return Ok(Authorized::Pending(Box::new(open)));
            }
            let entry = pending::new_entry(req, wallet, reasons, price.cloned(), now);
            pending.add(&entry)?;
            Ok(Authorized::Pending(Box::new(entry)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::temp_dir;
    use crate::ledger::{SendIntent, Status};
    use crate::pending::{PendingStatus, PENDING_FILE};
    use crate::policy::Limit;
    use crate::spend::{Origin, Pool};

    const W: &str = "DWallet";
    const NOW: u64 = 1_790_000_000;
    const PIV: u64 = 100_000_000;

    fn req(amount_piv: u64, origin: Origin) -> SendRequest {
        SendRequest {
            address: "DDest".into(),
            amount_sat: amount_piv * PIV,
            memo: String::new(),
            from: Pool::Public,
            reason: Some("r".into()),
            origin,
            price: None,
        }
    }

    fn spent(amount_piv: u64, ts: u64, status: Status) -> SendRecord {
        SendRecord {
            intent: SendIntent {
                id: format!("{ts}"),
                ts,
                wallet: W.into(),
                to: "DX".into(),
                amount_sat: amount_piv * PIV,
                fee_sat: 0,
                from: Pool::Public,
                memo: String::new(),
                reason: None,
                origin: Origin::Send,
                txid: "ab".repeat(32),
                usd_price_e8: None,
            },
            status,
            error: None,
        }
    }

    fn pending(tag: &str) -> PendingStore {
        PendingStore::at(temp_dir(tag).join(PENDING_FILE))
    }

    fn policy() -> Policy {
        Policy {
            max_per_tx: Some(Limit::Piv(100 * PIV)),
            max_daily: Some(Limit::Piv(150 * PIV)),
            approve_above: Some(Limit::Piv(10 * PIV)),
            ..Default::default()
        }
    }

    #[test]
    fn no_policy_proceeds() {
        let p = pending("gate-none");
        let out = authorize(None, &[], &p, &req(1_000_000, Origin::Send), W, None, false, NOW).unwrap();
        assert_eq!(out, Authorized::Proceed);
        assert!(p.list().unwrap().is_empty());
    }

    #[test]
    fn small_payment_proceeds() {
        let p = pending("gate-small");
        assert_eq!(authorize(Some(&policy()), &[], &p, &req(5, Origin::Send), W, None, false, NOW).unwrap(), Authorized::Proceed);
    }

    #[test]
    fn over_threshold_creates_pending_entry() {
        let p = pending("gate-pending");
        let out = authorize(Some(&policy()), &[], &p, &req(20, Origin::Send), W, None, false, NOW).unwrap();
        let Authorized::Pending(e) = out else { panic!("expected pending") };
        assert_eq!(e.request.amount_sat, 20 * PIV);
        assert_eq!(e.wallet, W);
        assert!(e.reasons[0].contains("approval threshold"));
        assert_eq!(p.list().unwrap(), vec![*e]);
    }

    #[test]
    fn over_cap_is_an_error_and_queues_nothing() {
        let p = pending("gate-deny");
        let err = authorize(Some(&policy()), &[], &p, &req(101, Origin::Send), W, None, false, NOW).unwrap_err().to_string();
        assert!(err.starts_with("blocked by spending policy:"), "{err}");
        assert!(p.list().unwrap().is_empty());
    }

    #[test]
    fn daily_total_comes_from_the_ledger_window() {
        let p = pending("gate-daily");
        let records = vec![
            spent(140, NOW - 100, Status::Broadcast),
            spent(500, NOW - ledger::DAY_SECS - 1, Status::Broadcast), // outside the window
            spent(500, NOW - 50, Status::Failed),                      // failed doesn't count
        ];
        assert_eq!(authorize(Some(&policy()), &records, &p, &req(10, Origin::Send), W, None, false, NOW).unwrap(), Authorized::Proceed);
        assert!(authorize(Some(&policy()), &records, &p, &req(11, Origin::Send), W, None, false, NOW).is_err());
    }

    #[test]
    fn unresolved_payments_count_toward_the_daily_cap() {
        let p = pending("gate-unresolved");
        let records = vec![spent(145, NOW - 100, Status::Unresolved)];
        assert!(authorize(Some(&policy()), &records, &p, &req(6, Origin::Send), W, None, false, NOW).is_err());
    }

    #[test]
    fn approved_replay_proceeds_but_caps_still_apply() {
        let p = pending("gate-approved");
        assert_eq!(authorize(Some(&policy()), &[], &p, &req(20, Origin::Send), W, None, true, NOW).unwrap(), Authorized::Proceed);
        assert!(authorize(Some(&policy()), &[], &p, &req(101, Origin::Send), W, None, true, NOW).is_err());
        assert!(p.list().unwrap().is_empty());
    }

    #[test]
    fn retries_for_the_same_task_reuse_the_open_entry() {
        let p = pending("gate-dedupe");
        let task = Origin::TaskApprove { task_id: 9, worker: "otter".into() };
        let Authorized::Pending(first) = authorize(Some(&policy()), &[], &p, &req(20, task.clone()), W, None, false, NOW).unwrap() else {
            panic!()
        };
        let Authorized::Pending(second) = authorize(Some(&policy()), &[], &p, &req(20, task), W, None, false, NOW + 60).unwrap() else {
            panic!()
        };
        assert_eq!(first.id, second.id);
        assert_eq!(p.list().unwrap().len(), 1);
    }

    #[test]
    fn retry_with_a_changed_amount_supersedes_the_open_entry() {
        let p = pending("gate-supersede");
        let task = Origin::TaskApprove { task_id: 9, worker: "otter".into() };
        let Authorized::Pending(first) = authorize(Some(&policy()), &[], &p, &req(20, task.clone()), W, None, false, NOW).unwrap() else {
            panic!()
        };
        let Authorized::Pending(second) = authorize(Some(&policy()), &[], &p, &req(30, task), W, None, false, NOW + 60).unwrap() else {
            panic!()
        };
        assert_ne!(first.id, second.id);
        assert_eq!(second.request.amount_sat, 30 * PIV);
        let statuses: Vec<PendingStatus> = p.list().unwrap().iter().map(|e| e.status).collect();
        assert_eq!(statuses, [PendingStatus::Pending, PendingStatus::Superseded]);
    }

    #[test]
    fn pending_entry_keeps_the_request_time_price_for_display() {
        let p = pending("gate-price");
        let price = Price { usd_e8: 6_888_275, fetched_at: NOW, source_updated_at: NOW, source: "oracle".into() };
        let Authorized::Pending(e) = authorize(Some(&policy()), &[], &p, &req(20, Origin::Send), W, Some(&price), false, NOW).unwrap() else {
            panic!()
        };
        assert_eq!(e.request_price, Some(price));
    }

    #[test]
    fn plain_send_retries_each_get_an_entry() {
        let p = pending("gate-plain");
        authorize(Some(&policy()), &[], &p, &req(20, Origin::Send), W, None, false, NOW).unwrap();
        authorize(Some(&policy()), &[], &p, &req(20, Origin::Send), W, None, false, NOW + 1).unwrap();
        assert_eq!(p.list().unwrap().len(), 2);
    }

    #[test]
    fn denied_entry_doesnt_block_a_new_request() {
        let p = pending("gate-after-deny");
        let task = Origin::TaskApprove { task_id: 9, worker: "otter".into() };
        let Authorized::Pending(first) = authorize(Some(&policy()), &[], &p, &req(20, task.clone()), W, None, false, NOW).unwrap() else {
            panic!()
        };
        p.resolve(&first.id, PendingStatus::Denied, None, NOW + 1).unwrap();
        let Authorized::Pending(second) = authorize(Some(&policy()), &[], &p, &req(20, task), W, None, false, NOW + 2).unwrap() else {
            panic!()
        };
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn dollar_limits_without_price_block() {
        let p = pending("gate-noprice");
        let pol = Policy { max_daily: Some(Limit::Usd(50 * PIV)), ..Default::default() };
        let err = authorize(Some(&pol), &[], &p, &req(1, Origin::Send), W, None, false, NOW).unwrap_err().to_string();
        assert!(err.contains("price"), "{err}");
    }

    #[test]
    fn checks_the_destination_against_the_allowlist() {
        let p = pending("gate-allow");
        let pol = Policy { allowlist: vec!["DDest".into()], ..Default::default() };
        assert_eq!(authorize(Some(&pol), &[], &p, &req(1, Origin::Send), W, None, false, NOW).unwrap(), Authorized::Proceed);
        let pol = Policy { allowlist: vec!["DOther".into()], ..Default::default() };
        assert!(matches!(authorize(Some(&pol), &[], &p, &req(1, Origin::Send), W, None, false, NOW).unwrap(), Authorized::Pending(_)));
    }
}
