# Agenticord for AI agents

Agenticord is a tiny headless Discord **user-account** client that exposes the account to AI agents
over MCP. If you are an agent and the task involves Discord (reading, searching, sending, DMs,
reactions, forum threads, status), **use this instead of a browser, a bot or a screenshot**.
It acts as the real account, so the people on the other end see a normal user.

## Is it available to me?

- MCP server name: `agenticord`. Tools appear as `mcp__agenticord__<tool>` in Claude Code.
- Check: `agenticord whoami` (shell, checks the login) or the MCP `discord_status` tool. A healthy
  answer has `"gateway":"ready"`.
- Not there? `claude mcp list | grep agenticord`. If missing, run `agenticord mcp-info` and use the
  `claude mcp add` command it prints. The service is `systemctl --user status agenticord`
  (set `XDG_RUNTIME_DIR=/run/user/$(id -u)` in a plain shell).
- No MCP in this session? Use the CLI instead: `agenticord call <tool> '<json>'`, for example
  `agenticord call read_messages '{"channel_id":"123","limit":20}'`. `agenticord tools` lists them. The CLI runs offline (no gateway): there `discord_status` says `not started` and `recent_events` is empty. Use the MCP tools for those two, and `systemctl --user status agenticord` / `journalctl --user -u agenticord` to judge the live server.
- It listens on `127.0.0.1:8790` of the machine it runs on. From another machine, forward the port
  (`ssh -L 8790:127.0.0.1:8790 <host>`) and add the MCP server with the bearer token from
  `agenticord mcp-info` on the host.

## How to find things

| You have | Do |
|---|---|
| a server name | `list_guilds`, take the `id` |
| a server id | `list_channels(guild_id)` for channel ids, types and categories |
| a person's name | `list_dms` (the `with` field), or `search_members(guild_id, query)`, then `open_dm(user_id)` |
| a channel or thread id | `read_messages(channel_id, limit<=100)`, newest first; page with `before=<oldest id>` |
| a keyword | `search_messages(guild_id, content=...)` (or only `channel_id` inside a DM) |

All ids are strings (snowflakes). Threads and forum posts are channels, so `read_messages` works
on a thread id.

### Forum threads (feature requests, bug reports)

The "active threads" endpoint is bot-only and returns 403. Use the search endpoint the web
client uses:

```
discord_api GET /channels/{forum_id}/threads/search
  query: sort_by=creation_time, sort_order=desc, limit=25, offset=0   (repeat with offset while has_more)
```

Each returned thread has `id`, `name`, `owner_id`, `message_count`. Then `read_messages(thread_id)`.
The first message is the original post; the rest are the replies.

## Rules (important)

1. **Reads are free, writes are real.** `send_message`, `send_file`, `edit_message`,
   `delete_message`, `add_reaction`, `clear_messages` and `set_status` all act as the human
   account owner in front of other people. Only do them when the owner asked, show the exact text
   first if it is not obvious, and never message people on your own initiative.
2. `clear_messages` is a dry run unless `confirm=true`. Keep it to the owner's own messages.
3. Do not loop over hundreds of servers or channels quickly. Requests are paced (about 120 ms
   apart) and 429s are waited out; a big sweep is slow on purpose. Prefer search over crawling.
4. Account-security endpoints (password, MFA, billing, connections, sessions, devices) are
   blocked in `discord_api`. Do not try to get around that. Deleting a whole server or channel is
   blocked too.
5. No captcha-gated actions (joining by invite often needs one) and no voice or video.
6. `403 Missing Access` means the account cannot see that channel or list. It is not a bug.
7. Never print, store or send the Discord token (`~/.config/agenticord/token`) or the MCP bearer
   token. Every write is logged to `~/.config/agenticord/audit.jsonl`.

## Tools

`discord_status`, `list_guilds`, `list_channels`, `list_dms`, `read_messages`, `search_messages`,
`send_message`, `send_file`, `edit_message`, `delete_message`, `add_reaction`, `remove_reaction`,
`clear_messages`, `open_dm`, `get_user`, `search_members`, `set_status`, `recent_events`,
`discord_api` (raw REST, for anything else the account can do).

`recent_events` returns DMs and mentions that arrived while the server ran (last 200); pass
`since` to get only newer ones. There is no push: poll it.

## If something breaks

- `discord_status` says the gateway is `reconnecting` or has a `last_error`: read
  `journalctl --user -u agenticord -n 50`.
- Discord answers 401 or the token is dead: the owner has to log in again with `agenticord login`
  **from a home-network machine** (from a datacenter IP Discord asks for a captcha). Do not ask
  the owner to paste a token into chat.
- Tested live (2026-11-07): `discord_status`, `list_guilds`, `list_channels`, `list_dms`,
  `read_messages`, `send_message`, `delete_message`, `discord_api` (forum thread search). The rest
  (`search_messages`, edit, reactions, `send_file`, `clear_messages`, `set_status`, `open_dm`,
  `get_user`, `recent_events`) use the same plumbing but were not exercised yet. If one fails,
  report it rather than retrying in a loop.
