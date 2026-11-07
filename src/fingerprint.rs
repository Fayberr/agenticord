//! How Agenticord presents itself to Discord: a current desktop Firefox on the
//! web app. See docs/protocol.md section 1.

use crate::util;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{sync::OnceLock, time::SystemTime};

/// Firefox 155 shipped on 2026-09-01, which is day 20697 since the unix epoch.
const FIRST_VERSION: i64 = 155;
const FIRST_DAY: i64 = 20697;
/// A release comes about every 14 days.
const RELEASE_DAYS: i64 = 14;
/// Used when the web app's page cannot be read and nothing is cached.
pub const FALLBACK_BUILD: u64 = 637268;
/// Bits forced to zero in the launch signature: "no client modifications".
const UNMODIFIED_MASK: u128 = 0x0080_1010_0810_0800_2081_0040_0100_0800;

fn firefox_version() -> i64 {
    let days = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(FIRST_DAY);
    // One release behind the newest, as most people's browsers are.
    FIRST_VERSION + (days - FIRST_DAY).max(0) / RELEASE_DAYS - 1
}

pub fn user_agent() -> &'static str {
    static AGENT: OnceLock<String> = OnceLock::new();
    AGENT.get_or_init(|| {
        let v = firefox_version();
        format!("Mozilla/5.0 (X11; Linux x86_64; rv:{v}.0) Gecko/20100101 Firefox/{v}.0")
    })
}

pub fn timezone() -> String {
    std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .or_else(|| std::env::var("TZ").ok().filter(|t| !t.is_empty()))
        .unwrap_or_else(|| "UTC".into())
}

pub struct Fingerprint {
    pub build_number: u64,
    launch_id: String,
    launch_signature: String,
    heartbeat_session: String,
}

impl Fingerprint {
    pub fn new(build_number: u64) -> Self {
        Self {
            build_number,
            launch_id: util::uuid(0),
            launch_signature: util::uuid(UNMODIFIED_MASK),
            heartbeat_session: util::uuid(0),
        }
    }

    /// The `properties` object of IDENTIFY and of `X-Super-Properties`.
    pub fn properties(&self) -> Value {
        let v = firefox_version();
        json!({
            "os": "Linux",
            "browser": "Firefox",
            "device": "",
            "system_locale": "en-US",
            "has_client_mods": false,
            "browser_user_agent": user_agent(),
            "browser_version": format!("{v}.0"),
            "os_version": "",
            "referrer": "",
            "referring_domain": "",
            "referrer_current": "",
            "referring_domain_current": "",
            "release_channel": "stable",
            "client_build_number": self.build_number,
            "client_event_source": null,
            "client_launch_id": self.launch_id,
            "launch_signature": self.launch_signature,
            "client_heartbeat_session_id": self.heartbeat_session,
        })
    }

    pub fn super_properties(&self) -> String {
        STANDARD.encode(self.properties().to_string())
    }
}

/// Reads the build number from the start of the web app's page, where
/// `GLOBAL_ENV` sets `"BUILD_NUMBER":"<digits>"`.
pub fn find_build_number(page: &[u8]) -> Option<u64> {
    const KEY: &[u8] = br#""BUILD_NUMBER":""#;
    let at = page.windows(KEY.len()).position(|w| w == KEY)?;
    let digits = &page[at + KEY.len()..];
    let end = digits
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    std::str::from_utf8(&digits[..end])
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_build_number() {
        assert_eq!(
            find_build_number(br#"GLOBAL_ENV = {"NODE_ENV":"production","BUILD_NUMBER":"637268","X":"y"}"#),
            Some(637268)
        );
        assert_eq!(find_build_number(br#"{"BUILD_NUMBER":""}"#), None);
        assert_eq!(find_build_number(b"nothing here"), None);
    }

    #[test]
    fn signature_has_no_modification_bits() {
        let sig = util::uuid(UNMODIFIED_MASK).replace('-', "");
        let bits = u128::from_str_radix(&sig, 16).unwrap();
        assert_eq!(bits & UNMODIFIED_MASK, 0);
        assert_eq!(&util::uuid(0)[14..15], "4");
    }

    #[test]
    fn user_agent_matches_version() {
        assert!(user_agent().contains(&format!("Firefox/{}.0", firefox_version())));
        assert!(firefox_version() >= 154);
    }
}
