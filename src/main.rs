mod cards;
mod cli;
mod core;
mod gate;
mod keys;
mod ledger;
mod mcp;
mod network;
mod operator;
mod payment;
mod pending;
mod policy;
mod price;
mod prover;
mod shield;
mod spend;
mod sync;
mod task;
mod timefmt;
mod updater;
mod wallet;

use serde_json::json;
use std::env;
use std::process;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn help_text() -> String {
    format!("\
PIVX Agent Kit v{} – CLI toolbox for AI agents to interact with the PIVX blockchain

Usage: pivx-agent-kit <command> [args]

Commands:
  init                                          Create a new wallet (shield + transparent)
  import <mnemonic>                             Import wallet from seed phrase
  address                                       Show shield and transparent addresses
  balance                                       Sync and show private + public balances
  send <address> <amount> [--usd] [--from private|public] [--reason <text>] [memo]
                                                Send PIV (default: from private). Amount
                                                in PIV ('5') or dollars ('5usd', '$5' in
                                                single quotes, or 5 --usd). The reason
                                                is kept in the spend history.
  history [--limit N] [--since <ts|YYYY-MM-DD>] [--csv]
                                                Every payment this wallet sent, newest
                                                first, with reasons and 24h totals
  resync                                        Reset and re-sync from checkpoint
  sign-message <message>                        Sign a message with the transparent privkey
                                                (base64 sig compatible with PIVX Core verifymessage)
  export                                        Export wallet seed phrase for migration
  serve                                         Run as MCP server (for AI agent integration)
  task <subcommand>                             PIVX Tasks platform client.
                                                Read:    list, search, get, profile
                                                Worker:  signup, submit
                                                Creator: create, approve, reject, cancel
                                                Inbox:   notifications
                                                Run `task` with no args for full usage.
  cards <subcommand>                            PIVCards (cards.pivxla.bz) — buy gift
                                                cards (Amazon, Steam, Uber, …) with PIV.
                                                Discovery: regions, search, details
                                                Order:     order create|pay|check|cancel|list
                                                Run `cards` with no args for full usage.
  policy show                                   Spending limits, 24h spend, what's left today
  policy set [--max-tx L] [--daily L] [--approve-above L] [--allow <addr>]... [--clear-allowlist]
                                                Set limits (L = PIV '25' or dollars '25usd';
                                                'none' removes one). Operator only: needs a
                                                terminal and a 'y'.
  policy clear                                  Remove all limits (operator only)
  pending list                                  Payments held for approval
  pending approve <id> | pending deny <id>      Release or refuse a held payment (operator only)
  update                                        Update to the latest release", VERSION)
}

/// Process exit code for a successful command: 2 when a payment was
/// held for approval, so scripts can't mistake it for a sent one.
fn exit_code_for(output: &serde_json::Value) -> i32 {
    if output.get("status").and_then(|s| s.as_str()) == Some("pending_approval") {
        2
    } else {
        0
    }
}

fn policy_command(args: &[String]) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let store = policy::PolicyStore::open_default()?;
    match args.first().map(|s| s.as_str()) {
        Some("show") | None => core::policy_status(),
        Some("set") => {
            let price_e8 = price::for_display().map(|p| p.usd_e8);
            operator::policy_set(&store, &operator::RealTerminal, &args[1..], price_e8, core::now_secs())
        }
        Some("clear") => operator::policy_clear(&store, &operator::RealTerminal),
        Some(other) => Err(format!("unknown policy subcommand: {}\n{}", other, operator::POLICY_SET_USAGE).into()),
    }
}

