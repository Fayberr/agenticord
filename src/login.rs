//! Logging in: by QR code scanned with the phone app (no password, 2FA or
//! token handling), or by pasting a token. See docs/protocol.md section 4.

use crate::{config, discord::{ApiError, Challenge, Discord}, fingerprint, info};
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

/// Trades the scan's ticket for the encrypted token. When Discord asks for a
/// captcha, a browser has to solve it (see `solve`), then the call is repeated.
async fn exchange(discord: &Discord, ticket: &str) -> Result<Value> {
    let path = "/users/@me/remote-auth/login";
    let body = json!({"ticket": ticket});
    let error = match discord.post(path, &body).await {
        Ok(answer) => return Ok(answer["encrypted_token"].clone()),
        Err(error) => error,
    };
    let Some(challenge) = error.downcast_ref::<ApiError>().and_then(|e| e.challenge.clone()) else {
        return Err(error);
    };
    let key = solve(&challenge).await?;
    let mut extra = vec![("x-captcha-key", key)];
    if let Some(rqtoken) = challenge.rqtoken {
        extra.push(("x-captcha-rqtoken", rqtoken));
    }
    match discord.post_with(path, &body, &extra).await {
        Ok(answer) => Ok(answer["encrypted_token"].clone()),
        Err(error) if error.downcast_ref::<ApiError>().is_some_and(|e| e.captcha) => bail!(
            "Discord did not accept the captcha answer.\n\
             Use `agenticord login --token-stdin` instead."
        ),
        Err(error) => Err(error),
    }
}

const CAPTCHA_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Agenticord login</title>
<body style="font:16px sans-serif;max-width:32em;margin:3em auto;padding:0 1em">
<h1>Agenticord login</h1>
<p>Discord wants a captcha before it hands over the login. Solve it here.</p>
<div id="box"></div><p id="note"></p>
<script>
const cfg = __CONFIG__;
const note = document.getElementById("note");
function ready() {
  const id = hcaptcha.render("box", {
    sitekey: cfg.sitekey,
    callback: t => fetch("/solved", {method: "POST", body: t}).then(() => {
      note.textContent = "Done. Go back to the terminal; you can close this tab.";
    }),
    "error-callback": e => { note.textContent = "Captcha error: " + e; },
  });
  if (cfg.rqdata) hcaptcha.setData(id, {rqdata: cfg.rqdata});
}
</script>
<script src="https://js.hcaptcha.com/1/api.js?render=explicit&onload=ready&host=discord.com" async defer></script>
"#;

/// Serves a one-page captcha on localhost and waits for the browser to send
/// back the answer.
async fn solve(challenge: &Challenge) -> Result<String> {
    use axum::{Router, extract::State, response::Html, routing::{get, post}};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    if challenge.service != "hcaptcha" {
        bail!("Discord asked for a `{}` captcha, which Agenticord does not know.\nUse `agenticord login --token-stdin` instead.", challenge.service);
    }
    let config = json!({"sitekey": challenge.sitekey, "rqdata": challenge.rqdata}).to_string().replace("</", "<\\/");
    let page = CAPTCHA_PAGE.replace("__CONFIG__", &config);
    let (sender, receiver) = oneshot::channel::<String>();
    let slot = Arc::new(Mutex::new(Some(sender)));
    let router = Router::new()
        .route("/", get(move || async move { Html(page) }))
        .route(
            "/solved",
            post(|State(slot): State<Arc<Mutex<Option<oneshot::Sender<String>>>>>, answer: String| async move {
                if let Some(sender) = slot.lock().ok().and_then(|mut s| s.take()) {
                    let _ = sender.send(answer);
                }
                "ok"
            }),
        )
        .with_state(slot);

    let port: u16 = std::env::var("AGENTICORD_CAPTCHA_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8792);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not open port {port} for the captcha page (set AGENTICORD_CAPTCHA_PORT)"))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    eprintln!("\nDiscord wants a captcha first. Open this in your browser:");
    eprintln!("  http://127.0.0.1:{port}/");
    eprintln!("(on a remote machine, forward the port first: ssh -L {port}:127.0.0.1:{port} <this machine>)");
    eprintln!("Waiting for the captcha (up to 4 minutes)...");
    let answer = tokio::time::timeout(Duration::from_secs(240), receiver).await;
    server.abort();
    match answer {
        Ok(Ok(key)) if !key.trim().is_empty() => Ok(key),
        Ok(_) => bail!("The captcha page closed without an answer."),
        Err(_) => bail!("Timed out waiting for the captcha. Run `agenticord login` again."),
    }
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
                let encrypted = exchange(&discord, ticket).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captcha_page_hands_back_the_answer() {
        // SAFETY: set before any other thread in this test reads it.
        unsafe { std::env::set_var("AGENTICORD_CAPTCHA_PORT", "18792") };
        let challenge = Challenge {
            service: "hcaptcha".into(),
            sitekey: "site-key-123".into(),
            rqdata: Some("data</script>".into()),
            rqtoken: None,
        };
        let solver = tokio::spawn(async move { solve(&challenge).await });
        let client = reqwest::Client::new();
        let mut page = None;
        for _ in 0..50 {
            if let Ok(r) = client.get("http://127.0.0.1:18792/").send().await {
                page = Some(r.text().await.unwrap());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let page = page.expect("page served");
        assert!(page.contains("site-key-123"));
        assert!(!page.contains("data</script>"), "rqdata must not close the script tag");
        client.post("http://127.0.0.1:18792/solved").body("P1_answer").send().await.unwrap();
        assert_eq!(solver.await.unwrap().unwrap(), "P1_answer");
    }
}
