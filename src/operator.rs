//! Operator-only commands: `policy set|show|clear` and
//! `pending list|approve|deny`.
//!
//! Anything that loosens control — changing the policy, approving or
//! denying a held payment — requires an interactive terminal and an
//! explicit "y". Agent shell tools run without a TTY, so an agent
//! calling these gets refused. None of them are exposed over MCP.

use serde_json::{json, Value};
use std::error::Error;

use crate::pending::{PendingEntry, PendingStatus, PendingStore};
use crate::policy::{self, Limit, Policy, PolicyStore};

pub const NOT_INTERACTIVE: &str =
    "this command must be run by a human in an interactive terminal (it changes what the agent may spend)";

/// The terminal the operator is typing at. Real impl checks stdin and
/// stderr (where the prompt goes) are TTYs and reads a line; tests
/// script it.
pub trait Terminal {
    fn is_interactive(&self) -> bool;
    /// Show `prompt` and return true only on an explicit yes.
    fn confirm(&self, prompt: &str) -> bool;
}

pub struct RealTerminal;

impl Terminal for RealTerminal {
    fn is_interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn confirm(&self, prompt: &str) -> bool {
        use std::io::{BufRead, Write};
        eprint!("{}\n\nApply? [y/N] ", prompt);
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
    }
}

/// Performs a held payment's original action with the approval applied.
pub type Replay<'a> = dyn FnMut(&PendingEntry) -> Result<Value, Box<dyn Error>> + 'a;

pub const POLICY_SET_USAGE: &str = "Usage: pivx-agent-kit policy set [--max-tx L] [--daily L] [--approve-above L] \
[--allow <addr>]... [--clear-allowlist]\n  L is PIV ('25') or dollars ('$25'); 'none' removes that limit.";

/// Apply `policy set` flags on top of `current` (flags not given keep
/// their current value). Validates the result.
pub fn apply_set_args(current: Option<&Policy>, args: &[String], now: u64) -> Result<Policy, String> {
    if args.is_empty() {
        return Err(POLICY_SET_USAGE.into());
    }
    let mut p = current.cloned().unwrap_or_default();
    let limit = |flag: &str, v: Option<&String>| -> Result<Option<Limit>, String> {
        let v = v.ok_or_else(|| format!("{} requires a value\n{}", flag, POLICY_SET_USAGE))?;
        if v.eq_ignore_ascii_case("none") {
            Ok(None)
        } else {
            policy::parse_limit(v).map(Some)
        }
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-tx" => {
                p.max_per_tx = limit("--max-tx", args.get(i + 1))?;
                i += 2;
            }
            "--daily" => {
                p.max_daily = limit("--daily", args.get(i + 1))?;
                i += 2;
            }
            "--approve-above" => {
                p.approve_above = limit("--approve-above", args.get(i + 1))?;
                i += 2;
            }
            "--allow" => {
                let addr = args.get(i + 1).ok_or("--allow requires an address")?;
                if !p.allowlist.contains(addr) {
                    p.allowlist.push(addr.clone());
                }
                i += 2;
            }
            "--clear-allowlist" => {
                p.allowlist.clear();
                i += 1;
            }
            other => return Err(format!("unknown argument: {}\n{}", other, POLICY_SET_USAGE)),
        }
    }
    policy::validate(&p)?;
    p.updated_at = now;
    Ok(p)
}

/// Human-readable summary of a policy for confirmation prompts and
/// `policy show`.
pub fn describe_policy(p: &Policy, price_e8: Option<u64>) -> String {
    let show = |l: Option<Limit>| l.map(|l| policy::describe_limit(l, price_e8)).unwrap_or_else(|| "no limit".into());
    format!(
        "  Max per payment:  {}\n  Max per 24h:      {}\n  Ask me above:     {}\n  Allowlist:        {}",
        show(p.max_per_tx),
        show(p.max_daily),
        p.approve_above.map(|l| policy::describe_limit(l, price_e8)).unwrap_or_else(|| "never".into()),
        if p.allowlist.is_empty() { "none".to_string() } else { p.allowlist.join(", ") }
    )
}

fn require_terminal(term: &dyn Terminal) -> Result<(), Box<dyn Error>> {
    if term.is_interactive() {
        Ok(())
    } else {
        Err(NOT_INTERACTIVE.into())
    }
}

