//! `pivx-agent-kit cards` — native client for PIVCards
//! (`cards.pivxla.bz`). Lets agents spend PIV on real-world gift
//! cards (Amazon, Steam, Uber, etc.) at any of the regional storefronts
//! PIVCards exposes.
//!
//! Endpoints touched (all unauthenticated — order privacy is via the
//! 32-byte random order ID itself):
//!   GET /api/v1/regions
//!   GET /api/v1/items/search?s=&region=
//!   GET /api/v1/items/details?item=
//!   GET /api/v1/order/create?item=&amount=&refundAddress=
//!   GET /api/v1/order/check?id=
//!   GET /api/v1/order/cancel?id=

mod client;
mod state;
pub(crate) mod commands;

use serde_json::Value;
use std::error::Error;

pub fn dispatch(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "regions" => commands::regions(&rest),
        "search" => commands::search(&rest),
        "details" => commands::details(&rest),
        "order" => order_dispatch(&rest),
        "" => Err(help_text().into()),
        other => Err(format!("unknown cards subcommand: {}\n\n{}", other, help_text()).into()),
    }
}

fn order_dispatch(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "create" => commands::order_create(&rest),
        "check" => commands::order_check(&rest),
        "pay" => commands::order_pay(&rest),
        "cancel" => commands::order_cancel(&rest),
        "list" => commands::order_list(&rest),
        "" => Err(help_text().into()),
        other => Err(format!("unknown cards order subcommand: {}\n\n{}", other, help_text()).into()),
    }
}

pub fn help_text() -> &'static str {
    "Usage: pivx-agent-kit cards <subcommand>

Discovery:
  regions                                       List supported region codes
  search   <query> [--region <CODE>]            Find card brands matching <query>
  details  <slug>                               Item info incl. packages (denominations)

Order lifecycle:
  order create <slug> --amount <N>              Open an invoice. Refund address is the
                                                kit's transparent address automatically.
  order pay    <id> [--from private|public]     Send the order's PIV from the wallet.
                                                Validates state and uses the platform's
                                                quoted price. Balance pool is
                                                auto-selected (prefers private/shield
                                                for privacy, falls back to public/
                                                transparent). Pass --from to override.
  order check  <id>                             Status + (when COMPLETE) dispatch payload
                                                (redemption code/pin).
  order cancel <id>                             Cancel a PENDING / PARTIAL_PENDING order.
  order list                                    List orders this wallet has created.

Status transitions returned by `order check`:
  status_num: 1 PENDING → 2 PARTIAL_PENDING / 3 PROCESSING → 4 WAITING → 5 COMPLETE
              6 CANCELLED is a terminal state for cancellations / expiry."
}