fn pending_command(args: &[String]) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let usage = "Usage: pivx-agent-kit pending list | approve <id> | deny <id>";
    let store = pending::PendingStore::open_default();
    match (args.first().map(|s| s.as_str()), args.get(1)) {
        (Some("list") | None, _) => core::pending_list(),
        (Some("approve"), Some(id)) => {
            let wallet = core::wallet_address()?;
            let price_now = price::for_display();
            let describe = |e: &pending::PendingEntry| {
                let dupes = store.open_duplicates_of(e, core::now_secs()).unwrap_or_default();
                operator::describe_pending(e, price_now.as_ref(), &dupes)
            };
            operator::pending_approve(&store, &operator::RealTerminal, id, &wallet, core::now_secs(), &describe, &mut |e| {
                let req = &e.request;
                match &req.origin {
                    spend::Origin::Send => core::send_approved(&req.to_request(), &e.id),
                    spend::Origin::TaskApprove { task_id, worker } => {
                        task::commands::approve_approved(*task_id, worker, req.from, &e.id)
                    }
                    spend::Origin::CardsPay { order_id } => cards::commands::order_pay_approved(order_id, req.from, &e.id),
                }
            })
        }
        (Some("deny"), Some(id)) => {
            let wallet = core::wallet_address()?;
            operator::pending_deny(&store, &operator::RealTerminal, id, &wallet, core::now_secs())
        }
        _ => Err(usage.into()),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // MCP server mode — long-lived, doesn't return
    if args.first().map(|s| s.as_str()) == Some("serve") {
        mcp::serve();
        return;
    }

    let result = match args.first().map(|s| s.as_str()) {
        Some("update") => updater::update(),
        Some("init") => core::init(),
        Some("import") => {
            let mnemonic = args.get(1..).map(|words| words.join(" "));
            match mnemonic {
                Some(m) if !m.is_empty() => core::import(&m),
                _ => Err("Usage: pivx-agent-kit import <word1 word2 ... word24>".into()),
            }
        }
        Some("export") => {
            let confirm = args.get(1).map(|s| s.as_str()) == Some("true");
            core::export_checked(confirm, operator::Terminal::is_interactive(&operator::RealTerminal))
        }
        Some("sign-message") => {
            let message = args.get(1..).map(|words| words.join(" "));
            match message {
                Some(m) if !m.is_empty() => core::sign_message(&m),
                _ => Err("Usage: pivx-agent-kit sign-message <message>".into()),
            }
        }
        Some("address") => core::address(),
        Some("balance") => core::balance(),
        Some("resync") => core::resync(),
        Some("task") => {
            let sub_args: Vec<String> = args.iter().skip(1).cloned().collect();
            task::dispatch(&sub_args)
        }
        Some("cards") => {
            let sub_args: Vec<String> = args.iter().skip(1).cloned().collect();
            cards::dispatch(&sub_args)
        }
        Some("send") => match cli::parse_send_args(&args[1..]) {
            Ok(a) => core::resolve_amount(a.amount).and_then(|(amount_sat, price)| {
                core::send(&spend::SendRequest {
                    address: a.address,
                    amount_sat,
                    memo: a.memo,
                    from: a.from,
                    reason: a.reason,
                    origin: spend::Origin::Send,
                    price,
                })
            }),
            Err(e) => Err(e.into()),
        },
        Some("policy") => policy_command(&args[1..]),
        Some("pending") => pending_command(&args[1..]),
        Some("history") => match cli::parse_history_args(&args[1..]) {
            Ok(h) if h.csv => match core::history_csv(h.limit, h.since) {
                Ok(csv) => {
                    print!("{}", csv);
                    return;
                }
                Err(e) => Err(e),
            },
            Ok(h) => core::history(h.limit, h.since),
            Err(e) => Err(e.into()),
        },
        _ => {
            println!("{}", help_text());
            return;
        }
    };

    match result {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
            let code = exit_code_for(&output);
            if code != 0 {
                process::exit(code);
            }
        }
        Err(e) => {
            let err = json!({ "error": e.to_string() });
            eprintln!("{}", serde_json::to_string_pretty(&err).unwrap());
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_payments_exit_with_2() {
        assert_eq!(exit_code_for(&json!({"status": "pending_approval", "pending_id": "p_1"})), 2);
        assert_eq!(exit_code_for(&json!({"status": "sent"})), 0);
        assert_eq!(exit_code_for(&json!({"count": 0})), 0);
        // A card payment that's held surfaces the same status.
        assert_eq!(exit_code_for(&json!({"order_id": "x", "status": "pending_approval"})), 2);
    }
}
