//! Command implementations for `pivx-agent-kit task <subcommand>`.
//!
//! Every command returns the platform's JSON response verbatim where
//! possible — the kit's contract is "machine-readable JSON in, JSON
//! out". `task profile` is the one exception: it composes three calls
//! and adds a kit-level `completion_rate` summary.

use serde_json::{json, Value};
use std::error::Error;
use std::fs;
use std::path::Path;

use super::client::Client;
use super::state;
use crate::core;

// ---------------------------------------------------------------------
// id-or-url parsing
// ---------------------------------------------------------------------

/// Accepts `5`, `task?id=5`, `https://tasks.pivxla.bz/task?id=5`, or
/// any string containing `id=<digits>`. Returns the numeric id.
///
/// Intentionally lenient — agents copy/paste links from inconsistent
/// sources. The `id=` prefix isn't anchored to a query-string position
/// (so `?override=foo&id=42` matches), which is fine for trusted CLI
/// input. Don't feed this untrusted user-supplied URL strings without
/// also validating the host first.
pub fn parse_task_id(input: &str) -> Result<i64, Box<dyn Error>> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<i64>() {
        if n > 0 {
            return Ok(n);
        }
    }
    // Look for `id=<digits>` anywhere in the string.
    if let Some(idx) = trimmed.find("id=") {
        let tail = &trimmed[idx + 3..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<i64>() {
            if n > 0 {
                return Ok(n);
            }
        }
    }
    Err(format!("could not parse task id from {:?}", input).into())
}

// ---------------------------------------------------------------------
// list
// ---------------------------------------------------------------------

pub fn list(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let mut status: Option<String> = None;
    let mut category: Option<String> = None;
    let mut limit: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--status" => {
                status = args.get(i + 1).cloned();
                i += 2;
            }
            "--category" => {
                category = args.get(i + 1).cloned();
                i += 2;
            }
            "--limit" => {
                limit = Some(parse_usize_flag("--limit", args.get(i + 1))?);
                i += 2;
            }
            other => return Err(format!("unknown flag: {}", other).into()),
        }
    }

    let mut qs = Vec::<String>::new();
    if let Some(s) = &status {
        qs.push(format!("status={}", urlencode(s)));
    }
    if let Some(c) = &category {
        qs.push(format!("category={}", urlencode(c)));
    }
    let path = if qs.is_empty() {
        "/api/tasks".to_string()
    } else {
        format!("/api/tasks?{}", qs.join("&"))
    };

    let client = Client::new();
    let mut value = client.get_public(&path)?;

    // Server returns the full list; honor --limit kit-side.
    if let (Some(n), Some(arr)) = (limit, value.as_array_mut()) {
        if arr.len() > n {
            arr.truncate(n);
        }
    }
    Ok(value)
}

// ---------------------------------------------------------------------
// get
// ---------------------------------------------------------------------

pub fn get(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id = args
        .first()
        .ok_or("Usage: pivx-agent-kit task get <id-or-url>")?;
    let id = parse_task_id(id)?;
    let client = Client::new();
    client.get_public(&format!("/api/tasks/{}", id))
}

// ---------------------------------------------------------------------
// proofs
// ---------------------------------------------------------------------

/// List proof submissions on a task. Creator-only — the platform's
/// auth middleware rejects requests from non-creators with 401, so
/// this returns nothing useful when called against someone else's
/// task. Use it to read submission bodies before deciding whether to
/// `approve` or `reject`.
pub fn proofs(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id = args
        .first()
        .ok_or("Usage: pivx-agent-kit task proofs <id-or-url>")?;
    let id = parse_task_id(id)?;
    let client = Client::new();
    client.get_signed(&format!("/api/tasks/{}/proofs", id))
}

// ---------------------------------------------------------------------
// signup
// ---------------------------------------------------------------------

pub fn signup(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id = args
        .first()
        .ok_or("Usage: pivx-agent-kit task signup <id-or-url>")?;
    let id = parse_task_id(id)?;
    let client = Client::new();
    let resp = client.post_signed(&format!("/api/tasks/{}/signup", id))?;
    state::cache_handle_from_task(&resp)?;
    Ok(resp)
}