/// `policy set`: needs a terminal and a "y".
pub fn policy_set(
    store: &PolicyStore,
    term: &dyn Terminal,
    args: &[String],
    price_e8: Option<u64>,
    now: u64,
) -> Result<Value, Box<dyn Error>> {
    require_terminal(term)?;
    // Merge onto the current policy only if it verifies. A tampered or
    // corrupt file must not have its values re-signed: start over.
    let (current, notice) = match store.load() {
        Ok(p) => (p, None),
        Err(e) => (
            None,
            Some(format!(
                "The existing policy file failed verification ({}). Starting from scratch: only the limits given now will apply.\n\n",
                e
            )),
        ),
    };
    let next = apply_set_args(current.as_ref(), args, now)?;
    let prompt = format!(
        "{}New spending policy:\n{}",
        notice.unwrap_or_default(),
        describe_policy(&next, price_e8)
    );
    if !term.confirm(&prompt) {
        return Err("cancelled; policy unchanged".into());
    }
    store.save(&next)?;
    Ok(json!({ "status": "policy_updated", "policy": next }))
}

/// `policy clear`: needs a terminal and a "y".
pub fn policy_clear(store: &PolicyStore, term: &dyn Terminal) -> Result<Value, Box<dyn Error>> {
    require_terminal(term)?;
    if !term.confirm("Remove the spending policy? The agent will be able to spend without limits.") {
        return Err("cancelled; policy unchanged".into());
    }
    store.clear()?;
    Ok(json!({ "status": "policy_cleared" }))
}

/// `pending approve <id>`: needs a terminal and a "y". `replay` performs
/// the original action (send / task approve / card payment) with the
/// approval applied and returns its result. The entry is marked
/// approved with the txid, or failed with the error.
pub fn pending_approve(
    store: &PendingStore,
    term: &dyn Terminal,
    id: &str,
    wallet: &str,
    now: u64,
    describe: &dyn Fn(&PendingEntry) -> String,
    replay: &mut Replay<'_>,
) -> Result<Value, Box<dyn Error>> {
    require_terminal(term)?;
    let entry = store.get_actionable(id, wallet, now)?;
    if !term.confirm(&describe(&entry)) {
        return Err("cancelled; payment still pending".into());
    }
    let done = crate::core::now_secs().max(now);
    match replay(&entry) {
        Ok(mut out) => {
            let status = out.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let txid = out.get("txid").and_then(|t| t.as_str()).unwrap_or("");
            let note = Some(format!("{} txid {}", status, txid));
            // Settle only what this process claimed (or, when the replay
            // reused an earlier payment without sending, the still-pending
            // entry). Never overwrite another run's outcome, and never let
            // a bookkeeping failure turn a payment into an error.
            let recorded = store
                .resolve_from(id, PendingStatus::Sending, PendingStatus::Approved, note.clone(), done)
                .and_then(|ok| if ok { Ok(true) } else { store.resolve_from(id, PendingStatus::Pending, PendingStatus::Approved, note, done) });
            match recorded {
                Ok(true) => {}
                Ok(false) => {
                    out["warning"] = json!(format!("payment {} went out (txid {}) but its entry was changed by another command meanwhile", id, txid));
                }
                Err(e) => {
                    out["warning"] = json!(format!(
                        "payment {} went out (txid {}) but recording it in pending.json failed: {}",
                        id, txid, e
                    ));
                }
            }
            Ok(out)
        }
        // Not ours to record: another approval took it, or it expired.
        Err(e) if e.downcast_ref::<crate::pending::NotActionable>().is_some() => Err(e),
        Err(e) => {
            // Only an entry this process claimed is marked failed; if the
            // replay failed before claiming it, it stays pending.
            let _ = store.resolve_from(id, PendingStatus::Sending, PendingStatus::Failed, Some(e.to_string()), done);
            Err(e)
        }
    }
}

/// `pending deny <id>`: needs a terminal and a "y".
pub fn pending_deny(
    store: &PendingStore,
    term: &dyn Terminal,
    id: &str,
    wallet: &str,
    now: u64,
) -> Result<Value, Box<dyn Error>> {
    require_terminal(term)?;
    let entry = store.get_actionable(id, wallet, now)?;
    if !term.confirm(&format!("Deny pending payment {}?", entry.id)) {
        return Err("cancelled; payment still pending".into());
    }
    let done = crate::core::now_secs().max(now);
    if !store.resolve_from(id, PendingStatus::Pending, PendingStatus::Denied, None, done)? {
        return Err(format!("pending payment {} changed while you were deciding (approved or sent elsewhere); not denied", id).into());
    }
    Ok(json!({ "status": "denied", "pending_id": id }))
}

