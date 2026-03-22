//! Self-updater: checks GitHub releases, downloads, verifies SHA256, and replaces the running binary.

use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

const REPO: &str = "PIVX-Labs/pivx-agent-kit";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn platform_name() -> Result<&'static str, Box<dyn Error>> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        ("windows", "x86_64") => Ok("windows-x86_64"),
        (os, arch) => Err(format!("Unsupported platform: {}-{}", os, arch).into()),
    }
}

fn fetch_latest_tag() -> Result<String, Box<dyn Error>> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let body = ureq::get(&url)
        .set("User-Agent", "pivx-agent-kit-updater")
        .call()?
        .into_string()?;
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    parsed["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No tag_name in release".into())
}

fn download(tag: &str, filename: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        REPO, tag, filename
    );
    let mut bytes = Vec::new();
    ureq::get(&url)
        .set("User-Agent", "pivx-agent-kit-updater")
        .call()?
        .into_reader()
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn find_checksum(checksums_txt: &str, filename: &str) -> Option<String> {
    for line in checksums_txt.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1].trim_start_matches('*') == filename {
            return Some(parts[0].to_lowercase());
        }
    }
    None
}

fn extract_tar_gz(archive: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut child = Command::new("tar")
        .args(["xzf", "-", "-O", "pivx-agent-kit"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child.stdin.take().unwrap().write_all(archive)?;
    let output = child.wait_with_output()?;

    if output.status.success() && !output.stdout.is_empty() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "Extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

pub fn update() -> Result<serde_json::Value, Box<dyn Error>> {
    let platform = platform_name()?;

    eprintln!("Checking for updates...");
    let tag = fetch_latest_tag()?;
    let latest = tag.trim_start_matches('v');

    if latest == CURRENT_VERSION {
        return Ok(serde_json::json!({
            "status": "up_to_date",
            "version": CURRENT_VERSION
        }));
    }

    eprintln!("Update available: v{} → {}", CURRENT_VERSION, tag);

    let archive_name = format!("pivx-agent-kit-{}.tar.gz", platform);

    // Download and verify checksums
    eprintln!("Verifying release integrity...");
    let checksums = String::from_utf8(download(&tag, "checksums.txt")?)?;
    let expected = find_checksum(&checksums, &archive_name)
        .ok_or_else(|| format!("No checksum for {}", archive_name))?;

    eprintln!("Downloading {}...", archive_name);
    let archive = download(&tag, &archive_name)?;

    let actual = crate::simd::hex::bytes_to_hex_string(&Sha256::digest(&archive));
    if actual != expected {
        return Err(format!(
            "SHA256 mismatch — download may be corrupted or tampered.\n  expected: {}\n  got:      {}",
            expected, actual
        )
        .into());
    }

    // Extract and replace
    eprintln!("Installing...");
    let binary = extract_tar_gz(&archive)?;

    let exe = std::env::current_exe()?;
    let tmp = exe.with_extension("tmp");

    fs::write(&tmp, &binary)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }

    fs::rename(&tmp, &exe)?;

    Ok(serde_json::json!({
        "status": "updated",
        "from": format!("v{}", CURRENT_VERSION),
        "to": tag
    }))
}
