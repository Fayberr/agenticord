# Discord user-client protocol notes

These are the protocol facts Agenticord relies on, written in my own words.
They describe how Discord's web client talks to Discord, so that a headless
client can do the same. Nothing here is copied from another client's source.
The facts were cross-checked against a working open-source client's behavior
and have to be confirmed against the live service, which is the authority.
Anything marked **(verify)** has not yet been confirmed live.

## 1. Presenting as the web client

Discord expects user traffic to look like a current desktop browser on the web
app. Agenticord claims a recent Firefox on Linux.

- `User-Agent`: `Mozilla/5.0 (X11; Linux x86_64; rv:N.0) Gecko/20100101 Firefox/N.0`.
  N is a recent release number. Firefox ships a release about every 14 days,
  so N is computed from the date, one release behind the newest.
- `X-Super-Properties`: base64 (standard alphabet) of the JSON properties
  object below. The gateway IDENTIFY carries the same object as `properties`.
- Other headers sent by the browser on API calls: `Accept: */*`,
  `Accept-Language: en-US,en;q=0.9`, `Origin: https://discord.com`,
  `Referer: https://discord.com/channels/@me`, `X-Discord-Locale: en-US`,
  `X-Discord-Timezone: <IANA zone>`, `X-Debug-Options: bugReporterEnabled`,
  and `Sec-Fetch-Dest: empty`, `Sec-Fetch-Mode: cors`, `Sec-Fetch-Site: same-origin`.
- `Authorization: <token>`: the raw user token, no `Bearer ` prefix.
- Cookies: a browser first loads `https://discord.com/app`, which sets the
  `__dcfduid`, `__sdcfduid` and `__cfruid` cookies. They are kept in a cookie
  jar and sent back on API calls.

### Properties object

```json
{
  "os": "Linux",
  "browser": "Firefox",
  "device": "",
  "system_locale": "en-US",
  "has_client_mods": false,
  "browser_user_agent": "<the User-Agent>",
  "browser_version": "N.0",
  "os_version": "",
  "referrer": "",
  "referring_domain": "",
  "referrer_current": "",
  "referring_domain_current": "",
  "release_channel": "stable",
  "client_build_number": 0,
  "client_event_source": null,
  "client_launch_id": "<uuid v4, once per process>",
  "launch_signature": "<uuid v4 with certain bits cleared>",
  "client_heartbeat_session_id": "<uuid v4, renewed about every 30 minutes>"
}
```

- `client_build_number` is the web app's current build. The HTML of
  `https://discord.com/app` sets it in a `GLOBAL_ENV` script as
  `"BUILD_NUMBER":"<digits>"`, near the top of the page. A stale number is
  tolerated for a while, but the current one is better.
- `launch_signature` is a random v4 UUID with a fixed set of bit positions
  forced to zero. The set reports "no client modifications". The 128-bit mask
  is `0x00801010_08100800_20810040_01000800`.

## 2. REST

- Base URL: `https://discord.com/api/v9`.
- JSON bodies. Snowflake ids are strings.
- Rate limits: HTTP 429 with a JSON body `{"retry_after": <seconds float>,
  "global": <bool>}` and a `Retry-After` header. Wait and retry.
- Search: an index that is not ready returns HTTP 202 with
  `{"code": 110000, "retry_after": <seconds>}`. Wait and repeat the call.
- A request that needs a captcha answers HTTP 400 with `captcha_key` set to a
  list such as `["captcha-required"]`, plus `captcha_sitekey` and
  `captcha_service`.
- HTTP 401 means the token is dead.

### Endpoints used

| Purpose | Call |
| --- | --- |
| Current account | `GET /users/@me` |
| Servers | `GET /users/@me/guilds?with_counts=true` |
| Channels of a server | `GET /guilds/{id}/channels` |
| DMs and group DMs | `GET /users/@me/channels` |
| Open a DM | `POST /users/@me/channels` `{"recipients": ["<user id>"]}` |
| Read messages | `GET /channels/{id}/messages?limit=N[&before=ID][&after=ID][&around=ID]` |
| Send | `POST /channels/{id}/messages` `{content, nonce, tts:false, flags:0, message_reference?}` |
| Send with a file | `POST /channels/{id}/messages` as multipart: `payload_json` and `files[0]` |
| Edit | `PATCH /channels/{c}/messages/{m}` `{content}` |
| Delete | `DELETE /channels/{c}/messages/{m}` |
| Add or remove a reaction | `PUT` or `DELETE /channels/{c}/messages/{m}/reactions/{emoji}/@me` (emoji percent-encoded, custom ones as `name:id`) |
| Search a server | `GET /guilds/{id}/messages/search?content=..&author_id=..&channel_id=..&has=..&offset=N` |
| Search a DM | `GET /channels/{id}/messages/search?content=..` |
| Search members | `GET /guilds/{id}/members/search?query=..&limit=N` |
| Profile | `GET /users/{id}/profile` |
| Typing | `POST /channels/{id}/typing` |
| Settings (status, custom status) | `PATCH /users/@me/settings` `{"status": "idle"}` or `{"custom_status": {...}}` |

