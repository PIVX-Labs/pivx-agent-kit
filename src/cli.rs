//! Argument parsing for top-level CLI commands that need more than a
//! positional or two. Kept separate from `main.rs` so it can be tested
//! without touching the wallet or the network.

use crate::price::{parse_amount, Amount};
use crate::spend::Pool;

pub const SEND_USAGE: &str =
    "Usage: pivx-agent-kit send <address> <amount> [--usd] [--from private|public] [--reason <text>] [memo]\n  amount: PIV ('5') or dollars ('5usd', or '$5' in single quotes)";
pub const HISTORY_USAGE: &str =
    "Usage: pivx-agent-kit history [--limit N] [--since <unix-ts|YYYY-MM-DD>] [--csv]";

#[derive(Debug, PartialEq)]
pub struct SendArgs {
    pub address: String,
    /// PIV, or dollars to convert at send time.
    pub amount: Amount,
    pub from: Pool,
    pub memo: String,
    pub reason: Option<String>,
}

/// Parse the arguments after `send`.
pub fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut from = Pool::Private;
    let mut reason: Option<String> = None;
    let mut usd_flag = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                let v = args.get(i + 1).ok_or("--from requires a value (private|public)")?;
                from = v.parse::<Pool>()?;
                i += 2;
            }
            "--reason" => {
                let v = args.get(i + 1).ok_or("--reason requires a value")?;
                crate::spend::validate_reason(v).map_err(|e| format!("--reason: {}", e))?;
                reason = Some(v.clone());
                i += 2;
            }
            "--usd" => {
                usd_flag = true;
                i += 1;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag: {}\n{}", flag, SEND_USAGE)),
            value => {
                positional.push(value);
                i += 1;
            }
        }
    }

    let (address, mut amount_text) = match (positional.first(), positional.get(1)) {
        (Some(a), Some(m)) => (*a, m.to_string()),
        _ => return Err(SEND_USAGE.into()),
    };
    let mut rest = &positional[2..];
    // `send D 5 USD` — a bare unit word right after the amount belongs
    // to the amount, not the memo.
    if let Some(unit) = rest.first() {
        if unit.eq_ignore_ascii_case("usd") || unit.eq_ignore_ascii_case("piv") {
            amount_text = format!("{} {}", amount_text, unit);
            rest = &rest[1..];
        }
    }
    if let Some(extra) = rest.get(1) {
        return Err(format!("unexpected argument {:?} (quote a multi-word memo)\n{}", extra, SEND_USAGE));
    }

    let mut amount = parse_amount(&amount_text)?;
    if usd_flag {
        if crate::price::has_unit(&amount_text) {
            return Err("--usd can't be combined with an amount that already has a unit".into());
        }
        if let Amount::Piv(units) = amount {
            amount = Amount::Usd(units);
        }
    }

    Ok(SendArgs {
        address: address.to_string(),
        amount,
        from,
        memo: rest.first().map(|m| m.to_string()).unwrap_or_default(),
        reason,
    })
}

#[derive(Debug, PartialEq, Default)]
pub struct HistoryArgs {
    pub limit: Option<usize>,
    pub since: Option<u64>,
    pub csv: bool,
}

