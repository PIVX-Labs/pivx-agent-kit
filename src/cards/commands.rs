//! `pivx-agent-kit cards <subcommand>` command handlers.
//!
//! Lifecycle of a purchase, from the agent's perspective:
//!
//!   1. `cards search <query> [--region]` — narrow down a brand.
//!   2. `cards details <slug>` — confirm packages (denominations) and
//!      that the item is buyable (`recipientType: "none"`).
//!   3. `cards order create <slug> --amount <N>` — opens the invoice;
//!      returns `{id, paymentAddress, paymentTotal_piv, expiry}`.
//!      The kit's transparent address is used as the refund address
//!      automatically — the agent never needs to think about it.
//!   4. `cards order pay <id> --from public|private` — convenience
//!      that re-fetches the order, validates state, sends the right
//!      PIV amount via `core::send`, and returns the txid.
//!   5. `cards order check <id>` — poll until `status_num == 5`
//!      (COMPLETE); the `dispatch` field then carries the redemption
//!      code/pin.
//!
//! State machine returned by `/order/check` (PIVCards' enum):
//!     1 PENDING          — waiting for first payment
//!     2 PARTIAL_PENDING  — partial payment received
//!     3 PROCESSING       — full payment, awaiting confirmations
//!     4 WAITING          — paid, awaiting upstream dispatch
//!     5 COMPLETE         — dispatch released, code/pin available
//!     6 CANCELLED        — cancelled or expired

use serde_json::{json, Value};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

use super::client::{url_encode, Client};
use super::state::{self, OrderEntry};
use crate::core;
use crate::wallet;

type Result = std::result::Result<Value, Box<dyn Error>>;

// ---------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------

pub fn regions(_args: &[String]) -> Result {
    Client::new().get("/api/v1/regions")
}

pub fn search(args: &[String]) -> Result {
    let mut query: Option<String> = None;
    let mut region: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--region" => {
                region = args.get(i + 1).cloned();
                i += 2;
            }
            other if !other.starts_with("--") && query.is_none() => {
                query = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unexpected arg: {}", other).into()),
        }
    }
    let q = query.ok_or(
        "Usage: cards search <query> [--region <EU|DE|GB|US|CA>]",
    )?;

    let mut path = format!("/api/v1/items/search?s={}", url_encode(&q));
    if let Some(r) = region.as_ref() {
        path.push_str(&format!("&region={}", url_encode(r)));
    }

    let raw = Client::new().get(&path)?;

    // The platform returns `[]` when no matches; pass through.
    let arr = raw
        .as_array()
        .ok_or("unexpected response shape from /items/search")?;

    // Filter to buyable items only — recipientType must be "none"
    // (no phone/email-bound cards). The search endpoint doesn't
    // include recipientType, so we surface what's there but flag
    // that details should be checked before order/create.
    let trimmed: Vec<Value> = arr
        .iter()
        .map(|item| {
            json!({
                "slug": item.get("slug"),
                "name": item.get("name"),
                "currency": item.get("currency"),
                "country": item.get("countryCode"),
                "logo": item.get("logoImage"),
            })
        })
        .collect();

    Ok(json!({
        "query": q,
        "region": region,
        "count": trimmed.len(),
        "results": trimmed,
        "_hint": "Pick a slug, then call `cards details <slug>` to see purchasable packages (denominations).",
    }))
}

