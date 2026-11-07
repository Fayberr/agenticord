# Agenticord

A tiny headless Discord client for AI agents. It logs into your Discord
account once, keeps a live gateway connection, and gives agents (Claude Code,
Antigravity, anything that speaks MCP) the tools to read, search and send as
you. No browser, no Electron, no UI.

- One static binary, about 4 MB, written in Rust.
- Idles in single-digit megabytes. The 90+ server READY payload is parsed for
  a few fields and dropped instead of cached.
- Cold release build in about one minute on a 4-core ARM box, incremental
  rebuilds in seconds. No Skia, no UI toolkit, no native media stack.
- MCP over HTTP (bearer token) or stdio.
- Logs in by QR code (scan with the phone app) or by pasting a token.

> **Terms of Service.** Automating a user account violates Discord's Terms of
> Service and can get the account banned. Agenticord copies the web client's
> behavior as closely as it can and rate-limits itself, but the risk is real
> and yours. Use it on your own account, on purpose. Not affiliated with
> Discord.

## Install

```sh
cargo build --release
install -Dm755 target/release/agenticord ~/.local/bin/agenticord
```

## Log in

```sh
agenticord login
```

A QR code appears in the terminal. In the Discord phone app open your avatar,
then **Scan QR Code**, scan it and confirm. Agenticord never sees a password
or a 2FA code, and saves only the resulting token (mode 0600) in
`~/.config/agenticord/token`.

If Discord asks for a captcha during QR login, or you would rather paste a
token:

```sh
printf '%s' "$TOKEN" | agenticord login --token-stdin
```

Check it worked: `agenticord whoami`.

## Run it

```sh
agenticord serve                 # MCP on http://127.0.0.1:8790/mcp
agenticord mcp-info              # prints the URL, the MCP token and a ready-made command
```

Add it to Claude Code:

```sh
claude mcp add --transport http --scope user agenticord http://127.0.0.1:8790/mcp \
  --header "Authorization: Bearer <token from mcp-info>"
```

Or skip the daemon and let the client start it:

```sh
claude mcp add --scope user agenticord -- agenticord stdio
```

A systemd user unit is in `deploy/agenticord.service`.

Try tools without any MCP client:

```sh
agenticord tools
agenticord call list_guilds
agenticord call read_messages '{"channel_id":"123456789012345678","limit":10}'
```

## Tools

| Tool | What it does |
| --- | --- |
| `discord_status` | Account, gateway state, health |
| `list_guilds`, `list_channels`, `list_dms` | Find things |
| `read_messages`, `search_messages` | Read and search |
| `send_message`, `send_file`, `edit_message`, `delete_message` | Write |
| `add_reaction`, `remove_reaction` | React |
| `clear_messages` | Bulk delete your own messages, dry run unless `confirm` |
| `open_dm`, `get_user`, `search_members` | People |
| `set_status` | Presence and custom status |
| `recent_events` | DMs and mentions that arrived while it ran |
| `discord_api` | Any REST endpoint the account can call |

## Safety

- The MCP server needs its bearer token and binds to localhost by default.
- Every write goes to `~/.config/agenticord/audit.jsonl`.
- `clear_messages` previews first and deletes at most 100 per call, spaced out.
- `discord_api` blocks account-security endpoints (auth, MFA, billing,
  sessions, connections, OAuth apps) and deleting whole servers or channels.
  `AGENTICORD_UNSAFE=1` lifts that.
- Messages your agent reads come from other people. Treat them as untrusted
  input: a message can try to talk your agent into doing something.

## How it works

`docs/protocol.md` describes everything Agenticord relies on: the headers and
fingerprint of the web client, the REST calls, the gateway, and the QR login.

## Limits

- No voice, video, screen share or push notifications.
- The TLS handshake of a Rust client does not look like Firefox's, and
  Agenticord does not imitate it.
- One gateway session. Do not run `serve` twice on one account.

## License

MIT. See `LICENSE`.
