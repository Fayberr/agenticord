//! Discord's REST API, called the way the web client calls it.
//! See docs/protocol.md section 2.

use crate::{config, debug, fingerprint::{self, Fingerprint}, info, util};
use anyhow::{Context, Result, anyhow};
use reqwest::{
    Client, Method,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::Duration,
};
use tokio::{sync::{Mutex, OnceCell}, time::Instant};

pub const API: &str = "https://discord.com/api/v9";
const WEB_APP: &str = "https://discord.com/app";
/// The gap kept between two requests, so a burst of tool calls does not look
/// like a script.
const MIN_GAP: Duration = Duration::from_millis(120);
const MAX_ATTEMPTS: usize = 5;

/// An error Discord answered with.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: Option<u64>,
    pub message: String,
    pub captcha: bool,
    /// What a browser needs to solve the captcha, when Discord sent it.
    pub challenge: Option<Challenge>,
}

/// A captcha Discord wants solved before it repeats a request.
#[derive(Debug, Clone)]
pub struct Challenge {
    pub service: String,
    pub sitekey: String,
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
}

impl Challenge {
    fn from_json(json: &Value) -> Option<Self> {
        Some(Self {
            service: json["captcha_service"].as_str().unwrap_or("hcaptcha").to_owned(),
            sitekey: json["captcha_sitekey"].as_str()?.to_owned(),
            rqdata: json["captcha_rqdata"].as_str().map(str::to_owned),
            rqtoken: json["captcha_rqtoken"].as_str().map(str::to_owned),
        })
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord answered {}", self.status)?;
        if let Some(code) = self.code {
            write!(f, " (code {code})")?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        if self.captcha {
            write!(f, " [captcha required]")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

pub struct Discord {
    http: Client,
    token: Option<String>,
    pub fp: Fingerprint,
    headers: HeaderMap,
    last: Mutex<Instant>,
    me: OnceCell<Value>,
    /// Set once Discord answers 401: the token no longer works.
    pub dead: AtomicBool,
}

fn header(name: &'static str, value: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(name),
        HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("")),
    )
}

impl Discord {
    /// Sets up HTTPS, loads Discord's web page for its cookies and current
    /// build number, and returns a client acting as `token` (or anonymous).
    pub async fn connect(token: Option<String>) -> Result<Arc<Self>> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(fingerprint::user_agent())
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .context("could not set up HTTPS")?;
        let build = learn_build(&http).await;
        let fp = Fingerprint::new(build);
        let mut headers = HeaderMap::new();
        for (name, value) in [
            header("accept", "*/*"),
            header("accept-language", "en-US,en;q=0.9"),
            header("origin", "https://discord.com"),
            header("referer", "https://discord.com/channels/@me"),
            header("x-discord-locale", "en-US"),
            header("x-discord-timezone", &fingerprint::timezone()),
            header("x-debug-options", "bugReporterEnabled"),
            header("x-super-properties", &fp.super_properties()),
            header("sec-fetch-dest", "empty"),
            header("sec-fetch-mode", "cors"),
            header("sec-fetch-site", "same-origin"),
        ] {
            headers.insert(name, value);
        }
        if let Some(token) = &token {
            let mut value = HeaderValue::from_str(token).context("the token has odd characters")?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("authorization"), value);
        }
        Ok(Arc::new(Self {
            http,
            token,
            fp,
            headers,
            last: Mutex::new(Instant::now() - MIN_GAP),
            me: OnceCell::new(),
            dead: AtomicBool::new(false),
        }))
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    async fn throttle(&self) {
        let mut last = self.last.lock().await;
        let wait = MIN_GAP.saturating_sub(last.elapsed());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        *last = Instant::now();
    }

    /// One API call. Waits out rate limits and a search index that is not
    /// ready yet; returns `Value::Null` for an empty answer.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        self.call_with(method, path, query, body, &[]).await
    }