pub fn details(args: &[String]) -> Result {
    let slug = args
        .first()
        .ok_or("Usage: cards details <slug>")?
        .clone();
    let raw = Client::new().get(&format!(
        "/api/v1/items/details?item={}",
        url_encode(&slug)
    ))?;

    // `recipientType` and `outOfStock` are the deal-breakers for an
    // agent — surface them up top so it doesn't have to dig.
    let recipient_type = raw
        .get("recipientType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let out_of_stock = raw
        .get("outOfStock")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let buyable = recipient_type == "none" && !out_of_stock;

    // Pluck the packages array, surfacing only the agent-relevant
    // fields. The full package object includes per-fiat conversion
    // tables that aren't useful here — `value` is the denomination
    // (as a string, in the item's `currency`) and the order/create
    // endpoint takes that as `amount`.
    let packages: Vec<Value> = raw
        .get("packages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    json!({
                        "amount": p.get("value"),
                        "usd_price": p.get("prices").and_then(|x| x.get("USD")),
                        "eur_price": p.get("eurPrice"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "slug": raw.get("slug"),
        "name": raw.get("name"),
        "base_name": raw.get("baseName"),
        "currency": raw.get("billCurrency"),
        "country": raw.get("countryCode"),
        "price_range": raw.get("_priceRange"),
        "recipient_type": recipient_type,
        "out_of_stock": out_of_stock,
        "buyable": buyable,
        "packages": packages,
        "redemption_methods": raw.get("redemptionMethods"),
        "_hint": if buyable {
            "Choose a package amount, then call `cards order create <slug> --amount <N>`."
        } else if out_of_stock {
            "Item is currently out of stock; no order can be placed."
        } else {
            "Item requires a recipient (phone/email) and isn't supported by the kit."
        },
    }))
}

// ---------------------------------------------------------------------
// Order lifecycle
// ---------------------------------------------------------------------

pub fn order_create(args: &[String]) -> Result {
    let mut slug: Option<String> = None;
    let mut amount: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--amount" => {
                amount = args.get(i + 1).cloned();
                i += 2;
            }
            other if !other.starts_with("--") && slug.is_none() => {
                slug = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unexpected arg: {}", other).into()),
        }
    }
    let slug = slug.ok_or("Usage: cards order create <slug> --amount <N>")?;
    let amount = amount.ok_or("Usage: cards order create <slug> --amount <N>")?;

    // Refund address is ALWAYS the kit's transparent address. Not
    // exposed as a flag — it's a property of the wallet, not a
    // per-call decision the agent should make.
    let refund_addr = current_transparent_address()?;

    let path = format!(
        "/api/v1/order/create?item={}&amount={}&refundAddress={}",
        url_encode(&slug),
        url_encode(&amount),
        url_encode(&refund_addr),
    );
    let raw = Client::new().get(&path)?;

    // Persist before we return so the agent can resume even if the
    // CLI invocation crashes between create and pay.
    if let Some(id) = raw.get("id").and_then(|v| v.as_str()) {
        let entry = OrderEntry {
            id: id.to_string(),
            item_slug: Some(slug.clone()),
            amount: Some(format!(
                "{} {}",
                amount,
                raw.get("product")
                    .and_then(|p| p.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            )),
            created_at: now_unix_secs(),
        };
        // Best-effort: if the cache write fails, don't fail the
        // user-visible operation. The order ID is still in the
        // returned JSON so the agent can re-record it.
        let _ = state::record_order(entry);
    }

    Ok(shape_order(&raw))
}

pub fn order_check(args: &[String]) -> Result {
    let id = args
        .first()
        .ok_or("Usage: cards order check <order-id>")?
        .clone();
    let raw = Client::new().get(&format!("/api/v1/order/check?id={}", url_encode(&id)))?;
    Ok(shape_order(&raw))
}

pub fn order_cancel(args: &[String]) -> Result {
    let id = args
        .first()
        .ok_or("Usage: cards order cancel <order-id>")?
        .clone();
    let raw = Client::new().get(&format!("/api/v1/order/cancel?id={}", url_encode(&id)))?;
    Ok(shape_order(&raw))
}

