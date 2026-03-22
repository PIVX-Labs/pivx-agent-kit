mod checkpoint;
mod core;
mod keys;
mod mainnet_checkpoints;
mod mcp;
mod network;
mod prover;
mod shield;
mod simd;
mod sync;
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
  init                              Create a new shielded wallet
  import <mnemonic>                 Import wallet from seed phrase
  address                           Show the shield receiving address
  balance                           Sync and show wallet balance
  send <address> <amount> [memo]    Send PIV to an address
  resync                            Reset and re-sync shield data from checkpoint
  export                            Export wallet seed phrase for migration
  serve                             Run as MCP server (for AI agent integration)", VERSION)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // MCP server mode — long-lived, doesn't return
    if args.first().map(|s| s.as_str()) == Some("serve") {
        mcp::serve();
        return;
    }

    let result = match args.first().map(|s| s.as_str()) {
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
        Some("address") => core::address(),
        Some("balance") => core::balance(),
        Some("resync") => core::resync(),
        Some("send") => {
            let addr = args.get(1).map(|s| s.as_str());
            let amount_str = args.get(2).map(|s| s.as_str());
            let memo = args.get(3).map(|s| s.as_str()).unwrap_or("");
            match (addr, amount_str) {
                (Some(a), Some(amt)) => match core::parse_piv_to_sat(amt) {
                    Ok(sat) if sat == 0 => Err("Amount must be greater than zero".into()),
                    Ok(sat) => core::send(a, sat, memo),
                    Err(e) => Err(e.into()),
                },
                _ => Err("Usage: pivx-agent-kit send <address> <amount> [memo]".into()),
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