    /// Like `call`, with extra request headers (used to hand back a solved captcha).
    pub async fn call_with(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        extra: &[(&'static str, String)],
    ) -> Result<Value> {
        for attempt in 1..=MAX_ATTEMPTS {
            self.throttle().await;
            let mut request = self
                .http
                .request(method.clone(), format!("{API}{path}"))
                .headers(self.headers.clone());
            for (name, value) in extra {
                if let Ok(value) = HeaderValue::from_str(value) {
                    request = request.header(HeaderName::from_static(name), value);
                }
            }
            if !query.is_empty() {
                request = request.query(query);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            debug!("{method} {path}");
            let response = request.send().await.context("the request to Discord failed")?;
            let status = response.status();
            let header_wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok());
            let text = response.text().await.context("could not read Discord's answer")?;
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

            let retry_after = json["retry_after"].as_f64().or(header_wait);
            let retryable = status.as_u16() == 429
                || (status.as_u16() == 202 && json["code"].as_u64() == Some(110000));
            if retryable && attempt < MAX_ATTEMPTS {
                let wait = retry_after.unwrap_or(2.0).clamp(0.2, 60.0) + 0.25;
                info!("Discord asked to wait {wait:.1}s ({path})");
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            if status.is_success() {
                return Ok(json);
            }
            if status.as_u16() == 401 {
                self.dead.store(true, Ordering::Relaxed);
            }
            let captcha = json["captcha_key"].is_array() || json["captcha_sitekey"].is_string();
            return Err(ApiError {
                status: status.as_u16(),
                code: json["code"].as_u64(),
                message: json["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| util::clip(&text, 300)),
                captcha,
                challenge: if captcha { Challenge::from_json(&json) } else { None },
            }
            .into());
        }
        Err(anyhow!("Discord kept asking to wait; gave up on {path}"))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.call(Method::POST, path, &[], Some(body)).await
    }

    pub async fn post_with(&self, path: &str, body: &Value, extra: &[(&'static str, String)]) -> Result<Value> {
        self.call_with(Method::POST, path, &[], Some(body), extra).await
    }

    /// The account this token belongs to, asked once.
    pub async fn me(&self) -> Result<&Value> {
        self.me
            .get_or_try_init(|| async { self.get("/users/@me", &[]).await })
            .await
    }

    pub async fn my_id(&self) -> Result<String> {
        Ok(self.me().await?["id"].as_str().unwrap_or_default().to_owned())
    }

    /// Sends a message with one file attached, read from `path`.
    pub async fn send_file(
        &self,
        channel: &str,
        path: &Path,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<Value> {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("could not read {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_owned();
        let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "mp4" => "video/mp4",
            "txt" | "md" | "log" => "text/plain",
            "json" => "application/json",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
        let mut payload = json!({
            "content": content,
            "nonce": util::nonce(),
            "tts": false,
            "flags": 0,
            "attachments": [{"id": 0, "filename": name}],
        });
        if let Some(reply) = reply_to {
            payload["message_reference"] = json!({"message_id": reply, "channel_id": channel});
        }
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(name)
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string())
            .part("files[0]", part);
        self.throttle().await;
        let response = self
            .http
            .post(format!("{API}/channels/{channel}/messages"))
            .headers(self.headers.clone())
            .multipart(form)
            .send()
            .await
            .context("the upload to Discord failed")?;
        let status = response.status();
        let text = response.text().await?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json);
        }
        Err(ApiError {
            status: status.as_u16(),
            code: json["code"].as_u64(),
            message: json["message"].as_str().unwrap_or(&text).to_owned(),
            captcha: false,
            challenge: None,
        }
        .into())
    }
}

/// Loads Discord's web page like a browser, which sets the cookies the API
/// expects, and reads the web client's current build number from it. Falls
/// back to the last number seen, then to a built-in one.
async fn learn_build(http: &Client) -> u64 {
    const READ: usize = 512 << 10;
    let fetch = async {
        let mut response = http
            .get(WEB_APP)
            .header("accept", "text/html,application/xhtml+xml")
            .header("accept-language", "en-US,en;q=0.9")
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none")
            .send()
            .await?
            .error_for_status()?;
        let mut page = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            page.extend_from_slice(&chunk);
            if let Some(number) = fingerprint::find_build_number(&page) {
                return anyhow::Ok(Some(number));
            }
            if page.len() >= READ {
                break;
            }
        }
        Ok(None)
    };
    match tokio::time::timeout(Duration::from_secs(12), fetch).await {
        Ok(Ok(Some(number))) => {
            let _ = config::write_secret("build_number", &number.to_string());
            debug!("web client build number {number}");
            number
        }
        other => {
            let cached = config::read("build_number").and_then(|n| n.parse::<u64>().ok());
            match other {
                Ok(Ok(None)) => info!("the web page had no build number"),
                Ok(Err(error)) => info!("could not read Discord's web page: {error}"),
                _ => info!("Discord's web page did not load in time"),
            }
            cached.unwrap_or(fingerprint::FALLBACK_BUILD)
        }
    }
}