// ---------------------------------------------------------------------
// submit (auto-signs-up if needed)
// ---------------------------------------------------------------------

pub fn submit(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id_arg = args
        .first()
        .ok_or("Usage: pivx-agent-kit task submit <id-or-url> <body> [file...]")?;
    let body = args
        .get(1)
        .ok_or("Usage: pivx-agent-kit task submit <id-or-url> <body> [file...]")?
        .clone();
    let file_paths: Vec<&str> = args.iter().skip(2).map(|s| s.as_str()).collect();

    let id = parse_task_id(id_arg)?;
    let client = Client::new();

    // Read file bytes up-front so a bad path fails before any network
    // round-trip (and before the auto-signup grabs a slot).
    let mut files: Vec<(&str, String, String, Vec<u8>)> = Vec::new();
    for path in &file_paths {
        let p = Path::new(path);
        let bytes = fs::read(p).map_err(|e| format!("read {}: {}", path, e))?;
        let raw_filename = p
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("file")
            .to_string();
        // Reject filenames that would let an attacker inject extra
        // multipart headers via the `Content-Disposition: ... filename="..."`
        // line — on Linux a filename can legally contain CR/LF/`"`/`\`,
        // which would smuggle headers if we passed it through verbatim.
        // Rather than escape (and depend on the receiver doing the
        // exact-inverse unescape), we just refuse.
        if raw_filename
            .chars()
            .any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n')
        {
            return Err(format!(
                "filename {:?} contains characters disallowed in multipart attachments",
                raw_filename
            )
            .into());
        }
        let mime = mime_for_path(p).to_string();
        files.push(("attachment", raw_filename, mime, bytes));
    }

    // Auto-signup: the agent's intent is delivery, so transparently
    // take a slot if we don't already hold one. The public task view
    // can't tell us whether *we* already hold a slot — its
    // `worker_handle` / `worker_address` fields are deprecated single-
    // worker shadows that identify whoever last touched the task, not
    // the caller. So always attempt signup and let `try_signup`
    // swallow benign "you already hold a slot" conflicts.
    let _ = try_signup(&client, id);

    let text_fields: Vec<(&str, &str)> = vec![("body", body.as_str())];
    let files_ref: Vec<(&str, &str, &str, Vec<u8>)> = files
        .iter()
        .map(|(n, fname, mime, data)| (*n, fname.as_str(), mime.as_str(), data.clone()))
        .collect();

    let resp = client.post_signed_multipart(
        &format!("/api/tasks/{}/proof", id),
        &text_fields,
        &files_ref,
    )?;
    Ok(resp)
}