/// `pivx_policy` / `policy show`: the policy with limits in both units,
/// what went out in the last 24h, and how much is left today.
pub fn policy_status(
    policy: Option<&Policy>,
    records: &[crate::ledger::SendRecord],
    price_e8: Option<u64>,
    now: u64,
) -> Value {
    use crate::price;
    let Some(p) = policy else {
        return json!({
            "policy_set": false,
            "note": "no spending limits are set; the operator can add them with `pivx-agent-kit policy set`",
        });
    };
    let spent = crate::ledger::spent_since(records, now.saturating_sub(crate::ledger::DAY_SECS));
    let describe = |l: Option<Limit>| l.map(|l| policy::describe_limit(l, price_e8));
    let mut v = json!({
        "policy_set": true,
        "max_per_tx": describe(p.max_per_tx),
        "max_daily": describe(p.max_daily),
        "approve_above": describe(p.approve_above),
        "allowlist": p.allowlist,
        "spent_24h_sat": spent,
        "spent_24h": spent as f64 / 1e8,
    });
    if let Some(pr) = price_e8 {
        v["spent_24h_usd"] = json!(price::format_usd(price::sat_to_usd_e8(spent, pr)));
    }
    match p.max_daily {
        Some(Limit::Piv(cap)) => {
            v["remaining_today_sat"] = json!(cap.saturating_sub(spent));
        }
        Some(Limit::Usd(cap)) => {
            v["remaining_today_usd"] = match price_e8 {
                Some(pr) => json!(price::format_usd(cap.saturating_sub(price::sat_to_usd_e8(spent, pr)))),
                None => Value::Null,
            };
        }
        None => {}
    }
    v
}

/// Make agent- or platform-supplied text safe to print: control
/// characters (newlines, escape sequences, C1 controls) are shown
/// escaped so they can't forge or hide prompt lines.
pub fn for_terminal(s: &str) -> String {
    s.chars()
        .map(|c| if crate::spend::is_unsafe_char(c) { c.escape_unicode().to_string() } else { c.to_string() })
        .collect()
}

/// The `pending approve` confirmation. Kit-derived facts (destination,
/// amount, balance, values) come first; agent text after, escaped.
pub fn describe_pending(
    e: &PendingEntry,
    price_now: Option<&crate::price::Price>,
    duplicates: &[PendingEntry],
) -> String {
    use crate::price;
    use crate::spend::Origin;
    let req = &e.request;
    let usd = |p: Option<&price::Price>| {
        p.map(|p| format!("${}", price::format_usd(price::sat_to_usd_e8(req.amount_sat, p.usd_e8))))
            .unwrap_or_else(|| "unknown".into())
    };
    let trigger = match &req.origin {
        Origin::Send => "direct payment".to_string(),
        Origin::TaskApprove { task_id, worker } => format!("bounty for task #{} to {}", task_id, for_terminal(worker)),
        Origin::CardsPay { order_id } => format!("PIVCards order {}", for_terminal(order_id)),
    };
    // Kit-derived facts first, so nothing agent-written can push them
    // off screen or imitate them above.
    let mut out = format!(
        "Pending payment {}\n  To:       {}\n  Amount:   {} PIV from {}\n  Value:    {} when requested · {} now\n  Type:     {}\n  Why held: {}\n  Expires:  {}",
        e.id,
        for_terminal(&req.address),
        pivx_wallet_kit::amount::format_sat_to_piv(req.amount_sat),
        req.from,
        usd(e.request_price.as_ref()),
        usd(price_now),
        trigger,
        for_terminal(&e.reasons.join("; ")),
        crate::timefmt::format_rfc3339(e.expires_at),
    );
    out.push_str(&format!(
        "\n  Reason (from the agent): {}",
        req.reason.as_deref().map(for_terminal).unwrap_or_else(|| "none".into())
    ));
    if !req.memo.is_empty() {
        out.push_str(&format!("\n  Memo (sent with the payment): {}", for_terminal(&req.memo)));
    }
    if !duplicates.is_empty() {
        let ids: Vec<&str> = duplicates.iter().map(|d| d.id.as_str()).collect();
        out.push_str(&format!(
            "\n\n  Warning: {} other pending payment(s) would pay the same destination the same amount: {}. \
             Approving more than one pays more than once.",
            ids.len(),
            ids.join(", ")
        ));
    }
    out.push_str("\n\nApprove and send?");
    out
}

/// Seed export hands over the keys, which defeats any spending policy:
/// with a policy set it needs a human at a terminal.
pub fn export_allowed(policy_set: bool, interactive: bool) -> Result<(), String> {
    if policy_set && !interactive {
        return Err("seed export is disabled while a spending policy is set: the seed would bypass every limit. \
                    The operator can export from an interactive terminal."
            .into());
    }
    Ok(())
}

