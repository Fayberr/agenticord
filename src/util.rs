//! Small helpers: ids, time, randomness, percent-encoding, logging.

use anyhow::{Result, bail};
use rsa::rand_core::{OsRng, RngCore};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Discord's epoch, in unix milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// A message nonce: a snowflake made from the current time.
pub fn nonce() -> String {
    (now_ms().saturating_sub(DISCORD_EPOCH_MS) << 22).to_string()
}

pub fn random_u64() -> u64 {
    OsRng.next_u64()
}

/// A random float in `0.0..1.0`.
pub fn random_unit() -> f64 {
    (random_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// `base` scaled by a random factor in `low..high`.
pub fn jitter(base: Duration, low: f64, high: f64) -> Duration {
    base.mul_f64(low + (high - low) * random_unit())
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random v4 UUID with the bits in `cleared` forced to zero.
pub fn uuid(cleared: u128) -> String {
    let mut bits = ((random_u64() as u128) << 64) | random_u64() as u128;
    bits = (bits & !(0xf << 76)) | (0x4 << 76);
    bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    bits &= !cleared;
    let h = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Accepts a snowflake given as a string or a number and returns it as a
/// string, refusing anything that is not digits (these go into URL paths).
pub fn snowflake(value: &serde_json::Value) -> Result<String> {
    let text = match value {
        serde_json::Value::String(s) => s.trim().to_owned(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => bail!("expected an id"),
    };
    if text.len() < 5 || text.len() > 25 || !text.bytes().all(|b| b.is_ascii_digit()) {
        bail!("`{text}` is not a valid Discord id");
    }
    Ok(text)
}

/// Percent-encodes everything except unreserved characters.
pub fn pct(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Cuts `text` to at most `max` characters.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text.to_owned(),
    }
}

pub fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("AGENTICORD_LOG").is_ok_and(|v| v == "debug"))
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { eprintln!("[agenticord] {}", format!($($t)*)) };
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { if $crate::util::debug_enabled() { eprintln!("[agenticord:debug] {}", format!($($t)*)) } };
}
