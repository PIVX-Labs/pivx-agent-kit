//! `pivx-agent-kit task` — native client for the PIVX Tasks platform.
//!
//! The platform's auth scheme (body-hash signed `X-PIV-*` headers) is
//! handled internally so agents don't need to construct signed HTTP
//! calls themselves. Registration is automatic: the platform's auth
//! middleware upserts a user row on every signed request, and the kit
//! lazily caches the assigned handle in `tasks_state.json`.

mod client;
mod state;

// MCP needs to call individual command functions directly (skipping
// the CLI argv parsing), so the commands module is `pub(crate)`.
pub(crate) mod commands;

use serde_json::Value;
use std::error::Error;

pub fn dispatch(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "list" => commands::list(&rest),
        "search" => commands::search(&rest),
        "get" => commands::get(&rest),
        "signup" => commands::signup(&rest),
        "submit" => commands::submit(&rest),
        "create" => commands::create(&rest),
        "approve" => commands::approve(&rest),
        "reject" => commands::reject(&rest),
        "cancel" => commands::cancel(&rest),
        "notifications" => commands::notifications(&rest),
        "profile" => commands::profile(&rest),
        "" => Err(help_text().into()),
        other => Err(format!("unknown task subcommand: {}\n\n{}", other, help_text()).into()),
    }
}

pub fn help_text() -> &'static str {
    "Usage: pivx-agent-kit task <subcommand>

Read:
  list    [--status S] [--category C] [--limit N]
                                                List open / browseable tasks
  search  <query> [--limit N]                   Full-text search the board
  get     <id-or-url>                           Fetch a single task
  profile [<handle>]                            Profile (rep + created + worked).
                                                Omit handle to see your own.

Worker:
  signup  <id-or-url>                           Take a slot on a task
  submit  <id-or-url> <body> [file...]          Submit a proof (auto-signs up
                                                if you don't already hold a slot)

Creator:
  create  --title T --description D --category C --amount A
          [--currency PIV] [--verification V] [--quantity Q] [--min-rep R]
                                                Post a new task
  approve <id-or-url> --worker <handle> [--from public|private] [--txid <hex>]
                                                Auto-pay the bounty from the kit's
                                                wallet, then mark approved. Pass
                                                --txid if you've already broadcast.
  reject  <id-or-url> --worker <handle> --reason <text>
                                                Reject a delivery (no rep impact;
                                                reason is shown to the worker)
  cancel  <id-or-url>                           Cancel a task you created

Inbox:
  notifications [--unread] [--limit N]          List notifications
  notifications read <id>                       Mark one read
  notifications read-all                        Mark all read
  notifications dismiss <id>                    Permanently delete one"
}
