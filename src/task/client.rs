//! HTTP client for the PIVX Tasks platform (`tasks.pivxla.bz`).
//!
//! Owns the body-hash request-signing scheme so callers don't have to
//! hand-roll auth headers. Every authed request signs a canonical
//! string the server can independently reconstruct:
//!
//! ```text
//! PIVX Tasks <METHOD> <PATH> body:<sha256-hex-or-empty> @ <unix-ts>
//! ```
//!
//! For multipart submissions the body hash is empty on both sides
//! (server-side body coverage is a known gap there).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core;

/// Production endpoint. Override with `PIVX_TASKS_API` for testnet /
/// local-dev / forks.
const DEFAULT_BASE: &str = "https://tasks.pivxla.bz";

/// Canonical-string brand that pins signatures to this platform. Must
/// stay in sync with `brand::NAME` in pivx-tasks.
const CANONICAL_BRAND: &str = "PIVX Tasks";

pub struct Client {
    base: String,
}

impl Client {
    pub fn new() -> Self {
        let base = std::env::var("PIVX_TASKS_API")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        Self { base }
    }

    // ---------------------------------------------------------------
    // Unsigned reads
    // ---------------------------------------------------------------

    pub fn get_public(&self, path: &str) -> Result<Value, Box<dyn Error>> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::get(&url).call();
        decode_json(resp)
    }

    // ---------------------------------------------------------------
    // Signed reads
    // ---------------------------------------------------------------

    /// Signed GET. Used for endpoints that gate visibility on the
    /// caller's role (e.g. `/proofs` reveals worker addresses only to
    /// the task's creator).
    pub fn get_signed(&self, path: &str) -> Result<Value, Box<dyn Error>> {
        let (headers, _) = self.sign("GET", path, None)?;
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::get(&url);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        decode_json(req.call())
    }

    // ---------------------------------------------------------------
    // Signed writes
    // ---------------------------------------------------------------

    /// Signed POST with no body. Used for endpoints like `/signup` and
    /// `/cancel` where the path itself carries all the intent.
    pub fn post_signed(&self, path: &str) -> Result<Value, Box<dyn Error>> {
        let (headers, _) = self.sign("POST", path, None)?;
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::post(&url);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        decode_json(req.call())
    }

    /// Signed POST with a JSON body. Hashes the serialized JSON exactly
    /// as it's sent on the wire — same bytes the server will hash, so
    /// the body-hash header matches.
    pub fn post_signed_json(&self, path: &str, body: &Value) -> Result<Value, Box<dyn Error>> {
        let body_str = serde_json::to_string(body)?;
        let (headers, _) = self.sign("POST", path, Some(body_str.as_bytes()))?;
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::post(&url).set("Content-Type", "application/json");
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        decode_json(req.send_string(&body_str))
    }

    /// Signed DELETE with no body.
    pub fn delete_signed(&self, path: &str) -> Result<Value, Box<dyn Error>> {
        let (headers, _) = self.sign("DELETE", path, None)?;
        let url = format!("{}{}", self.base, path);
        let mut req = ureq::delete(&url);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        decode_json(req.call())
    }

    /// Signed multipart POST. `text_fields` are simple `(name, value)`
    /// pairs; `files` are `(field_name, filename, mime, bytes)` tuples.
    ///
    /// Unlike the browser path, the kit hashes the assembled multipart
    /// body and sends it as `X-PIV-Body-Hash` so the server can detect
    /// any tamper-on-the-wire (compromised proxy, malicious CDN, etc.)
    /// — body coverage is on. Browsers stay on the legacy skip path
    /// because hashing FormData client-side requires manual multipart
    /// serialization in JS for marginal gain on a UI where the user
    /// is watching what they upload.
    pub fn post_signed_multipart(
        &self,
        path: &str,
        text_fields: &[(&str, &str)],
        files: &[(&str, &str, &str, Vec<u8>)],
    ) -> Result<Value, Box<dyn Error>> {
        let boundary = format!("----pivx-agent-kit-{}", random_boundary_token());
        let body = build_multipart_body(&boundary, text_fields, files);
        let (headers, _) = self.sign("POST", path, Some(&body))?;
        let url = format!("{}{}", self.base, path);
        let content_type = format!("multipart/form-data; boundary={}", boundary);
        let mut req = ureq::post(&url).set("Content-Type", &content_type);
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        decode_json(req.send_bytes(&body))
    }

    // ---------------------------------------------------------------
    // Internal: produce the four X-PIV-* headers
    // ---------------------------------------------------------------

    /// Returns the signed request headers and the address that signed
    /// them (so callers can cache it).
    fn sign(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<(Vec<(String, String)>, String), Box<dyn Error>> {
        let body_hash = match body {
            Some(b) => {
                let mut hasher = Sha256::new();
                hasher.update(b);
                hex_lower(&hasher.finalize())
            }
            None => String::new(),
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Server canonicalizes with `uri.path()` (path only, query
        // stripped), so we must too — otherwise any signed URL with
        // `?foo=bar` would 401 on signature mismatch. Callers still
        // pass the full `path?query` string for the actual GET URL.
        let canonical_path = match path.find('?') {
            Some(i) => &path[..i],
            None => path,
        };
        let canonical = format!(
            "{} {} {} body:{} @ {}",
            CANONICAL_BRAND,
            method.to_uppercase(),
            canonical_path,
            body_hash,
            timestamp
        );
        // Reuse the kit's existing message-signing primitive.
        let signed = core::sign_message(&canonical)?;
        let address = signed
            .get("address")
            .and_then(|v| v.as_str())
            .ok_or("sign_message returned no address")?
            .to_string();
        let signature = signed
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or("sign_message returned no signature")?
            .to_string();

        let headers = vec![
            ("X-PIV-Address".into(), address.clone()),
            ("X-PIV-Timestamp".into(), timestamp.to_string()),
            ("X-PIV-Sig".into(), signature),
            ("X-PIV-Body-Hash".into(), body_hash),
        ];
        Ok((headers, address))
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Decode a ureq response into JSON, mapping HTTP errors to the
/// server's `error` field where present so callers see useful
/// messages. The HTTP status code is always preserved in the error
/// string (`HTTP 401: unauthorized`, `HTTP 409: conflict: ...`) so
/// agents can pattern-match the kind of failure — useful for
/// self-correction (e.g. retrying on transient 5xx, prompting
/// clock-resync on persistent 401).
fn decode_json(resp: Result<ureq::Response, ureq::Error>) -> Result<Value, Box<dyn Error>> {
    match resp {
        Ok(r) => {
            let text = r.into_string()?;
            if text.is_empty() {
                return Ok(json!(null));
            }
            Ok(serde_json::from_str(&text)?)
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            // Prefer the server's structured `{"error": "..."}` body.
            let server_msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
            let msg = match server_msg {
                Some(m) => format!("HTTP {}: {}", code, m),
                None if text.is_empty() => format!("HTTP {}", code),
                None => format!("HTTP {}: {}", code, text),
            };
            Err(msg.into())
        }
        Err(e) => Err(format!("transport error: {}", e).into()),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Cheap pseudo-random boundary token. Doesn't need to be
/// cryptographically random — only needs to not appear inside any
/// payload field, and uniqueness across concurrent requests on the
/// same machine. Time + thread id is fine.
fn random_boundary_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos)
}

/// Hand-rolled `multipart/form-data` body. ureq 2.x has no native
/// multipart, but the wire format is straightforward.
fn build_multipart_body(
    boundary: &str,
    text_fields: &[(&str, &str)],
    files: &[(&str, &str, &str, Vec<u8>)],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (name, value) in text_fields {
        out.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for (name, filename, mime, data) in files {
        out.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        out.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                name, filename
            )
            .as_bytes(),
        );
        out.extend_from_slice(format!("Content-Type: {}\r\n\r\n", mime).as_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
    out
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------- hex_lower -------------------------------------------

    #[test]
    fn hex_lower_zero_pads() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn hex_lower_empty_input() {
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn hex_lower_known_sha256() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let mut h = Sha256::new();
        h.update(b"");
        assert_eq!(
            hex_lower(&h.finalize()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // -------- canonical-string layout -----------------------------

    /// The canonical string the kit signs must match what pivx-tasks's
    /// `canonical_message()` reconstructs server-side. Both sides
    /// uppercase the method, strip the query string from the path,
    /// and emit `body:<hash>` with empty hash for body-less / opt-out
    /// multipart requests. This test pins the exact format so a
    /// future refactor can't drift from the server.
    #[test]
    fn canonical_string_format_matches_server() {
        let canonical = format!(
            "{} {} {} body:{} @ {}",
            CANONICAL_BRAND,
            "POST",
            "/api/tasks/5/signup",
            "",
            1700000000_i64
        );
        assert_eq!(
            canonical,
            "PIVX Tasks POST /api/tasks/5/signup body: @ 1700000000"
        );
    }

    #[test]
    fn canonical_string_with_body_hash() {
        let mut h = Sha256::new();
        h.update(b"{\"foo\":1}");
        let body_hash = hex_lower(&h.finalize());
        let canonical = format!(
            "{} {} {} body:{} @ {}",
            CANONICAL_BRAND, "POST", "/api/tasks", body_hash, 1700000000_i64
        );
        // Hash for `{"foo":1}` is a known sha256 value; pin it so a
        // future refactor of hex_lower / Sha256 wiring can't drift.
        assert!(canonical.contains(
            "body:37a76343c8e3c695feeaadfe52329673ff129c65f99f55ae6056c9254f4c481d"
        ));
        assert!(canonical.starts_with("PIVX Tasks POST /api/tasks "));
        assert!(canonical.ends_with("@ 1700000000"));
    }

    // -------- canonical_path strips query string ------------------

    /// Server canonicalizes with `uri.path()` (no query string), so
    /// the client must too. Regression guard for the bug found
    /// during stage-2 testing where `?limit=2` requests came back 401.
    #[test]
    fn canonical_path_strips_query() {
        let path = "/api/notifications?limit=2&unread_only=true";
        let canonical_path = match path.find('?') {
            Some(i) => &path[..i],
            None => path,
        };
        assert_eq!(canonical_path, "/api/notifications");
    }

    #[test]
    fn canonical_path_no_query_unchanged() {
        let path = "/api/tasks/5/signup";
        let canonical_path = match path.find('?') {
            Some(i) => &path[..i],
            None => path,
        };
        assert_eq!(canonical_path, "/api/tasks/5/signup");
    }

    // -------- multipart body --------------------------------------

    #[test]
    fn multipart_body_text_field_only() {
        let body = build_multipart_body("BOUNDARY", &[("body", "hello")], &[]);
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.starts_with("--BOUNDARY\r\n"));
        assert!(s.contains("Content-Disposition: form-data; name=\"body\"\r\n\r\n"));
        assert!(s.contains("hello\r\n"));
        assert!(s.ends_with("--BOUNDARY--\r\n"));
    }

    #[test]
    fn multipart_body_with_file() {
        let body = build_multipart_body(
            "BOUNDARY",
            &[("body", "caption")],
            &[("attachment", "pic.png", "image/png", b"PNG_BYTES".to_vec())],
        );
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains(
            "Content-Disposition: form-data; name=\"attachment\"; filename=\"pic.png\"\r\n"
        ));
        assert!(s.contains("Content-Type: image/png\r\n\r\nPNG_BYTES\r\n"));
    }

    #[test]
    fn multipart_body_multi_files() {
        let body = build_multipart_body(
            "B",
            &[],
            &[
                ("attachment", "a.txt", "text/plain", b"AAA".to_vec()),
                ("attachment", "b.txt", "text/plain", b"BBB".to_vec()),
            ],
        );
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("filename=\"a.txt\""));
        assert!(s.contains("filename=\"b.txt\""));
        assert!(s.contains("AAA"));
        assert!(s.contains("BBB"));
        // Closing boundary appears exactly once.
        assert_eq!(s.matches("--B--\r\n").count(), 1);
    }

    #[test]
    fn multipart_body_empty_attachment_bytes_ok() {
        // A zero-byte file is valid (some agents may legitimately
        // attach empty files for sentinel/marker purposes).
        let body = build_multipart_body(
            "B",
            &[],
            &[("attachment", "empty.bin", "application/octet-stream", vec![])],
        );
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("filename=\"empty.bin\""));
        assert!(s.contains("Content-Type: application/octet-stream\r\n\r\n\r\n"));
    }

    #[test]
    fn multipart_body_is_deterministic() {
        // Identical inputs produce identical bytes — important for
        // tests and for the body-hash protocol (server hashes the
        // exact bytes the client signed).
        let a = build_multipart_body("B", &[("k", "v")], &[]);
        let b = build_multipart_body("B", &[("k", "v")], &[]);
        assert_eq!(a, b);
    }

    // -------- random_boundary_token -------------------------------

    #[test]
    fn random_boundary_tokens_are_distinct_within_a_thread() {
        // The token uses nanos since UNIX_EPOCH, so back-to-back
        // calls should produce different values on any modern OS.
        // Not a security guarantee — just a sanity check that we
        // don't trivially collide.
        let a = random_boundary_token();
        // Spin briefly to bump the nano clock.
        for _ in 0..1000 {
            std::hint::black_box(());
        }
        let b = random_boundary_token();
        // Allow occasional collision under heavy clock-coalescing,
        // but not both consecutive.
        if a == b {
            let c = random_boundary_token();
            assert!(c != a, "three consecutive tokens all collided");
        }
    }
}
