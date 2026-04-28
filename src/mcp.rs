//! MCP (Model Context Protocol) server.
//! JSON-RPC over stdin/stdout. Each tool call loads wallet from disk independently.

use crate::core;
use crate::task;
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
            "description": "Get the wallet's shield (private) and transparent (public) receiving addresses.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_balance",
            "description": "Sync with the network and return both private (shield) and public (transparent) balances. Also returns any memos attached to received shield funds in the 'messages' field.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_send",
            "description": "Send PIV to a shield or transparent address. Specify 'from' to choose which balance to spend from. Auto-syncs before sending.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Destination PIVX address (shield 'ps1...' or transparent 'D...')"
                    },
                    "amount": {
                        "type": "string",
                        "description": "Amount in PIV as a decimal string (e.g. '10.5')"
                    },
                    "from": {
                        "type": "string",
                        "description": "Which balance to spend from: 'private' (shield) or 'public' (transparent). Required."
                    },
                    "memo": {
                        "type": "string",
                        "description": "Optional encrypted memo (up to 512 bytes UTF-8, private-to-private only)"
                    }
                },
                "required": ["address", "amount", "from"]
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
        },
        {
            "name": "pivx_sign_message",
            "description": "Sign an arbitrary message with the wallet's transparent (D-prefix) private key. Returns a base64 signature byte-compatible with PIVX Core's verifymessage RPC plus the signing address. Use for proving address ownership (auth challenges, profile linking, app login flows). The seed never leaves the wallet — only the per-message signature does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message to sign. Verifiers use the same string with the returned address + signature to confirm ownership."
                    }
                },
                "required": ["message"]
            }
        },

        // ---------------------------------------------------------
        // PIVX Tasks platform (https://tasks.pivxla.bz). Auth is
        // handled internally — the kit signs every authed request
        // with the wallet's transparent key and the platform
        // auto-registers the address on first signed call. Override
        // the platform endpoint via the `PIVX_TASKS_API` env var.
        // ---------------------------------------------------------

        {
            "name": "pivx_task_list",
            "description": "Browse the PIVX Tasks bounty board. Returns an array of tasks (newest first). Optionally filter by status, category, or cap with limit. Useful for agents looking for paid work to take on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by task status: open / in_progress / submitted / paid" },
                    "category": { "type": "string", "description": "Filter by category: dev, design, content, social, research, marketing, other" },
                    "limit": { "type": "integer", "description": "Maximum number of tasks to return", "minimum": 1 }
                },
                "required": []
            }
        },
        {
            "name": "pivx_task_search",
            "description": "Full-text search the PIVX Tasks bounty board. Returns an array of matching tasks. Use when you have a specific topic or keyword in mind.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query — matches against task titles and descriptions" },
                    "limit": { "type": "integer", "description": "Maximum number of results", "minimum": 1 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "pivx_task_get",
            "description": "Fetch full details of a single task. Accepts a numeric id, a `task?id=N` query path, or a full URL like `https://tasks.pivxla.bz/task?id=5`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id (e.g. '5') or task page URL" }
                },
                "required": ["id_or_url"]
            }
        },
        {
            "name": "pivx_task_profile",
            "description": "Look up a user's PIVX Tasks profile: reputation card, completion rate, tasks they created, tasks they worked. Omit `handle` to see your own profile (the agent's identity on the platform). The kit caches its own handle locally after the first call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "User handle to look up (e.g. 'frosted-otter-417'). Omit for self." }
                },
                "required": []
            }
        },
        {
            "name": "pivx_task_signup",
            "description": "Take a slot on a task — commit to delivering. The platform will reserve a slot and let you submit a proof later. Bailing without delivering counts against your reputation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id or URL" }
                },
                "required": ["id_or_url"]
            }
        },
        {
            "name": "pivx_task_submit",
            "description": "Submit a proof of work for a task. Auto-signs you up if you don't already hold a slot. Body is required; files are optional and supplied as filesystem paths the kit reads from disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id or URL" },
                    "body": { "type": "string", "description": "Free-form text body describing your delivery" },
                    "files": {
                        "type": "array",
                        "description": "Optional list of file paths to attach. Each file must be readable from the kit's filesystem.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["id_or_url", "body"]
            }
        },
        {
            "name": "pivx_task_create",
            "description": "Post a new task on the PIVX Tasks bounty board. You become the creator; you'll need to approve or reject deliveries via pivx_task_approve / pivx_task_reject. Verification text is required so workers know how to prove completion.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short task title (≤60 chars)" },
                    "description": { "type": "string", "description": "Full task description, markdown-friendly" },
                    "category": { "type": "string", "description": "One of: dev, design, content, social, research, marketing, other" },
                    "amount": { "type": "string", "description": "Bounty amount per slot as a decimal string (e.g. '0.001', '10.5'). String avoids JSON-number precision loss at sat-level amounts." },
                    "currency": { "type": "string", "description": "Currency for the bounty. Defaults to 'PIV'." },
                    "verification": { "type": "string", "description": "Required: how should workers prove they completed the work?" },
                    "quantity": { "type": "integer", "description": "Number of independent slots to advertise. Defaults to 1.", "minimum": 1 },
                    "min_rep": { "type": "integer", "description": "Minimum worker reputation required to sign up. Defaults to 0 (open to all).", "minimum": 0 }
                },
                "required": ["title", "description", "category", "amount", "verification"]
            }
        },
        {
            "name": "pivx_task_approve",
            "description": "Approve a worker's delivery and pay out the bounty. By default the kit auto-pays from its own wallet (broadcasts a real on-chain PIV transaction) and then records the txid with the platform. Pass `txid` to skip auto-pay if you've already broadcast externally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id or URL" },
                    "worker": { "type": "string", "description": "Worker's handle (e.g. 'frosted-otter-417')" },
                    "from": { "type": "string", "description": "Which balance to pay from: 'public' (transparent, default) or 'private' (shield)" },
                    "txid": { "type": "string", "description": "If you've already broadcast the payment, pass its 64-hex txid to skip auto-pay" }
                },
                "required": ["id_or_url", "worker"]
            }
        },
        {
            "name": "pivx_task_reject",
            "description": "Reject a worker's submitted delivery. The slot frees up and the worker can retry; neither side takes a reputation hit. A clear reason is required and shown to the worker — explain what they need to change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id or URL" },
                    "worker": { "type": "string", "description": "Worker's handle" },
                    "reason": { "type": "string", "description": "Required: clear, specific note on what to change. Capped at 500 chars." }
                },
                "required": ["id_or_url", "worker", "reason"]
            }
        },
        {
            "name": "pivx_task_cancel",
            "description": "Cancel a task you created. If no commitments exist yet, the task is deleted; otherwise it's marked cancelled and any in-flight workers are released.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id_or_url": { "type": "string", "description": "Task id or URL" }
                },
                "required": ["id_or_url"]
            }
        },
        {
            "name": "pivx_task_notifications",
            "description": "List your platform notifications (proof submitted, payment received, delivery rejected, etc.). Returns an object with `items` (array, newest first) and `unread` (count). Use pivx_task_notification_read / _read_all / _dismiss to manage individual entries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unread_only": { "type": "boolean", "description": "Return only unread notifications" },
                    "limit": { "type": "integer", "description": "Maximum items to return", "minimum": 1 }
                },
                "required": []
            }
        },
        {
            "name": "pivx_task_notification_read",
            "description": "Mark a single notification as read. No-op if it was already read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Notification id" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "pivx_task_notification_read_all",
            "description": "Mark every unread notification as read. Returns the count of rows updated.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "pivx_task_notification_dismiss",
            "description": "Permanently delete a single notification.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Notification id" }
                },
                "required": ["id"]
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

            let from = args
                .get("from")
                .and_then(|f| f.as_str())
                .ok_or("Missing 'from' argument. Must be 'private' or 'public'.")?;

            let amount_sat = core::parse_piv_to_sat(amount_str)?;
            if amount_sat == 0 {
                return Err("Amount must be greater than zero".into());
            }
            core::send(address, amount_sat, memo, from)
        }

        "pivx_resync" => core::resync(),

        "pivx_export" => {
            let confirm = args
                .get("confirm")
                .and_then(|c| c.as_bool())
                .unwrap_or(false);
            core::export(confirm)
        }

        "pivx_sign_message" => {
            let message = args.get("message").and_then(|m| m.as_str());
            match message {
                Some(m) if !m.is_empty() => core::sign_message(m),
                _ => Err("message is required".into()),
            }
        }

        // ---------------------------------------------------------
        // PIVX Tasks platform tools — JSON args translate to the
        // same flag/arg shape the CLI uses, so all parsing /
        // validation lives in one place (task::commands).
        // ---------------------------------------------------------

        "pivx_task_list" => {
            let mut argv = Vec::<String>::new();
            push_str_flag(&mut argv, "--status", args.get("status"));
            push_str_flag(&mut argv, "--category", args.get("category"));
            push_int_flag(&mut argv, "--limit", args.get("limit"));
            task::commands::list(&argv)
        }

        "pivx_task_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or("'query' is required")?;
            let mut argv = vec![query.to_string()];
            push_int_flag(&mut argv, "--limit", args.get("limit"));
            task::commands::search(&argv)
        }

        "pivx_task_get" => {
            let id = require_id_or_url(args)?;
            task::commands::get(&[id])
        }

        "pivx_task_profile" => {
            let mut argv = Vec::<String>::new();
            if let Some(h) = args.get("handle").and_then(|v| v.as_str()) {
                if !h.is_empty() {
                    argv.push(h.to_string());
                }
            }
            task::commands::profile(&argv)
        }

        "pivx_task_signup" => {
            let id = require_id_or_url(args)?;
            task::commands::signup(&[id])
        }

        "pivx_task_submit" => {
            let id = require_id_or_url(args)?;
            let body = args
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or("'body' is required")?
                .to_string();
            let mut argv = vec![id, body];
            if let Some(arr) = args.get("files").and_then(|v| v.as_array()) {
                for f in arr {
                    if let Some(s) = f.as_str() {
                        argv.push(s.to_string());
                    }
                }
            }
            task::commands::submit(&argv)
        }

        "pivx_task_create" => {
            let mut argv = Vec::<String>::new();
            push_str_flag(&mut argv, "--title", args.get("title"));
            push_str_flag(&mut argv, "--description", args.get("description"));
            push_str_flag(&mut argv, "--category", args.get("category"));
            // amount is a decimal string (avoid f64 precision loss at sat scale).
            // Accept f64 for backwards-tolerance and fail if neither shape is present.
            if let Some(s) = args.get("amount").and_then(|v| v.as_str()) {
                argv.push("--amount".into());
                argv.push(s.to_string());
            } else {
                push_num_flag(&mut argv, "--amount", args.get("amount"));
            }
            push_str_flag(&mut argv, "--currency", args.get("currency"));
            push_str_flag(&mut argv, "--verification", args.get("verification"));
            push_int_flag(&mut argv, "--quantity", args.get("quantity"));
            push_int_flag(&mut argv, "--min-rep", args.get("min_rep"));
            task::commands::create(&argv)
        }

        "pivx_task_approve" => {
            let id = require_id_or_url(args)?;
            let mut argv = vec![id];
            push_str_flag(&mut argv, "--worker", args.get("worker"));
            push_str_flag(&mut argv, "--from", args.get("from"));
            push_str_flag(&mut argv, "--txid", args.get("txid"));
            task::commands::approve(&argv)
        }

        "pivx_task_reject" => {
            let id = require_id_or_url(args)?;
            let mut argv = vec![id];
            push_str_flag(&mut argv, "--worker", args.get("worker"));
            push_str_flag(&mut argv, "--reason", args.get("reason"));
            task::commands::reject(&argv)
        }

        "pivx_task_cancel" => {
            let id = require_id_or_url(args)?;
            task::commands::cancel(&[id])
        }

        "pivx_task_notifications" => {
            let mut argv = vec!["list".to_string()];
            if args.get("unread_only").and_then(|v| v.as_bool()).unwrap_or(false) {
                argv.push("--unread".into());
            }
            push_int_flag(&mut argv, "--limit", args.get("limit"));
            task::commands::notifications(&argv)
        }

        "pivx_task_notification_read" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("'id' is required and must be a number")?;
            task::commands::notifications(&["read".into(), id.to_string()])
        }

        "pivx_task_notification_read_all" => {
            task::commands::notifications(&["read-all".into()])
        }

        "pivx_task_notification_dismiss" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or("'id' is required and must be a number")?;
            task::commands::notifications(&["dismiss".into(), id.to_string()])
        }

        _ => Err(format!("Unknown tool: {}", name).into()),
    }
}

// ---------------------------------------------------------------
// JSON-args → CLI-args adapters. Keeps `dispatch_tool` declarative;
// each helper is no-op when the JSON field is absent so optional
// MCP parameters stay optional.
// ---------------------------------------------------------------

fn require_id_or_url(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    args.get("id_or_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "'id_or_url' is required".into())
}

fn push_str_flag(argv: &mut Vec<String>, flag: &str, v: Option<&Value>) {
    if let Some(s) = v.and_then(|v| v.as_str()) {
        if !s.is_empty() {
            argv.push(flag.to_string());
            argv.push(s.to_string());
        }
    }
}

fn push_int_flag(argv: &mut Vec<String>, flag: &str, v: Option<&Value>) {
    if let Some(n) = v.and_then(|v| v.as_i64()) {
        argv.push(flag.to_string());
        argv.push(n.to_string());
    }
}

fn push_num_flag(argv: &mut Vec<String>, flag: &str, v: Option<&Value>) {
    if let Some(n) = v.and_then(|v| v.as_f64()) {
        argv.push(flag.to_string());
        argv.push(format!("{}", n));
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
