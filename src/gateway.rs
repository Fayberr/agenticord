//! Discord's gateway as a user client: identify, heartbeats, resume, and the
//! few events Agenticord keeps. See docs/protocol.md section 3.
//!
//! The big READY payload is parsed for the handful of fields needed and
//! dropped at once, which is what keeps memory low.

use crate::{debug, discord::Discord, info, state::Shared, util};
use anyhow::{Context, Result, bail};
use flate2::{Decompress, FlushDecompress};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Instant, Interval, MissedTickBehavior, interval_at, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, header},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

const URL: &str = "wss://gateway.discord.gg";
const QUERY: &str = "/?encoding=json&v=9&compress=zlib-stream";
/// The capability bits the web client sends.
const CAPABILITIES: u64 = 1734653;
/// IDENTIFYs allowed per hour before Agenticord holds off.
const IDENTIFY_LIMIT: usize = 8;
const IDENTIFY_WINDOW: Duration = Duration::from_secs(3600);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub enum Cmd {
    /// online, idle, dnd or invisible.
    Presence(String),
    Close,
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

enum End {
    Resume,
    Reidentify,
    Fatal(String),
    Shutdown,
}

#[derive(Deserialize)]
struct Frame<'a> {
    op: u64,
    s: Option<u64>,
    t: Option<&'a str>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Ready {
    session_id: String,
    resume_gateway_url: Option<String>,
    user: ReadyUser,
    #[serde(default)]
    guilds: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct ReadyUser {
    id: String,
    username: String,
}

#[derive(Deserialize)]
struct Author {
    id: String,
    username: String,
    global_name: Option<String>,
    #[serde(default)]
    bot: bool,
}

#[derive(Deserialize)]
struct Mention {
    id: String,
}

#[derive(Deserialize)]
struct MessageCreate {
    id: String,
    channel_id: String,
    guild_id: Option<String>,
    #[serde(default)]
    content: String,
    author: Author,
    #[serde(default)]
    mentions: Vec<Mention>,
    #[serde(default)]
    mention_everyone: bool,
}

/// One zlib stream for a whole connection: frames end with `00 00 FF FF`.
struct Inflater {
    inflate: Decompress,
    buffer: Vec<u8>,
}

impl Inflater {
    fn new() -> Self {
        Self { inflate: Decompress::new(true), buffer: Vec::new() }
    }

    fn push(&mut self, data: &[u8]) -> Result<Option<String>> {
        self.buffer.extend_from_slice(data);
        if !self.buffer.ends_with(&[0, 0, 0xff, 0xff]) {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(self.buffer.len() * 4);
        let mut input: &[u8] = &self.buffer;
        // The decompressor can hold output back after it has used all its
        // input, so only stop once a call neither reads nor writes anything.
        loop {
            out.reserve(32 << 10);
            let (in_before, out_before) = (self.inflate.total_in(), self.inflate.total_out());
            self.inflate
                .decompress_vec(input, &mut out, FlushDecompress::Sync)
                .context("could not inflate a gateway frame")?;
            let used = (self.inflate.total_in() - in_before) as usize;
            input = &input[used..];
            if used == 0 && self.inflate.total_out() == out_before {
                break;
            }
        }
        self.buffer.clear();
        if self.buffer.capacity() > 1 << 20 {
            self.buffer = Vec::new();
        }
        Ok(Some(String::from_utf8(out).context("a gateway frame was not text")?))
    }
}

/// Runs the gateway for the life of the process: connects, keeps the session
/// alive, resumes after a drop, identifies again only when it must.
pub async fn run(discord: Arc<Discord>, shared: Arc<Shared>, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
    let mut session = Session::default();
    let mut retry = Duration::from_secs(2);
    let mut identifies: VecDeque<Instant> = VecDeque::new();
    loop {
        let resume = session.id.is_some() && session.resume_url.is_some();
        if !resume {
            let now = Instant::now();
            while identifies.front().is_some_and(|t| now.duration_since(*t) >= IDENTIFY_WINDOW) {
                identifies.pop_front();
            }
            if identifies.len() >= IDENTIFY_LIMIT {
                shared.set_status("paused: too many logins this hour");
                info!("holding off new gateway logins for 10 minutes");
                sleep(Duration::from_secs(600)).await;
                continue;
            }
            identifies.push_back(now);
        }
        shared.set_status(if resume { "resuming" } else { "connecting" });
        let started = Instant::now();
        let end = connect_once(&discord, &shared, &mut session, &mut cmds, resume).await;
        let up = started.elapsed();
        shared.with(|s| s.reconnects += 1);
        match end {
            Ok(End::Shutdown) => {
                shared.set_status("stopped");
                return;
            }
            Ok(End::Fatal(reason)) => {
                shared.set_status(&format!("stopped: {reason}"));
                info!("the gateway stopped for good: {reason}");
                return;
            }
            Ok(End::Reidentify) => {
                session = Session::default();
                sleep(util::jitter(Duration::from_secs(5), 0.2, 1.0)).await;
            }
            Ok(End::Resume) => {}
            Err(error) => {
                let text = format!("{error:#}");
                info!("gateway connection lost: {text}");
                shared.with(|s| s.last_error = Some(text));
            }
        }
        if discord.dead.load(std::sync::atomic::Ordering::Relaxed) {
            shared.set_status("stopped: Discord rejected the token");
            return;
        }
        if up >= Duration::from_secs(60) {
            retry = Duration::from_secs(2);
        }
        shared.set_status("reconnecting");
        sleep(util::jitter(retry, 0.5, 1.0)).await;
        retry = (retry * 2).min(Duration::from_secs(120));
    }
}

struct Conn<'a> {
    ws: Ws,
    inflate: Inflater,
    heartbeat: Option<Interval>,
    acked: bool,
    resume: bool,
    discord: &'a Discord,
    shared: &'a Shared,
    session: &'a mut Session,
}

async fn connect_once(
    discord: &Discord,
    shared: &Shared,
    session: &mut Session,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    resume: bool,
) -> Result<End> {
    let url = match (&session.resume_url, resume) {
        (Some(base), true) => format!("{}{QUERY}", base.trim_end_matches('/')),
        _ => format!("{URL}{QUERY}"),
    };
    let mut request = url.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_str(crate::fingerprint::user_agent())?,
    );
    headers.insert(header::ORIGIN, HeaderValue::from_static("https://discord.com"));
    let (ws, _) = timeout(Duration::from_secs(20), connect_async(request))
        .await
        .context("connecting to the gateway timed out")?
        .context("could not reach the gateway")?;
    let mut conn = Conn {
        ws,
        inflate: Inflater::new(),
        heartbeat: None,
        acked: true,
        resume,
        discord,
        shared,
        session,
    };
    loop {
        tokio::select! {
            message = conn.ws.next() => {
                let message = match message {
                    None => return Ok(End::Resume),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(message)) => message,
                };
                let text = match message {
                    Message::Binary(bytes) => conn.inflate.push(&bytes)?,
                    Message::Text(text) => Some(text.as_str().to_owned()),
                    Message::Close(frame) => return Ok(close_end(frame)),
                    _ => None,
                };
                if let Some(text) = text {
                    if let Some(end) = conn.frame(&text).await? {
                        return Ok(end);
                    }
                }
            }
            _ = async {
                match conn.heartbeat.as_mut() {
                    Some(interval) => { interval.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                if !conn.acked {
                    info!("Discord stopped answering heartbeats; reconnecting");
                    return Ok(End::Resume);
                }
                conn.acked = false;
                conn.send_heartbeat().await?;
            }
            command = cmds.recv() => match command {
                None | Some(Cmd::Close) => {
                    let _ = conn.ws.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })).await;
                    return Ok(End::Shutdown);
                }
                Some(Cmd::Presence(status)) => {
                    conn.shared.with(|s| s.presence = status.clone());
                    conn.send(json!({"op": 3, "d": presence(&status)})).await?;
                }
            },
        }
    }
}