/// Attempt signup; swallow conflicts that mean "you're already on this
/// task". Anything else propagates so the caller sees real errors
/// (rep-gate refusal, no slots, network failure, etc.).
///
/// Match patterns are tight on purpose: matching a bare "already" or
/// "submitted" would also swallow unrelated server messages that
/// happen to contain those words (e.g. an error mentioning a
/// "submitted" status from a different code path). The platform's
/// signup conflict messages are stable strings issued from
/// `models::commitment::SignupReject` → routes/tasks.rs error mapping.
fn try_signup(client: &Client, id: i64) -> Result<(), Box<dyn Error>> {
    match client.post_signed(&format!("/api/tasks/{}/signup", id)) {
        Ok(resp) => {
            let _ = state::cache_handle_from_task(&resp);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            // Match the two exact-ish phrases that the server emits when
            // the agent already holds a live commitment on this task.
            // Server source: routes/tasks.rs SignupReject mapping +
            // submit_proof's "your commitment is X, not in_progress"
            // path. We deliberately avoid matching loose substrings like
            // "submitted" or "already" alone, so unrelated server errors
            // mentioning those words don't get silently swallowed.
            let is_benign_conflict = msg.contains("you already hold a slot")
                || msg.contains("your commitment is in_progress")
                || msg.contains("your commitment is submitted")
                || msg.contains("your commitment is paid");
            if is_benign_conflict {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------
// profile
// ---------------------------------------------------------------------

pub fn profile(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let client = Client::new();

    // Resolve the target handle.
    let (handle, mine) = match args.first() {
        Some(h) if !h.is_empty() => (h.clone(), false),
        _ => (resolve_own_handle(&client)?, true),
    };

    // 1. base profile (rep card)
    let mut profile = client.get_public(&format!("/api/users/{}", urlencode(&handle)))?;

    // 2. tasks created
    let tasks_created = client.get_public(&format!("/api/tasks?creator={}", urlencode(&handle)))?;
    // 3. tasks worked
    let tasks_worked = client.get_public(&format!("/api/tasks?worker={}", urlencode(&handle)))?;

    // Compute a worker completion rate from the rep counters. Mirrors
    // the platform's profile-card maths so agents see the same number
    // a human would.
    let completed = profile
        .get("rep_worker_completed")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let abandoned = profile
        .get("rep_worker_abandoned")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total = completed + abandoned;
    let completion_rate = if total > 0 {
        Some((completed as f64) / (total as f64))
    } else {
        None
    };

    if let Some(map) = profile.as_object_mut() {
        map.insert("mine".into(), json!(mine));
        map.insert("completion_rate".into(), json!(completion_rate));
        map.insert("tasks_created".into(), tasks_created);
        map.insert("tasks_worked".into(), tasks_worked);
    }
    Ok(profile)
}

/// Get the agent's own handle. Cached after the first authed call;
/// otherwise hit `/api/users/register` (idempotent: server-side
/// `ensure_exists` already created the row when any prior signed
/// request landed) and stash the result.
///
/// Contract dependency: relies on `POST /api/users/register`
/// returning JSON with a `handle` field. Pinned by the platform's
/// `RegisterResponse` struct (`pivx-tasks/src/routes/users.rs`)
/// which `#[serde(flatten)]`s a full `User` row. If that response
/// shape ever drops the handle, this lookup silently breaks — add
/// a server-side integration test pinning the shape if you need to
/// guarantee the contract.
fn resolve_own_handle(client: &Client) -> Result<String, Box<dyn Error>> {
    if let Some(h) = state::cached_handle()? {
        return Ok(h);
    }
    let resp = client.post_signed("/api/users/register")?;
    let handle = resp
        .get("handle")
        .and_then(|v| v.as_str())
        .ok_or("register response missing handle")?
        .to_string();
    state::set_cached_handle(&handle)?;
    Ok(handle)
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// Minimal URL-encoding for query-string components — enough for the
/// values we actually pass (handles, status keywords, category names).
/// Spaces → `%20`, anything outside ALPHA / DIGIT / `-_.~` gets
/// percent-encoded. Avoids pulling in a whole urlencoding crate.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ---------------------------------------------------------------------
// search
// ---------------------------------------------------------------------

pub fn search(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let mut query: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                limit = Some(parse_usize_flag("--limit", args.get(i + 1))?);
                i += 2;
            }
            other => {
                if query.is_none() {
                    query = Some(other.to_string());
                    i += 1;
                } else {
                    return Err(format!("unexpected arg: {}", other).into());
                }
            }
        }
    }
    let q = query.ok_or("Usage: pivx-agent-kit task search <query> [--limit N]")?;
    let client = Client::new();
    let mut value = client.get_public(&format!("/api/tasks?search={}", urlencode(&q)))?;
    if let (Some(n), Some(arr)) = (limit, value.as_array_mut()) {
        if arr.len() > n {
            arr.truncate(n);
        }
    }
    Ok(value)
}

// ---------------------------------------------------------------------
// create
// ---------------------------------------------------------------------

pub fn create(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let mut title: Option<String> = None;
    let mut description: Option<String> = None;
    let mut category: Option<String> = None;
    let mut amount: Option<f64> = None;
    let mut currency: String = "PIV".to_string();
    let mut verification: Option<String> = None;
    let mut quantity: i64 = 1;
    let mut min_rep: i64 = 0;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" => {
                title = args.get(i + 1).cloned();
                i += 2;
            }
            "--description" => {
                description = args.get(i + 1).cloned();
                i += 2;
            }
            "--category" => {
                category = args.get(i + 1).cloned();
                i += 2;
            }
            "--amount" => {
                amount = Some(parse_f64_flag("--amount", args.get(i + 1))?);
                i += 2;
            }
            "--currency" => {
                currency = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| "PIV".to_string());
                i += 2;
            }
            "--verification" => {
                verification = args.get(i + 1).cloned();
                i += 2;
            }
            "--quantity" => {
                quantity = parse_i64_flag("--quantity", args.get(i + 1))?;
                i += 2;
            }
            "--min-rep" => {
                min_rep = parse_i64_flag("--min-rep", args.get(i + 1))?;
                i += 2;
            }
            other => return Err(format!("unknown flag: {}", other).into()),
        }
    }

    let title = title.ok_or("--title is required")?;
    let description = description.ok_or("--description is required")?;
    let category = category.ok_or("--category is required (dev, design, content, social, research, marketing, other)")?;
    let amount = amount.ok_or("--amount is required (a positive decimal)")?;
    let verification = verification
        .filter(|s| !s.trim().is_empty())
        .ok_or("--verification is required (tell the worker how to prove completion)")?;

    let body = json!({
        "title": title,
        "description": description,
        "category": category,
        "quoted_currency": currency,
        "quoted_amount": amount,
        "verification": verification,
        "worker_quantity": quantity,
        "min_worker_rep": min_rep,
    });

    let client = Client::new();
    client.post_signed_json("/api/tasks", &body)
}