`nonce` is a snowflake built from the current time: `(unix_ms - 1420070400000) << 22`.
Deleting a message only works for types 0, 19, 20 and 23 (normal, reply, slash
command, context menu); system messages are not deletable.

## 3. Gateway

- URL: `wss://gateway.discord.gg/?encoding=json&v=9&compress=zlib-stream`.
  Send the same `User-Agent` and `Origin: https://discord.com` as for REST.
- `compress=zlib-stream` makes the whole connection one zlib stream. Binary
  frames are appended to a buffer, and a message is complete when the buffer
  ends with `00 00 FF FF`. Inflate with a decompressor that persists for the
  life of the connection.
- Frames are `{"op": N, "d": ..., "s": seq, "t": "EVENT"}`.

| op | Name | Direction |
| --- | --- | --- |
| 0 | Dispatch | server |
| 1 | Heartbeat | both |
| 2 | Identify | client |
| 3 | Presence update | client |
| 6 | Resume | client |
| 7 | Reconnect | server |
| 9 | Invalid session | server (`d` is true if resumable) |
| 10 | Hello | server (`d.heartbeat_interval` in ms) |
| 11 | Heartbeat ACK | server |

Flow: connect, receive Hello, start heartbeats (first one after
`interval * random(0..1)`, then every interval, each carrying the last sequence
number or null), send Identify (or Resume). READY arrives as a dispatch.

### Identify

```json
{"op": 2, "d": {
  "token": "<token>",
  "capabilities": 1734653,
  "properties": { ... section 1 ... },
  "presence": {"status": "online", "since": 0, "activities": [], "afk": false},
  "compress": false,
  "client_state": {"guild_versions": {}}
}}
```

`capabilities` is a bit field the web client sends. `1734653` is the value in
use at the time of writing **(verify)**. `compress` stays false because the
transport compression above is used instead.

### READY

Carries `session_id`, `resume_gateway_url`, `user` (with `id`, `username`,
`global_name`) and `guilds`. User READY payloads are large (megabytes for an
account in many servers), so a headless client parses only the few fields it
needs and drops the rest.

### Resume

`{"op": 6, "d": {"token", "session_id", "seq"}}` on a new connection to
`resume_gateway_url` (same query string). Invalid session with `d: false`
means identify again after a random 1 to 5 second wait.

### Close codes

- Final, do not retry with this token: 4004 (auth failed), 4010, 4011, 4012,
  4013, 4014.
- Identify again: 4007 (bad sequence), 4009 (session timed out), 4003.
- Anything else: reconnect and resume.

Keep identifies rare. Discord flags accounts that identify often, so cap them
to a handful per hour and back off exponentially between reconnects.

### Events used

- `MESSAGE_CREATE`: `id`, `channel_id`, `guild_id` (absent in DMs), `content`,
  `author` (`id`, `username`, `global_name`, `bot`), `mentions` (users).

## 4. QR login (remote auth)

The phone app can authorize a new device by scanning a code. This avoids
handling a password, 2FA or the token by hand.

Gateway: `wss://remote-auth-gateway.discord.gg/?v=2` with `Origin: https://discord.com`.

1. Client makes an RSA-2048 key pair.
2. Server: `{"op": "hello", "heartbeat_interval": ms, "timeout_ms": ms}`.
3. Client: `{"op": "init", "encoded_public_key": base64(SPKI DER)}`.
4. Server: `{"op": "nonce_proof", "encrypted_nonce": base64}`.
   Client decrypts with RSA-OAEP (SHA-256, MGF1 SHA-256) and answers
   `{"op": "nonce_proof", "proof": base64url-no-pad(sha256(nonce))}`.
5. Server: `{"op": "pending_remote_init", "fingerprint": "..."}`. The QR code
   encodes `https://discord.com/ra/<fingerprint>`.
6. After scanning: `{"op": "pending_ticket", "encrypted_user_payload": base64}`
   (decrypts to `id:discriminator:avatar:username`).
7. After the phone user confirms: `{"op": "pending_login", "ticket": "..."}`.
   Client calls `POST /users/@me/remote-auth/login` `{"ticket": ...}` without
   an Authorization header. The reply has `encrypted_token`, which decrypts
   with the same key to the user token. This call can demand a captcha.
8. Client sends `{"op": "heartbeat"}` every 40 seconds (or the hello interval)
   while waiting. `{"op": "cancel"}` means the phone user declined.

## 5. Known limits

- The TLS handshake of a Rust client does not look like Firefox's.
  Fingerprinting at that layer is not imitated.
- Voice, video, screen share and push notifications are out of scope.
- Using a user token in a program is against Discord's Terms of Service.