/// `pending list` / `pivx_pending_list`: this wallet's entries, newest
/// first, with an effective status (pending entries past their expiry
/// show as `expired`).
pub fn pending_list(store: &PendingStore, wallet: &str, now: u64) -> Result<Value, Box<dyn Error>> {
    let items: Vec<Value> = store
        .list()?
        .into_iter()
        .filter(|e| e.wallet == wallet)
        .map(|e| {
            let status = if e.status == PendingStatus::Pending && e.is_expired(now) {
                json!("expired")
            } else {
                serde_json::to_value(e.status).unwrap_or(Value::Null)
            };
            json!({
                "id": e.id,
                "status": status,
                "created": crate::timefmt::format_rfc3339(e.created_at),
                "expires": crate::timefmt::format_rfc3339(e.expires_at),
                "to": e.request.address,
                "amount": e.request.amount_sat as f64 / 1e8,
                "amount_sat": e.request.amount_sat,
                "from": e.request.from,
                "reason": e.request.reason,
                "origin": e.request.origin,
                "why_held": e.reasons,
                "note": e.note,
            })
        })
        .collect();
    Ok(json!({ "count": items.len(), "pending": items }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::temp_dir;
    use crate::pending::{new_entry, PENDING_FILE};
    use crate::policy::POLICY_FILE;
    use crate::spend::{Origin, Pool, SendRequest};
    use std::cell::{Cell, RefCell};

    const W: &str = "DWallet";
    const NOW: u64 = 1_790_000_000;
    const PIV: u64 = 100_000_000;

    struct Script {
        interactive: bool,
        answer: bool,
        prompts: RefCell<Vec<String>>,
    }

    impl Script {
        fn human(answer: bool) -> Self {
            Self { interactive: true, answer, prompts: RefCell::new(vec![]) }
        }
        fn agent() -> Self {
            Self { interactive: false, answer: true, prompts: RefCell::new(vec![]) }
        }
    }

    impl Terminal for Script {
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn confirm(&self, prompt: &str) -> bool {
            self.prompts.borrow_mut().push(prompt.to_string());
            self.answer
        }
    }

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pstore(tag: &str) -> PolicyStore {
        PolicyStore::at(temp_dir(tag).join(POLICY_FILE), [1u8; 32])
    }

    fn qstore(tag: &str) -> PendingStore {
        PendingStore::at(temp_dir(tag).join(PENDING_FILE))
    }

    fn entry(q: &PendingStore) -> PendingEntry {
        let req = SendRequest {
            address: "DDest".into(),
            amount_sat: 20 * PIV,
            memo: String::new(),
            from: Pool::Public,
            reason: Some("Steam card".into()),
            origin: Origin::Send,
            price: None,
        };
        let e = new_entry(&req, W, vec!["amount 20.00 PIV exceeds approval threshold 10.00 PIV".into()], None, NOW);
        q.add(&e).unwrap();
        e
    }

    // -------- apply_set_args ---------------------------------------

    #[test]
    fn set_args_build_a_policy() {
        let p = apply_set_args(None, &a(&["--max-tx", "$25", "--daily", "$50", "--approve-above", "$10", "--allow", "DFriend1"]), NOW).unwrap();
        assert_eq!(p.max_per_tx, Some(Limit::Usd(25 * PIV)));
        assert_eq!(p.max_daily, Some(Limit::Usd(50 * PIV)));
        assert_eq!(p.approve_above, Some(Limit::Usd(10 * PIV)));
        assert_eq!(p.allowlist, vec!["DFriend1".to_string()]);
        assert_eq!(p.updated_at, NOW);
    }

    #[test]
    fn set_args_merge_onto_current() {
        let current = Policy { max_per_tx: Some(Limit::Piv(100 * PIV)), allowlist: vec!["DFriend1".into()], ..Default::default() };
        let p = apply_set_args(Some(&current), &a(&["--daily", "500", "--allow", "DFriend2"]), NOW).unwrap();
        assert_eq!(p.max_per_tx, Some(Limit::Piv(100 * PIV)));
        assert_eq!(p.max_daily, Some(Limit::Piv(500 * PIV)));
        assert_eq!(p.allowlist, vec!["DFriend1".to_string(), "DFriend2".to_string()]);
        let p = apply_set_args(Some(&current), &a(&["--max-tx", "none", "--clear-allowlist"]), NOW).unwrap();
        assert_eq!(p.max_per_tx, None);
        assert!(p.allowlist.is_empty());
    }

    #[test]
    fn set_args_reject_bad_input() {
        for bad in [
            vec![],
            vec!["--max-tx"],
            vec!["--max-tx", "lots"],
            vec!["--bogus", "1"],
            vec!["--max-tx", "$5", "--approve-above", "$5"],
            vec!["--allow", "not an address"],
        ] {
            assert!(apply_set_args(None, &a(&bad), NOW).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn describe_mentions_every_limit() {
        let p = apply_set_args(None, &a(&["--max-tx", "$25", "--daily", "$50", "--approve-above", "$10"]), NOW).unwrap();
        let d = describe_policy(&p, Some(6_888_275));
        assert!(d.contains("$25.00") && d.contains("$50.00") && d.contains("$10.00"), "{d}");
        assert!(d.contains("Allowlist") && d.contains("none"), "{d}");
    }

    // -------- terminal safety --------------------------------------

    #[test]
    fn for_terminal_escapes_control_characters() {
        assert_eq!(for_terminal("plain text ☕"), "plain text ☕");
        assert_eq!(for_terminal("a\nb"), "a\\u{a}b");
        assert_eq!(for_terminal("hide\x1b[8m"), "hide\\u{1b}[8m");
        assert_eq!(for_terminal("c1\u{9b}x"), "c1\\u{9b}x");
        assert_eq!(for_terminal("tab\t"), "tab\\u{9}");
        assert!(!for_terminal("\r\x07\x7f").chars().any(|c| c.is_control()));
        assert_eq!(for_terminal("a\u{202E}b"), "a\\u{202e}b");
        assert_eq!(for_terminal("z\u{200B}w"), "z\\u{200b}w");
    }

    fn describe_entry(reason: &str) -> PendingEntry {
        let req = SendRequest {
            address: "DRealDestination".into(),
            amount_sat: 900 * PIV,
            memo: "secret\nmemo".into(),
            from: Pool::Private,
            reason: Some(reason.into()),
            origin: Origin::Send,
            price: None,
        };
        new_entry(&req, W, vec!["destination DRealDestination isn't on the allowlist".into()], None, NOW)
    }

    #[test]
    fn prompt_shows_authoritative_fields_before_agent_text() {
        let e = describe_entry("fine");
        let d = describe_pending(&e, None, &[]);
        let to = d.find("To:").unwrap();
        let amount = d.find("Amount:").unwrap();
        let reason = d.find("Reason").unwrap();
        assert!(to < reason && amount < reason, "{d}");
        assert!(d.contains("DRealDestination") && d.contains("900.00000000 PIV"), "{d}");
        assert!(d.contains("Memo (sent with the payment)") && d.contains("secret\\u{a}memo"), "memo shown, escaped: {d}");
    }

    #[test]
    fn prompt_cannot_be_forged_by_the_reason() {
        let evil = "ok\n  To:       DAttacker\n  Amount:   1 PIV\n\nApprove and send?\x1b[8m";
        let e = describe_entry(evil);
        let d = describe_pending(&e, None, &[]);
        assert!(!d.contains('\x1b'), "escape must not reach the terminal: {d:?}");
        // The fake lines stay inside the one escaped reason line.
        let starts = |label: &str| d.lines().filter(|l| l.trim_start().starts_with(label)).count();
        assert_eq!(starts("To:"), 1, "{d}");
        assert_eq!(starts("Amount:"), 1, "{d}");
        assert_eq!(starts("Approve and send?"), 1, "{d}");
    }

    #[test]
    fn prompt_warns_about_duplicates() {
        let e = describe_entry("fine");
        let mut dup = e.clone();
        dup.id = "p_bbbbbbbbbbbb".into();
        let d = describe_pending(&e, None, &[dup]);
        assert!(d.contains("p_bbbbbbbbbbbb") && d.to_lowercase().contains("same destination"), "{d}");
    }

    #[test]
    fn prompt_shows_request_time_value_when_known() {
        let mut e = describe_entry("fine");
        e.request_price = Some(crate::price::Price { usd_e8: 6_888_275, fetched_at: NOW, source_updated_at: NOW, source: "oracle".into() });
        let now = crate::price::Price { usd_e8: 10_000_000, fetched_at: NOW, source_updated_at: NOW, source: "oracle".into() };
        let d = describe_pending(&e, Some(&now), &[]);
        assert!(d.contains("$61.99") && d.contains("$90.00"), "{d}");
    }

    #[test]
    fn export_gate() {
        assert!(export_allowed(false, false).is_ok(), "no policy: unchanged behaviour");
        assert!(export_allowed(true, true).is_ok());
        assert!(export_allowed(true, false).unwrap_err().contains("spending policy"));
    }

    #[test]
    fn set_on_tampered_policy_starts_from_scratch() {
        let s = pstore("op-tampered-fresh");
        policy_set(&s, &Script::human(true), &a(&["--daily", "100", "--allow", "DFriend1"]), None, NOW).unwrap();
        let file = s_path(&s);
        let text = std::fs::read_to_string(&file).unwrap().replace("DFriend1", "DAgentAddr");
        std::fs::write(&file, text).unwrap();
        let t = Script::human(true);
        policy_set(&s, &t, &a(&["--max-tx", "25"]), None, NOW).unwrap();
        let p = s.load().unwrap().unwrap();
        assert!(p.allowlist.is_empty(), "tampered values must not be re-signed: {p:?}");
        assert_eq!(p.max_daily, None);
        assert!(t.prompts.borrow()[0].contains("failed verification"), "{}", t.prompts.borrow()[0]);
    }

    #[test]
    fn set_works_on_a_corrupt_policy_file() {
        let s = pstore("op-corrupt");
        std::fs::write(s_path(&s), "garbage").unwrap();
        policy_set(&s, &Script::human(true), &a(&["--max-tx", "25"]), None, NOW).unwrap();
        assert!(s.load().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn approve_reports_success_even_if_recording_fails() {
        use std::os::unix::fs::PermissionsExt;
        let q = qstore("op-approve-record-fail");
        let e = entry(&q);
        let dir = q.path_for_tests().parent().unwrap().to_path_buf();
        let out = pending_approve(&q, &Script::human(true), &e.id, W, NOW + 10, &|_| String::new(), &mut |got| {
            q.consume(&got.id, W, NOW + 10).unwrap();
            // The pending file becomes unwritable after the payment went out.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            Ok(json!({"status": "sent", "txid": "ab".repeat(32)}))
        });
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = out.expect("a sent payment must not be reported as an error");
        assert_eq!(out["txid"], json!("ab".repeat(32)));
        assert!(out["warning"].as_str().unwrap().contains(&e.id));
    }

    // -------- policy set / clear -----------------------------------

    #[test]
    fn agent_without_terminal_cannot_set_policy() {
        let s = pstore("op-agent-set");
        let t = Script::agent();
        let err = policy_set(&s, &t, &a(&["--daily", "$5000"]), None, NOW).unwrap_err().to_string();
        assert_eq!(err, NOT_INTERACTIVE);
        assert!(t.prompts.borrow().is_empty());
        assert_eq!(s.load().unwrap(), None);
    }

    #[test]
    fn human_declining_changes_nothing() {
        let s = pstore("op-decline");
        let err = policy_set(&s, &Script::human(false), &a(&["--daily", "$50"]), None, NOW).unwrap_err().to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(s.load().unwrap(), None);
    }

    #[test]
    fn human_confirming_saves_signed_policy() {
        let s = pstore("op-confirm");
        let t = Script::human(true);
        let out = policy_set(&s, &t, &a(&["--daily", "$50"]), Some(6_888_275), NOW).unwrap();
        assert_eq!(out["status"], "policy_updated");
        assert_eq!(s.load().unwrap().unwrap().max_daily, Some(Limit::Usd(50 * PIV)));
        assert!(t.prompts.borrow()[0].contains("$50.00"));
    }

    #[test]
    fn set_rewrites_a_tampered_policy() {
        let s = pstore("op-resign");
        policy_set(&s, &Script::human(true), &a(&["--daily", "100"]), None, NOW).unwrap();
        let file = s_path(&s);
        let text = std::fs::read_to_string(&file).unwrap().replace("10000000000", "99900000000");
        std::fs::write(&file, text).unwrap();
        assert!(s.load().is_err());
        policy_set(&s, &Script::human(true), &a(&["--daily", "100"]), None, NOW).unwrap();
        assert!(s.load().unwrap().is_some());
    }

    fn s_path(s: &PolicyStore) -> std::path::PathBuf {
        s.path_for_tests()
    }

    #[test]
    fn clear_needs_terminal_and_yes() {
        let s = pstore("op-clear");
        policy_set(&s, &Script::human(true), &a(&["--daily", "100"]), None, NOW).unwrap();
        assert_eq!(policy_clear(&s, &Script::agent()).unwrap_err().to_string(), NOT_INTERACTIVE);
        assert!(policy_clear(&s, &Script::human(false)).is_err());
        assert!(s.load().unwrap().is_some());
        policy_clear(&s, &Script::human(true)).unwrap();
        assert_eq!(s.load().unwrap(), None);
    }

    // -------- policy status ----------------------------------------

    fn spent(sat: u64, ts: u64) -> crate::ledger::SendRecord {
        crate::ledger::SendRecord {
            intent: crate::ledger::SendIntent {
                id: format!("{ts}"),
                ts,
                wallet: W.into(),
                to: "DX".into(),
                amount_sat: sat,
                fee_sat: 0,
                from: Pool::Public,
                memo: String::new(),
                reason: None,
                origin: Origin::Send,
                txid: "ab".repeat(32),
                usd_price_e8: None,
            },
            status: crate::ledger::Status::Broadcast,
            error: None,
        }
    }

    #[test]
    fn status_without_policy() {
        let v = policy_status(None, &[], None, NOW);
        assert_eq!(v["policy_set"], false);
        assert!(v["note"].as_str().unwrap().contains("no spending limits"));
    }

    #[test]
    fn status_reports_remaining_today() {
        let p = Policy { max_per_tx: Some(Limit::Piv(100 * PIV)), max_daily: Some(Limit::Piv(150 * PIV)), approve_above: Some(Limit::Piv(10 * PIV)), ..Default::default() };
        let v = policy_status(Some(&p), &[spent(40 * PIV, NOW - 100), spent(500 * PIV, NOW - 90_000)], None, NOW);
        assert_eq!(v["policy_set"], true);
        assert_eq!(v["spent_24h_sat"], 40 * PIV);
        assert_eq!(v["remaining_today_sat"], 110 * PIV);
        assert_eq!(v["max_per_tx"], "100.00 PIV");
        assert_eq!(v["approve_above"], "10.00 PIV");
    }

    #[test]
    fn status_in_dollars_with_price() {
        let p = Policy { max_daily: Some(Limit::Usd(50 * PIV)), ..Default::default() };
        let v = policy_status(Some(&p), &[spent(100 * PIV, NOW - 100)], Some(6_888_275), NOW);
        assert_eq!(v["spent_24h_usd"], "6.89");
        assert_eq!(v["remaining_today_usd"], "43.11");
        let v = policy_status(Some(&p), &[], None, NOW);
        assert!(v["remaining_today_usd"].is_null(), "unknown without a price: {v}");
    }

    #[test]
    fn status_remaining_never_negative() {
        let p = Policy { max_daily: Some(Limit::Piv(10 * PIV)), ..Default::default() };
        let v = policy_status(Some(&p), &[spent(50 * PIV, NOW - 1)], None, NOW);
        assert_eq!(v["remaining_today_sat"], 0);
    }

    // -------- pending approve / deny / list ------------------------

    #[test]
    fn agent_cannot_approve() {
        let q = qstore("op-agent-approve");
        let e = entry(&q);
        let called = Cell::new(false);
        let err = pending_approve(&q, &Script::agent(), &e.id, W, NOW + 10, &|_| String::new(), &mut |_| {
            called.set(true);
            Ok(json!({}))
        })
        .unwrap_err()
        .to_string();
        assert_eq!(err, NOT_INTERACTIVE);
        assert!(!called.get());
        assert_eq!(q.list().unwrap()[0].status, PendingStatus::Pending);
    }

    #[test]
    fn approve_replays_and_records_txid() {
        let q = qstore("op-approve");
        let e = entry(&q);
        let t = Script::human(true);
        let out = pending_approve(&q, &t, &e.id, W, NOW + 10, &|e| format!("Pay {} PIV", e.request.amount_sat / PIV), &mut |got| {
            assert_eq!(got.id, e.id);
            q.consume(&got.id, W, NOW + 10).unwrap(); // as send_with does
            Ok(json!({"status": "sent", "txid": "ab".repeat(32)}))
        })
        .unwrap();
        assert_eq!(out["status"], "sent");
        assert!(t.prompts.borrow()[0].contains("Pay 20 PIV"));
        let got = &q.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved);
        assert!(got.note.as_deref().unwrap().contains(&"ab".repeat(32)));
    }

    #[test]
    fn approve_declined_does_nothing() {
        let q = qstore("op-approve-no");
        let e = entry(&q);
        let err = pending_approve(&q, &Script::human(false), &e.id, W, NOW + 10, &|_| String::new(), &mut |_| panic!("must not replay"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(q.list().unwrap()[0].status, PendingStatus::Pending);
    }

    #[test]
    fn approve_replay_failure_marks_failed() {
        let q = qstore("op-approve-fail");
        let e = entry(&q);
        let err = pending_approve(&q, &Script::human(true), &e.id, W, NOW + 10, &|_| String::new(), &mut |got| {
            q.consume(&got.id, W, NOW + 10).unwrap();
            Err("order expired".into())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("order expired"));
        let got = &q.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Failed);
        assert!(got.note.as_deref().unwrap().contains("order expired"));
    }

    #[test]
    fn losing_a_concurrent_approval_leaves_the_winners_record_alone() {
        // Another `pending approve` consumed and paid this entry while we
        // waited on the spend lock; our replay fails "not actionable".
        let q = qstore("op-approve-race");
        let e = entry(&q);
        let out = pending_approve(&q, &Script::human(true), &e.id, W, NOW + 10, &|_| String::new(), &mut |got| {
            q.resolve(&got.id, PendingStatus::Approved, Some("sent txid abc".into()), NOW + 11).unwrap();
            Err(Box::new(crate::pending::NotActionable(format!("pending payment {} was already approved", got.id))))
        });
        assert!(out.is_err());
        let got = &q.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved, "the winner's outcome must survive");
        assert_eq!(got.note.as_deref(), Some("sent txid abc"));
    }

    #[test]
    fn approve_rejects_expired_and_unknown() {
        let q = qstore("op-approve-expired");
        let e = entry(&q);
        let err = pending_approve(&q, &Script::human(true), &e.id, W, e.expires_at, &|_| String::new(), &mut |_| panic!())
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(pending_approve(&q, &Script::human(true), "p_nope", W, NOW, &|_| String::new(), &mut |_| panic!()).is_err());
    }

    /// Terminal that, while the operator reads the prompt, lets another
    /// command settle the entry.
    struct Interleave<'a> {
        q: &'a PendingStore,
        id: String,
    }

    impl Terminal for Interleave<'_> {
        fn is_interactive(&self) -> bool {
            true
        }
        fn confirm(&self, _prompt: &str) -> bool {
            self.q.resolve(&self.id, PendingStatus::Approved, Some("sent txid abc".into()), NOW + 1).unwrap();
            true
        }
    }

    #[test]
    fn deny_does_not_overwrite_an_entry_paid_meanwhile() {
        let q = qstore("op-deny-race");
        let e = entry(&q);
        let t = Interleave { q: &q, id: e.id.clone() };
        assert!(pending_deny(&q, &t, &e.id, W, NOW).is_err());
        let got = &q.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved);
        assert_eq!(got.note.as_deref(), Some("sent txid abc"));
    }

    #[test]
    fn failed_approval_does_not_overwrite_another_runs_record() {
        // Our replay fails before claiming (e.g. spend-lock timeout) while
        // another approval already paid: their record must stay.
        let q = qstore("op-approve-fail-other");
        let e = entry(&q);
        let out = pending_approve(&q, &Script::human(true), &e.id, W, NOW + 10, &|_| String::new(), &mut |got| {
            q.resolve(&got.id, PendingStatus::Approved, Some("sent txid abc".into()), NOW + 11).unwrap();
            Err("another payment from this wallet is still in progress".into())
        });
        assert!(out.is_err());
        let got = &q.list().unwrap()[0];
        assert_eq!(got.status, PendingStatus::Approved);
        assert_eq!(got.note.as_deref(), Some("sent txid abc"));
    }

    #[test]
    fn deny_needs_terminal_and_marks_denied() {
        let q = qstore("op-deny");
        let e = entry(&q);
        assert_eq!(pending_deny(&q, &Script::agent(), &e.id, W, NOW).unwrap_err().to_string(), NOT_INTERACTIVE);
        let out = pending_deny(&q, &Script::human(true), &e.id, W, NOW + 5).unwrap();
        assert_eq!(out["status"], "denied");
        assert_eq!(q.list().unwrap()[0].status, PendingStatus::Denied);
    }

    #[test]
    fn list_shows_effective_status_for_this_wallet() {
        let q = qstore("op-list");
        let e = entry(&q);
        let mut other = e.clone();
        other.id = "p_aaaaaaaaaaaa".into();
        other.wallet = "DOther".into();
        q.add(&other).unwrap();
        let v = pending_list(&q, W, NOW + 10).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["pending"][0]["id"], json!(e.id));
        assert_eq!(v["pending"][0]["status"], "pending");
        assert_eq!(v["pending"][0]["amount"], json!(20.0));
        let v = pending_list(&q, W, e.expires_at).unwrap();
        assert_eq!(v["pending"][0]["status"], "expired");
    }
}