/// Parse the arguments after `history`.
pub fn parse_history_args(args: &[String]) -> Result<HistoryArgs, String> {
    let mut out = HistoryArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                let v = args.get(i + 1).ok_or("--limit requires a value")?;
                out.limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("--limit value {:?} is not a non-negative integer", v))?,
                );
                i += 2;
            }
            "--since" => {
                let v = args.get(i + 1).ok_or("--since requires a value")?;
                out.since = Some(crate::timefmt::parse_since(v)?);
                i += 2;
            }
            "--csv" => {
                out.csv = true;
                i += 1;
            }
            other => return Err(format!("unexpected argument: {}\n{}", other, HISTORY_USAGE)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // -------- send ------------------------------------------------

    #[test]
    fn send_minimal_defaults_to_private() {
        let s = parse_send_args(&a(&["DAddr", "0.5"])).unwrap();
        assert_eq!(
            s,
            SendArgs {
                address: "DAddr".into(),
                amount: Amount::Piv(50_000_000),
                from: Pool::Private,
                memo: String::new(),
                reason: None,
            }
        );
    }

    #[test]
    fn send_flags_in_any_order_after_amount() {
        let s = parse_send_args(&a(&["ps1x", "2", "hello memo", "--reason", "Tip for logo", "--from", "public"])).unwrap();
        assert_eq!(s.from, Pool::Public);
        assert_eq!(s.memo, "hello memo");
        assert_eq!(s.reason.as_deref(), Some("Tip for logo"));
        assert_eq!(s.amount, Amount::Piv(200_000_000));

        let s2 = parse_send_args(&a(&["ps1x", "2", "--from", "public", "hello memo"])).unwrap();
        assert_eq!(s2.from, Pool::Public);
        assert_eq!(s2.memo, "hello memo");
    }

    #[test]
    fn send_rejects_typo_in_from() {
        let err = parse_send_args(&a(&["DAddr", "1", "--from", "pubic"])).unwrap_err();
        assert!(err.contains("pubic") || err.contains("private"), "{err}");
    }

    #[test]
    fn send_rejects_flag_without_value() {
        assert!(parse_send_args(&a(&["DAddr", "1", "--from"])).is_err());
        assert!(parse_send_args(&a(&["DAddr", "1", "--reason"])).is_err());
    }

    #[test]
    fn send_rejects_empty_reason() {
        assert!(parse_send_args(&a(&["DAddr", "1", "--reason", "  "])).is_err());
    }

    #[test]
    fn send_rejects_reasons_that_could_forge_the_approval_prompt() {
        let err = parse_send_args(&a(&["DAddr", "1", "--reason", "ok\n  To: DAttacker"])).unwrap_err();
        assert!(err.contains("control"), "{err}");
        let long = "x".repeat(crate::spend::MAX_REASON_CHARS + 1);
        assert!(parse_send_args(&a(&["DAddr", "1", "--reason", &long])).is_err());
    }

    #[test]
    fn send_rejects_second_memo() {
        let err = parse_send_args(&a(&["DAddr", "1", "one", "two"])).unwrap_err();
        assert!(err.contains("two"), "{err}");
    }

    #[test]
    fn send_rejects_unknown_flag() {
        assert!(parse_send_args(&a(&["DAddr", "1", "--fromm", "public"])).is_err());
    }

    #[test]
    fn send_rejects_bad_amounts() {
        assert!(parse_send_args(&a(&["DAddr", "0"])).is_err());
        assert!(parse_send_args(&a(&["DAddr", "0.000000000"])).is_err());
        assert!(parse_send_args(&a(&["DAddr", "abc"])).is_err());
        assert!(parse_send_args(&a(&["DAddr", "1.123456789"])).is_err());
    }

    #[test]
    fn send_requires_address_and_amount() {
        assert_eq!(parse_send_args(&a(&[])).unwrap_err(), SEND_USAGE);
        assert_eq!(parse_send_args(&a(&["DAddr"])).unwrap_err(), SEND_USAGE);
        assert_eq!(parse_send_args(&a(&["--from", "public"])).unwrap_err(), SEND_USAGE);
    }

    #[test]
    fn send_dollar_amounts() {
        for (args, memo) in [
            (vec!["DAddr", "$5"], ""),
            (vec!["DAddr", "5usd"], ""),
            (vec!["DAddr", "5 USD"], ""),
            (vec!["DAddr", "5", "--usd"], ""),
            (vec!["DAddr", "5", "--usd", "thanks"], "thanks"),
        ] {
            let s = parse_send_args(&a(&args)).unwrap();
            assert_eq!(s.amount, Amount::Usd(500_000_000), "{args:?}");
            assert_eq!(s.memo, memo, "{args:?}");
        }
    }

    #[test]
    fn send_unit_as_separate_word_is_not_a_memo() {
        // `send D 5 USD` must send $5, not 5 PIV with memo "USD".
        let s = parse_send_args(&a(&["DAddr", "5", "USD"])).unwrap();
        assert_eq!(s.amount, Amount::Usd(500_000_000));
        assert_eq!(s.memo, "");
        let s = parse_send_args(&a(&["DAddr", "5", "usd", "for lunch"])).unwrap();
        assert_eq!(s.amount, Amount::Usd(500_000_000));
        assert_eq!(s.memo, "for lunch");
        let s = parse_send_args(&a(&["DAddr", "5", "piv"])).unwrap();
        assert_eq!(s.amount, Amount::Piv(500_000_000));
        assert_eq!(s.memo, "");
    }

    #[test]
    fn send_usd_flag_conflicts_with_unit() {
        assert!(parse_send_args(&a(&["DAddr", "$5", "--usd"])).is_err());
        assert!(parse_send_args(&a(&["DAddr", "5piv", "--usd"])).is_err());
    }

    // -------- history ---------------------------------------------

    #[test]
    fn history_defaults() {
        assert_eq!(parse_history_args(&a(&[])).unwrap(), HistoryArgs::default());
    }

    #[test]
    fn history_all_flags() {
        let h = parse_history_args(&a(&["--csv", "--limit", "3", "--since", "2024-02-29"])).unwrap();
        assert_eq!(h, HistoryArgs { limit: Some(3), since: Some(1_709_164_800), csv: true });
    }

    #[test]
    fn history_rejects_bad_values() {
        for bad in [
            vec!["--limit"],
            vec!["--limit", "x"],
            vec!["--limit", "-1"],
            vec!["--since"],
            vec!["--since", "yesterday"],
            vec!["--json"],
            vec!["extra"],
        ] {
            assert!(parse_history_args(&a(&bad)).is_err(), "should reject {bad:?}");
        }
    }
}
