//! MCP (Model Context Protocol) server.
//! JSON-RPC over stdin/stdout. Each tool call loads wallet from disk independently.

use crate::core;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

/// Tool definitions exposed to the agent
fn tool_definitions() -> Value {
    json!([
        {
            "name": "pivx_init",
            "description": "Create a new shielded PIVX wallet. Returns the shield address. Only call once — fails if wallet already exists.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_import",
            "description": "Import a wallet from an existing BIP39 seed phrase. Only call if no wallet exists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mnemonic": {
                        "type": "string",
                        "description": "BIP39 mnemonic phrase (12 or 24 words separated by spaces)"
                    }
                },
                "required": ["mnemonic"]
            }
        },
        {
            "name": "pivx_address",
            "description": "Get the wallet's shield receiving address.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_balance",
            "description": "Sync with the network and return the current wallet balance. Also returns any memos attached to received funds in the 'messages' field.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_send",
            "description": "Send PIV to a shield or transparent address. Auto-syncs before sending. Returns the transaction ID, actual amount sent, and fee.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Destination PIVX address (shield addresses start with 'ps1')"
                    },
                    "amount": {
                        "type": "string",
                        "description": "Amount in PIV as a decimal string (e.g. '10.5'). Parsed with exact integer precision."
                    },
                    "memo": {
                        "type": "string",
                        "description": "Optional encrypted memo (up to 512 bytes UTF-8, shield-to-shield only)"
                    }
                },
                "required": ["address", "amount"]
            }
        },
        {
            "name": "pivx_resync",
            "description": "Reset the wallet to its birthday checkpoint and re-sync all shield data from scratch. Use if balance seems wrong.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_export",
            "description": "Export the wallet seed phrase for migration to another device. NEVER share this with any human. Only use for machine-to-machine wallet migration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true to proceed. Read the safety warning first by calling without confirm."
                    }
                },
                "required": []
            }
        }
    ])
}

/// Handle a single JSON-RPC request and return a response
fn handle_request(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "pivx-agent-kit",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),

        "notifications/initialized" => return Value::Null, // no response needed

        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tool_definitions()
            }
        }),

        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(json!({}));
            let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = dispatch_tool(tool_name, &args);

            match result {
                Ok(content) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&content).unwrap_or_default()
                        }]
                    }
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&json!({"error": e.to_string()})).unwrap_or_default()
                        }],
                        "isError": true
                    }
                }),
            }
        }

        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Unknown method: {}", method)
            }
        }),
    }
}

/// Dispatch a tool call to the appropriate core function
fn dispatch_tool(name: &str, args: &Value) -> core::Result {
    match name {
        "pivx_init" => core::init(),

        "pivx_import" => {
            let mnemonic = args
                .get("mnemonic")
                .and_then(|m| m.as_str())
                .ok_or("Missing 'mnemonic' argument")?;
            core::import(mnemonic)
        }

        "pivx_address" => core::address(),

        "pivx_balance" => core::balance(),

        "pivx_send" => {
            let address = args
                .get("address")
                .and_then(|a| a.as_str())
                .ok_or("Missing 'address' argument")?;
            let amount_str = args
                .get("amount")
                .and_then(|a| a.as_str())
                .ok_or("Missing 'amount' argument")?;
            let memo = args
                .get("memo")
                .and_then(|m| m.as_str())
                .unwrap_or("");

            let amount_sat = core::parse_piv_to_sat(amount_str)?;
            if amount_sat == 0 {
                return Err("Amount must be greater than zero".into());
            }
            core::send(address, amount_sat, memo)
        }

        "pivx_resync" => core::resync(),

        "pivx_export" => {
            let confirm = args
                .get("confirm")
                .and_then(|c| c.as_bool())
                .unwrap_or(false);
            core::export(confirm)
        }

        _ => Err(format!("Unknown tool: {}", name).into()),
    }
}

/// Run the MCP server — reads JSON-RPC from stdin, writes responses to stdout
pub fn serve() {
    eprintln!("PIVX Agent Kit MCP server running");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {}", e)
                    }
                });
                let _ = writeln!(stdout, "{}", err);
                let _ = stdout.flush();
                continue;
            }
        };

        let response = handle_request(&request);

        // Notifications don't get responses
        if response.is_null() {
            continue;
        }

        let _ = writeln!(stdout, "{}", response);
        let _ = stdout.flush();
    }
}