/// Convenience: pay an open order from the wallet. Re-fetches the
/// order state first to (a) verify it's still PENDING and (b) get
/// the canonical `paymentAddress` and `paymentTotal` from the
/// platform — the agent doesn't have to thread those through itself.
pub fn order_pay(args: &[String]) -> Result {
    let mut id: Option<String> = None;
    let mut from = "private".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                let v = args
                    .get(i + 1)
                    .ok_or("--from requires a value (private|public)")?;
                if v != "private" && v != "public" {
                    return Err("--from must be 'private' or 'public'".into());
                }
                from = v.to_string();
                i += 2;
            }
            other if !other.starts_with("--") && id.is_none() => {
                id = Some(other.to_string());
                i += 1;
            }
            other => return Err(format!("unexpected arg: {}", other).into()),
        }
    }
    let id = id.ok_or("Usage: cards order pay <order-id> [--from private|public]")?;

    let order = Client::new().get(&format!("/api/v1/order/check?id={}", url_encode(&id)))?;

    let status_num = order
        .get("status_num")
        .and_then(|v| v.as_i64())
        .ok_or("order response missing status_num")?;
    if status_num != 1 {
        return Err(format!(
            "order is not in PENDING state (status_num={}, status={}). Pay only works on a pending order.",
            status_num,
            order.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
        )
        .into());
    }

    let payment_address = order
        .get("paymentAddress")
        .and_then(|v| v.as_str())
        .ok_or("order missing paymentAddress")?
        .to_string();
    // PIVCards stores the price as a number of PIV (decimal). Use
    // the kit's existing decimal parser so we never lose precision
    // at the satoshi boundary.
    let amount_piv = order
        .get("paymentTotal")
        .map(|v| {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                v.to_string()
            }
        })
        .ok_or("order missing paymentTotal")?;
    let amount_sat = core::parse_piv_to_sat(&amount_piv)
        .map_err(|e| format!("could not parse paymentTotal '{}': {}", amount_piv, e))?;

    let send_result = core::send(&payment_address, amount_sat, "", &from)?;

    Ok(json!({
        "order_id": id,
        "from": from,
        "amount_piv": amount_piv,
        "amount_sat": amount_sat,
        "payment_address": payment_address,
        "send_result": send_result,
        "_hint": "Poll `cards order check <order-id>` until status_num == 5 (COMPLETE). The dispatch field will then carry the redemption code/pin.",
    }))
}

pub fn order_list(_args: &[String]) -> Result {
    let cached = state::list_orders();
    let entries: Vec<Value> = cached
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "item_slug": e.item_slug,
                "amount": e.amount,
                "created_at": e.created_at,
            })
        })
        .collect();
    Ok(json!({
        "count": entries.len(),
        "orders": entries,
        "_hint": if entries.is_empty() {
            "No orders cached for this wallet. Create one with `cards order create <slug> --amount <N>`."
        } else {
            "Use `cards order check <id>` for live status; `cards order pay <id>` to fund a pending order."
        },
    }))
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Project a raw order response into the agent-facing shape. PIVCards'
/// `toPublicJSON` already returns most of what we want, but trimming
/// + renaming gives the agent a flatter, more obvious set of fields.
fn shape_order(raw: &Value) -> Value {
    json!({
        "id": raw.get("id"),
        "status": raw.get("status"),
        "status_num": raw.get("status_num"),
        "product": raw.get("product"),
        "payment_total_piv": raw.get("paymentTotal"),
        "payment_remaining_piv": raw.get("paymentRemaining"),
        "payment_address": raw.get("paymentAddress"),
        "confirmations": raw.get("confirmations"),
        "refund_address": raw.get("refundAddress"),
        "expiry": raw.get("expiry"),
        "expiry_utc": raw.get("serverTimeExpiry"),
        // The dispatch payload (code/pin) is null until status_num==5.
        // When present, it's the redemption data.
        "dispatch": raw.get("dispatch"),
        "cashback_code": raw.get("cashbackCode"),
    })
}

fn current_transparent_address() -> std::result::Result<String, Box<dyn Error>> {
    let wallet_data = wallet::load_wallet()
        .map_err(|e| format!("cannot load wallet (run `init` or `import` first): {}", e))?;
    let addr = wallet_data.get_transparent_address()?;
    Ok(addr)
}

fn now_unix_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}
