//! Logging in: by QR code scanned with the phone app (no password, 2FA or
//! token handling), or by pasting a token. See docs/protocol.md section 4.

use crate::{config, discord::{ApiError, Discord}, fingerprint, info};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}};
use futures_util::{SinkExt, StreamExt};
use rsa::{Oaep, RsaPrivateKey, pkcs8::EncodePublicKey, rand_core::OsRng};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::{HeaderValue, header}},
};

const URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";

fn text(value: Value) -> Message {
    Message::text(value.to_string())
}

fn decrypt(key: &RsaPrivateKey, field: &Value) -> Result<Vec<u8>> {
    let encrypted = STANDARD
        .decode(field.as_str().context("Discord's login service sent nothing to read")?)
        .context("could not read Discord's login service")?;
    key.decrypt(Oaep::new::<Sha256>(), &encrypted)
        .map_err(|_| anyhow!("could not decrypt what Discord's login service sent"))
}

/// Prints `url` as a QR code in the terminal: black modules on white, which
/// scans on dark and light terminals alike.
fn print_qr(url: &str) -> Result<()> {
    use qrcode::{Color, QrCode};
    let code = QrCode::new(url.as_bytes())?;
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 2usize;
    let size = width + quiet * 2;
    let dark = |x: usize, y: usize| -> bool {
        x >= quiet
            && y >= quiet
            && x < quiet + width
            && y < quiet + width
            && colors[(y - quiet) * width + (x - quiet)] == Color::Dark
    };
    let mut out = String::new();
    let mut y = 0;
    while y < size {
        for x in 0..size {
            let top = dark(x, y);
            let bottom = y + 1 < size && dark(x, y + 1);
            let fg = if top { 30 } else { 97 };
            let bg = if bottom { 40 } else { 107 };
            out.push_str(&format!("\x1b[{fg};{bg}m\u{2580}"));
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    eprintln!("{out}");
    Ok(())
}

/// Runs the QR login and returns the account's token.
pub async fn qr() -> Result<String> {
    let discord = Discord::connect(None).await?;
    eprintln!("Making a login key...");
    let key = tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut OsRng, 2048))
        .await?
        .context("could not make a key")?;
    let public = key.to_public_key().to_public_key_der().context("could not encode the key")?;

    let mut request = URL.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(header::USER_AGENT, HeaderValue::from_str(fingerprint::user_agent())?);
    headers.insert(header::ORIGIN, HeaderValue::from_static("https://discord.com"));
    let (mut socket, _) = connect_async(request)
        .await
        .context("could not reach Discord's login service")?;

    let mut heartbeat = interval(Duration::from_secs(40));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let deadline = tokio::time::sleep(Duration::from_secs(15 * 60));
    tokio::pin!(deadline);

    loop {
        let message = tokio::select! {
            _ = heartbeat.tick() => {
                socket.send(text(json!({"op": "heartbeat"}))).await?;
                continue;
            }
            _ = &mut deadline => bail!("Timed out waiting for the scan. Run `agenticord login` again."),
            message = socket.next() => message,
        };
        let message = match message {
            Some(Ok(Message::Text(message))) => message,
            Some(Ok(Message::Close(_))) | None => bail!("The code expired. Run `agenticord login` again."),
            Some(Ok(_)) => continue,
            Some(Err(error)) => bail!("Discord's login service dropped the connection: {error}"),
        };
        let message: Value = serde_json::from_str(&message).context("could not understand Discord's login service")?;
        match message["op"].as_str().unwrap_or_default() {
            "hello" => {
                if let Some(ms) = message["heartbeat_interval"].as_u64() {
                    let every = Duration::from_millis(ms.max(1000));
                    heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                }
                let init = json!({"op": "init", "encoded_public_key": STANDARD.encode(public.as_bytes())});
                socket.send(text(init)).await?;
            }
            "nonce_proof" => {
                let nonce = decrypt(&key, &message["encrypted_nonce"])?;
                let proof = URL_SAFE_NO_PAD.encode(Sha256::digest(&nonce));
                socket.send(text(json!({"op": "nonce_proof", "proof": proof}))).await?;
            }
            "pending_remote_init" => {
                let fingerprint = message["fingerprint"].as_str().context("no code from Discord")?;
                let url = format!("https://discord.com/ra/{fingerprint}");
                eprintln!("\nScan this with the Discord phone app:");
                eprintln!("  Settings (your avatar) > Scan QR Code\n");
                if print_qr(&url).is_err() {
                    eprintln!("(could not draw the code, open this on the phone instead)");
                }
                eprintln!("{url}\n");
                eprintln!("Waiting for the scan (the code lasts about two minutes)...");
            }
            "pending_ticket" => {
                let payload = decrypt(&key, &message["encrypted_user_payload"])?;
                let payload = String::from_utf8_lossy(&payload);
                let name = payload.splitn(4, ':').nth(3).unwrap_or("your account");
                eprintln!("Scanned by {name}. Confirm the login on the phone.");
            }
            "pending_login" => {
                let ticket = message["ticket"].as_str().context("no ticket from Discord")?;
                let encrypted = match discord
                    .post("/users/@me/remote-auth/login", &json!({"ticket": ticket}))
                    .await
                {
                    Ok(answer) => answer["encrypted_token"].clone(),
                    Err(error) => {
                        if error.downcast_ref::<ApiError>().is_some_and(|e| e.captcha) {
                            bail!(
                                "Discord wants a captcha for this login, which Agenticord cannot show.\n\
                                 Use `agenticord login --token-stdin` instead."
                            );
                        }
                        return Err(error);
                    }
                };
                let _ = socket.close(None).await;
                let token = String::from_utf8(decrypt(&key, &encrypted)?).context("unreadable token")?;
                if token.is_empty() || token.contains(char::is_whitespace) {
                    bail!("Discord sent a token Agenticord cannot read");
                }
                return Ok(token);
            }
            "cancel" => bail!("The login was cancelled on the phone."),
            _ => {}
        }
    }
}

/// Checks `token` against Discord and saves it. Returns the username.
pub async fn save_token(token: &str) -> Result<String> {
    let token = token.trim().trim_matches('"');
    if token.len() < 20 || token.contains(char::is_whitespace) {
        bail!("that does not look like a Discord token");
    }
    let discord = Discord::connect(Some(token.to_owned())).await?;
    let me = discord.me().await.context("Discord did not accept the token")?;
    let name = me["global_name"]
        .as_str()
        .or_else(|| me["username"].as_str())
        .unwrap_or("unknown")
        .to_owned();
    let handle = me["username"].as_str().unwrap_or("").to_owned();
    config::write_secret("token", token)?;
    info!("saved the login for {name} (@{handle}) in {}", config::dir().display());
    Ok(format!("{name} (@{handle})"))
}
