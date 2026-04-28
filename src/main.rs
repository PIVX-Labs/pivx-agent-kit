mod core;
mod keys;
mod mcp;
mod network;
mod prover;
mod shield;
mod sync;
mod task;
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
  send <address> <amount> --from <private|public> [memo]
                                                Send PIV from private or public balance
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
  update                                        Update to the latest release", VERSION)
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
            core::export(confirm)
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
        Some("send") => {
            let addr = args.get(1).map(|s| s.as_str());
            let amount_str = args.get(2).map(|s| s.as_str());
            // Parse --from flag and memo from remaining args
            let mut from = "private";
            let mut memo = "";
            let mut i = 3;
            while i < args.len() {
                if args[i] == "--from" {
                    if let Some(f) = args.get(i + 1) {
                        from = if f == "public" { "public" } else { "private" };
                        i += 2;
                        continue;
                    }
                } else if memo.is_empty() {
                    memo = &args[i];
                }
                i += 1;
            }
            match (addr, amount_str) {
                (Some(a), Some(amt)) => match core::parse_piv_to_sat(amt) {
                    Ok(sat) if sat == 0 => Err("Amount must be greater than zero".into()),
                    Ok(sat) => core::send(a, sat, memo, from),
                    Err(e) => Err(e.into()),
                },
                _ => Err("Usage: pivx-agent-kit send <address> <amount> --from <private|public> [memo]".into()),
            }
        }
        _ => {
            println!("{}", help_text());
            return;
        }
    };

    match result {
        Ok(output) => {
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        Err(e) => {
            let err = json!({ "error": e.to_string() });
            eprintln!("{}", serde_json::to_string_pretty(&err).unwrap());
            process::exit(1);
        }
    }
}