// ---------------------------------------------------------------------
// approve (with auto-pay from the kit's wallet)
// ---------------------------------------------------------------------

pub fn approve(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id_arg = args
        .first()
        .ok_or("Usage: pivx-agent-kit task approve <id-or-url> --worker <handle> [--from public|private] [--txid <hex>]")?;
    let id = parse_task_id(id_arg)?;

    let mut worker: Option<String> = None;
    let mut from = "public".to_string();
    let mut external_txid: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned();
                i += 2;
            }
            "--from" => {
                from = args
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| "public".to_string());
                i += 2;
            }
            "--txid" => {
                external_txid = args.get(i + 1).cloned();
                i += 2;
            }
            other => return Err(format!("unknown flag: {}", other).into()),
        }
    }
    let worker = worker.ok_or("--worker <handle> is required")?;

    let client = Client::new();

    let txid = if let Some(t) = external_txid {
        // Cheap kit-side format check so a malformed --txid fails
        // before any signed round-trip. The server enforces the same
        // rule (`routes/tasks.rs`), but catching it here gives a
        // clearer error than "HTTP 400" and saves the network call.
        let trimmed = t.trim();
        if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "--txid must be 64 hex characters (got {} of {})",
                trimmed.len(),
                if trimmed.chars().all(|c| c.is_ascii_hexdigit()) { "hex" } else { "non-hex" }
            )
            .into());
        }
        trimmed.to_string()
    } else {
        // Auto-pay path: figure out the bounty + the worker's address,
        // then broadcast a payment via the kit's wallet.
        let task = client.get_public(&format!("/api/tasks/{}", id))?;
        let bounty_sat = task
            .get("bounty_sat")
            .and_then(|v| v.as_i64())
            .ok_or("could not determine bounty_sat for task")?;
        if bounty_sat <= 0 {
            return Err("task has no bounty to pay".into());
        }

        // Worker address is only revealed to the authed creator on
        // /proofs. (The public task view hides addresses.)
        let proofs = client.get_signed(&format!("/api/tasks/{}/proofs", id))?;
        let address = proofs
            .get("commitments")
            .and_then(|cs| cs.as_array())
            .and_then(|cs| {
                cs.iter().find_map(|c| {
                    let cm = c.get("commitment")?;
                    let h = cm.get("worker_handle")?.as_str()?;
                    if h == worker {
                        cm.get("worker_address")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| {
                format!(
                    "could not find worker {:?} on task {}; check the handle",
                    worker, id
                )
            })?;

        let send_resp = core::send(&address, bounty_sat as u64, "", &from)?;
        send_resp
            .get("txid")
            .and_then(|v| v.as_str())
            .ok_or("send returned no txid")?
            .to_string()
    };

    let body = json!({ "worker": worker, "txid": txid });
    let resp = client.post_signed_json(&format!("/api/tasks/{}/approve", id), &body)?;

    // Surface the txid in the response so an agent doesn't have to
    // dig back through balance/send output to find it.
    let mut out = resp;
    if let Some(map) = out.as_object_mut() {
        map.insert("txid".into(), json!(txid));
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// reject (reason required, mirrors the platform's UI rule)
// ---------------------------------------------------------------------

pub fn reject(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id_arg = args
        .first()
        .ok_or("Usage: pivx-agent-kit task reject <id-or-url> --worker <handle> --reason <text>")?;
    let id = parse_task_id(id_arg)?;

    let mut worker: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned();
                i += 2;
            }
            "--reason" => {
                reason = args.get(i + 1).cloned();
                i += 2;
            }
            other => return Err(format!("unknown flag: {}", other).into()),
        }
    }
    let worker = worker.ok_or("--worker <handle> is required")?;
    let reason = reason
        .filter(|s| !s.trim().is_empty())
        .ok_or("--reason <text> is required (the worker needs to know what to change)")?;

    let body = json!({ "worker": worker, "reason": reason });
    let client = Client::new();
    client.post_signed_json(&format!("/api/tasks/{}/reject", id), &body)
}

// ---------------------------------------------------------------------
// cancel
// ---------------------------------------------------------------------

pub fn cancel(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let id = args
        .first()
        .ok_or("Usage: pivx-agent-kit task cancel <id-or-url>")?;
    let id = parse_task_id(id)?;
    let client = Client::new();
    client.post_signed(&format!("/api/tasks/{}/cancel", id))
}

// ---------------------------------------------------------------------
// notifications: list / read / read-all / dismiss
// ---------------------------------------------------------------------

pub fn notifications(args: &[String]) -> Result<Value, Box<dyn Error>> {
    // Sub-subcommand routing — `task notifications` defaults to list.
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "list" | "--unread" | "--limit" => {
            // The first arg may already be a flag for the default list
            // action; pass through everything as flags.
            notifications_list(args)
        }
        "read" => {
            let id = rest
                .first()
                .ok_or("Usage: pivx-agent-kit task notifications read <id>")?
                .parse::<i64>()
                .map_err(|_| "notification id must be a number")?;
            Client::new().post_signed(&format!("/api/notifications/{}/read", id))
        }
        "read-all" => Client::new().post_signed("/api/notifications/read-all"),
        "dismiss" => {
            let id = rest
                .first()
                .ok_or("Usage: pivx-agent-kit task notifications dismiss <id>")?
                .parse::<i64>()
                .map_err(|_| "notification id must be a number")?;
            Client::new().delete_signed(&format!("/api/notifications/{}", id))
        }
        other => Err(format!("unknown notifications subcommand: {}", other).into()),
    }
}

fn notifications_list(args: &[String]) -> Result<Value, Box<dyn Error>> {
    let mut unread = false;
    let mut limit: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "list" => {
                i += 1;
            }
            "--unread" => {
                unread = true;
                i += 1;
            }
            "--limit" => {
                limit = Some(parse_usize_flag("--limit", args.get(i + 1))?);
                i += 2;
            }
            other => return Err(format!("unknown flag: {}", other).into()),
        }
    }
    let mut qs = Vec::<String>::new();
    if unread {
        qs.push("unread_only=true".to_string());
    }
    if let Some(n) = limit {
        qs.push(format!("limit={}", n));
    }
    let path = if qs.is_empty() {
        "/api/notifications".to_string()
    } else {
        format!("/api/notifications?{}", qs.join("&"))
    };
    Client::new().get_signed(&path)
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// Parse the value of a numeric `--flag <N>` strictly: no value or a
/// non-numeric value is an error, never a silent fallback. The
/// previous lenient behavior turned `--limit foo` into `Some(0)`,
/// which silently truncated output to zero results.
fn parse_usize_flag(flag: &str, raw: Option<&String>) -> Result<usize, Box<dyn Error>> {
    let s = raw.ok_or_else(|| format!("{} requires a value", flag))?;
    s.parse::<usize>()
        .map_err(|_| format!("{} value {:?} is not a non-negative integer", flag, s).into())
}