fn presence(status: &str) -> Value {
    json!({"status": status, "since": 0, "activities": [], "afk": false})
}

fn close_end(frame: Option<CloseFrame>) -> End {
    let Some(frame) = frame else { return End::Resume };
    let code = u16::from(frame.code);
    debug!("gateway closed with {code} {}", frame.reason);
    match code {
        4004 | 4010 | 4011 | 4012 | 4013 | 4014 => {
            End::Fatal(format!("Discord closed the gateway with {code} {}", frame.reason))
        }
        4003 | 4007 | 4009 => End::Reidentify,
        _ => End::Resume,
    }
}

impl Conn<'_> {
    async fn send(&mut self, value: Value) -> Result<()> {
        self.ws.send(Message::text(value.to_string())).await?;
        Ok(())
    }

    async fn send_heartbeat(&mut self) -> Result<()> {
        let seq = self.session.seq;
        self.send(json!({"op": 1, "d": seq})).await
    }

    async fn frame(&mut self, text: &str) -> Result<Option<End>> {
        let frame: Frame = serde_json::from_str(text).context("could not parse a gateway frame")?;
        if let Some(seq) = frame.s {
            self.session.seq = Some(seq);
        }
        match frame.op {
            0 => self.dispatch(frame.t.unwrap_or(""), frame.d)?,
            1 => self.send_heartbeat().await?,
            7 => return Ok(Some(End::Resume)),
            9 => {
                let resumable = frame.d.is_some_and(|d| d.get() == "true");
                return Ok(Some(if resumable { End::Resume } else { End::Reidentify }));
            }
            10 => {
                let interval = frame
                    .d
                    .and_then(|d| serde_json::from_str::<Value>(d.get()).ok())
                    .and_then(|d| d["heartbeat_interval"].as_u64())
                    .context("the gateway's hello had no heartbeat interval")?;
                let every = Duration::from_millis(interval.max(1000));
                let first = util::jitter(every, 0.0, 1.0);
                let mut beat = interval_at(Instant::now() + first, every);
                beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
                self.heartbeat = Some(beat);
                self.hello().await?;
            }
            11 => self.acked = true,
            _ => {}
        }
        Ok(None)
    }

    async fn hello(&mut self) -> Result<()> {
        let token = match self.discord.token() {
            Some(token) => token.to_owned(),
            None => bail!("the gateway needs a token"),
        };
        if self.resume {
            let (Some(id), seq) = (self.session.id.clone(), self.session.seq) else {
                bail!("nothing to resume");
            };
            return self
                .send(json!({"op": 6, "d": {"token": token, "session_id": id, "seq": seq}}))
                .await;
        }
        let status = self.shared.with(|s| s.presence.clone());
        let identify = json!({"op": 2, "d": {
            "token": token,
            "capabilities": CAPABILITIES,
            "properties": self.discord.fp.properties(),
            "presence": presence(&status),
            "compress": false,
            "client_state": {"guild_versions": {}},
        }});
        self.send(identify).await
    }

    fn dispatch(&mut self, name: &str, data: Option<&RawValue>) -> Result<()> {
        let Some(data) = data else { return Ok(()) };
        match name {
            "READY" => {
                let ready: Ready = serde_json::from_str(data.get()).context("could not read READY")?;
                self.session.id = Some(ready.session_id);
                self.session.resume_url = ready.resume_gateway_url;
                let guilds = ready.guilds.len();
                self.shared.with(|s| {
                    s.user_id = Some(ready.user.id);
                    s.username = Some(ready.user.username);
                    s.guilds = guilds;
                    s.connected_at = util::now_secs();
                    s.last_error = None;
                    s.status = "ready".into();
                });
                info!("gateway ready ({guilds} servers)");
            }
            "RESUMED" => {
                self.shared.with(|s| {
                    s.status = "ready".into();
                    s.last_error = None;
                });
                info!("gateway session resumed");
            }
            "MESSAGE_CREATE" => {
                let Ok(message) = serde_json::from_str::<MessageCreate>(data.get()) else {
                    return Ok(());
                };
                let me = self.shared.with(|s| s.user_id.clone()).unwrap_or_default();
                if message.author.id == me {
                    return Ok(());
                }
                let mentioned =
                    message.mention_everyone || message.mentions.iter().any(|m| m.id == me);
                let direct = message.guild_id.is_none();
                if !(direct || mentioned) {
                    return Ok(());
                }
                self.shared.push_event(json!({
                    "type": if direct { "dm" } else { "mention" },
                    "message_id": message.id,
                    "channel_id": message.channel_id,
                    "guild_id": message.guild_id,
                    "author_id": message.author.id,
                    "author": message.author.global_name.unwrap_or(message.author.username),
                    "bot": message.author.bot,
                    "content": util::clip(&message.content, 500),
                    "time": util::now_secs(),
                }));
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    /// Compresses `text` as one zlib-stream message (sync flush at the end).
    fn deflate(c: &mut Compress, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() / 2 + 1024);
        let start = c.total_in();
        while ((c.total_in() - start) as usize) < text.len() {
            out.reserve(1 << 16);
            let done = (c.total_in() - start) as usize;
            c.compress_vec(&text[done..], &mut out, FlushCompress::None).unwrap();
        }
        loop {
            out.reserve(1 << 16);
            c.compress_vec(&[], &mut out, FlushCompress::Sync).unwrap();
            if out.ends_with(&[0, 0, 0xff, 0xff]) && out.len() < out.capacity() {
                break;
            }
        }
        out
    }

    #[test]
    fn inflates_big_frames_in_chunks() {
        let mut compressor = Compress::new(Compression::default(), true);
        let mut inflater = Inflater::new();
        for size in [100usize, 70_000, 6_000_000, 12_345_678] {
            let mut text = String::from("{\"d\":\"");
            let mut x: u64 = 12345;
            while text.len() < size {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Repetitive like real gateway JSON, so it compresses well.
                text.push_str(&format!("{{\"id\":\"{}\",\"name\":\"channel\",\"type\":0}},", (x >> 50) % 10));
            }
            text.push_str("\"}");
            let packed = deflate(&mut compressor, text.as_bytes());
            let mut got = None;
            for chunk in packed.chunks(16 * 1024) {
                if let Some(frame) = inflater.push(chunk).unwrap() {
                    got = Some(frame);
                }
            }
            let got = got.expect("a frame");
            assert_eq!(got.len(), text.len(), "size {size}");
            assert!(got == text);
        }
    }
}