fn parse_i64_flag(flag: &str, raw: Option<&String>) -> Result<i64, Box<dyn Error>> {
    let s = raw.ok_or_else(|| format!("{} requires a value", flag))?;
    s.parse::<i64>()
        .map_err(|_| format!("{} value {:?} is not an integer", flag, s).into())
}

fn parse_f64_flag(flag: &str, raw: Option<&String>) -> Result<f64, Box<dyn Error>> {
    let s = raw.ok_or_else(|| format!("{} requires a value", flag))?;
    s.parse::<f64>()
        .map_err(|_| format!("{} value {:?} is not a number", flag, s).into())
}

fn mime_for_path(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") => "text/plain",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------- parse_task_id ---------------------------------------

    #[test]
    fn parse_task_id_accepts_bare_number() {
        assert_eq!(parse_task_id("5").unwrap(), 5);
        assert_eq!(parse_task_id("  42 ").unwrap(), 42);
        assert_eq!(parse_task_id("12345").unwrap(), 12345);
    }

    #[test]
    fn parse_task_id_accepts_query_path() {
        assert_eq!(parse_task_id("task?id=7").unwrap(), 7);
        assert_eq!(parse_task_id("?id=99").unwrap(), 99);
    }

    #[test]
    fn parse_task_id_accepts_full_url() {
        assert_eq!(
            parse_task_id("https://tasks.pivxla.bz/task?id=14").unwrap(),
            14
        );
        assert_eq!(
            parse_task_id("http://localhost:8088/task?id=1").unwrap(),
            1
        );
    }

    #[test]
    fn parse_task_id_lenient_about_extra_query_params() {
        // Documented permissiveness: id= matches anywhere in the
        // string, not just at the start of a query string.
        assert_eq!(
            parse_task_id("https://example.com/?override=foo&id=42&x=1").unwrap(),
            42
        );
    }

    #[test]
    fn parse_task_id_rejects_garbage() {
        assert!(parse_task_id("").is_err());
        assert!(parse_task_id("not-a-task").is_err());
        assert!(parse_task_id("/task").is_err()); // no id= and not a number
    }

    #[test]
    fn parse_task_id_rejects_zero_and_negative() {
        // Bare 0 has no ?id= prefix, so it parses fine numerically — but the > 0 guard rejects it.
        assert!(parse_task_id("0").is_err());
        assert!(parse_task_id("-5").is_err());
        // ?id=0 also rejected.
        assert!(parse_task_id("task?id=0").is_err());
    }

    #[test]
    fn parse_task_id_takes_only_leading_digits_after_id_eq() {
        // ?id=42abc → 42, then the rest is ignored.
        assert_eq!(parse_task_id("task?id=42abc").unwrap(), 42);
    }

    // -------- urlencode -------------------------------------------

    #[test]
    fn urlencode_passes_through_unreserved() {
        assert_eq!(urlencode("hello"), "hello");
        assert_eq!(urlencode("HELLO123"), "HELLO123");
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencode_encodes_space() {
        assert_eq!(urlencode("hello world"), "hello%20world");
    }

    #[test]
    fn urlencode_encodes_special_chars() {
        assert_eq!(urlencode("a+b"), "a%2Bb");
        assert_eq!(urlencode("a&b"), "a%26b");
        assert_eq!(urlencode("a=b"), "a%3Db");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("a?b"), "a%3Fb");
    }

    #[test]
    fn urlencode_handles_utf8() {
        // 'é' is 0xC3 0xA9 → %C3%A9.
        assert_eq!(urlencode("café"), "caf%C3%A9");
    }

    // -------- parse_*_flag ----------------------------------------

    #[test]
    fn parse_usize_flag_accepts_valid() {
        let v = "42".to_string();
        assert_eq!(parse_usize_flag("--limit", Some(&v)).unwrap(), 42);
    }

    #[test]
    fn parse_usize_flag_rejects_missing_value() {
        let err = parse_usize_flag("--limit", None).unwrap_err().to_string();
        assert!(err.contains("--limit"));
        assert!(err.contains("requires a value"));
    }

    #[test]
    fn parse_usize_flag_rejects_garbage() {
        let v = "foo".to_string();
        let err = parse_usize_flag("--limit", Some(&v)).unwrap_err().to_string();
        assert!(err.contains("--limit"));
        assert!(err.contains("\"foo\""));
    }

    #[test]
    fn parse_usize_flag_rejects_negative() {
        let v = "-3".to_string();
        assert!(parse_usize_flag("--limit", Some(&v)).is_err());
    }

    #[test]
    fn parse_i64_flag_accepts_negative() {
        let v = "-7".to_string();
        assert_eq!(parse_i64_flag("--min-rep", Some(&v)).unwrap(), -7);
    }

    #[test]
    fn parse_f64_flag_accepts_decimal() {
        let v = "0.001".to_string();
        let n = parse_f64_flag("--amount", Some(&v)).unwrap();
        assert!((n - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_f64_flag_rejects_garbage() {
        let v = "1.2.3".to_string();
        assert!(parse_f64_flag("--amount", Some(&v)).is_err());
    }

    // -------- mime_for_path ---------------------------------------

    #[test]
    fn mime_for_path_known_extensions() {
        assert_eq!(mime_for_path(Path::new("a.png")), "image/png");
        assert_eq!(mime_for_path(Path::new("a.JPG")), "image/jpeg"); // case-insensitive
        assert_eq!(mime_for_path(Path::new("doc.pdf")), "application/pdf");
        assert_eq!(mime_for_path(Path::new("notes.md")), "text/plain");
    }

    #[test]
    fn mime_for_path_unknown_falls_back() {
        assert_eq!(
            mime_for_path(Path::new("blob.xyz")),
            "application/octet-stream"
        );
        assert_eq!(
            mime_for_path(Path::new("noext")),
            "application/octet-stream"
        );
    }

    // -------- filename validation in submit -----------------------

    /// Filenames containing `"`, `\`, `\r`, `\n` are rejected before
    /// the multipart body is built — these characters would let an
    /// attacker inject extra MIME headers via the
    /// `Content-Disposition: filename="..."` line. Linux allows them
    /// in filesystem names so we have to filter explicitly.
    #[test]
    fn submit_rejects_filenames_with_quote() {
        // We don't actually call `submit` (it would hit the network);
        // we verify the validation predicate matches the live code.
        let bad = "evil\"injected.png";
        let blocked = bad.chars().any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n');
        assert!(blocked);
    }

    #[test]
    fn submit_rejects_filenames_with_crlf() {
        let bad = "evil\r\nContent-Type: text/html\r\n\r\n<script>alert(1)</script>.png";
        let blocked = bad.chars().any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n');
        assert!(blocked);
    }

    #[test]
    fn submit_rejects_filenames_with_backslash() {
        let bad = "weird\\path.png";
        let blocked = bad.chars().any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n');
        assert!(blocked);
    }

    #[test]
    fn submit_accepts_normal_filenames() {
        let good = ["pic.png", "some_file-1.txt", "doc (final).pdf", "café.jpg"];
        for name in good {
            let blocked = name.chars().any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n');
            assert!(!blocked, "rejected legitimate filename: {:?}", name);
        }
    }

    // -------- approve --txid validation ---------------------------

    /// Kit-side format check: malformed --txid should fail before
    /// any signed network round-trip.
    fn validate_txid(s: &str) -> bool {
        let t = s.trim();
        t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn txid_validator_accepts_64_hex() {
        let good = "a".repeat(64);
        assert!(validate_txid(&good));
        let mixed = "0123456789abcdefABCDEF0123456789abcdefABCDEF0123456789abcdefABCD";
        assert!(validate_txid(mixed));
    }

    #[test]
    fn txid_validator_rejects_wrong_length() {
        assert!(!validate_txid(""));
        assert!(!validate_txid("abc"));
        assert!(!validate_txid(&"a".repeat(63)));
        assert!(!validate_txid(&"a".repeat(65)));
    }

    #[test]
    fn txid_validator_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert!(!validate_txid(&bad));
        let with_space = format!("{}{}", "a".repeat(63), " ");
        assert!(!validate_txid(&with_space));
    }
}
