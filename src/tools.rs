//! The tools exposed to AI clients. Each one is a thin, compact wrapper over
//! Discord's REST API; `discord_api` reaches everything else.

use crate::{
    config,
    discord::Discord,
    gateway::Cmd,
    state::Shared,
    util::{self, clip, pct, snowflake},
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Map, Value, json};
use std::{io::Write, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedSender;

pub struct Ctx {
    pub discord: Arc<Discord>,
    pub shared: Arc<Shared>,
    pub gateway: Option<UnboundedSender<Cmd>>,
}

/// Most characters of a message returned to the model.
const CONTENT_CAP: usize = 1800;
/// Most messages `clear_messages` removes in one call.
const CLEAR_CAP: usize = 100;

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
    })
}

fn id_prop(what: &str) -> Value {
    json!({"type": "string", "description": what})
}

pub fn list() -> Vec<Value> {
    let channel = id_prop("Channel id (a server channel, DM or thread)");
    let message = id_prop("Message id");
    vec![
        tool("discord_status", "Connection state, the account, and Agenticord's own health.", json!({}), &[]),
        tool("list_guilds", "Servers the account is in: id, name, owner flag, member count.", json!({}), &[]),
        tool(
            "list_channels",
            "Channels of a server: id, name, type, category. Sorted by category and position.",
            json!({"guild_id": id_prop("Server id")}),
            &["guild_id"],
        ),
        tool(
            "list_dms",
            "Open DMs and group DMs, most recently active first.",
            json!({"limit": {"type": "integer", "description": "Default 50, max 200"}}),
            &[],
        ),
        tool(
            "read_messages",
            "Read messages from a channel, newest first. Page with before/after.",
            json!({
                "channel_id": channel,
                "limit": {"type": "integer", "description": "1 to 100, default 25"},
                "before": id_prop("Only messages before this message id"),
                "after": id_prop("Only messages after this message id"),
            }),
            &["channel_id"],
        ),
        tool(
            "search_messages",
            "Search messages. Give guild_id to search a server (optionally narrowed by channel_id) or only channel_id to search a DM.",
            json!({
                "guild_id": id_prop("Server id"),
                "channel_id": id_prop("Channel id"),
                "content": {"type": "string", "description": "Words to look for"},
                "author_id": id_prop("Only messages by this user"),
                "has": {"type": "string", "description": "link, embed, file, image, video, sound or sticker"},
                "mentions": id_prop("Only messages mentioning this user id"),
                "offset": {"type": "integer", "description": "Skip this many results (25 per page)"},
            }),
            &[],
        ),
        tool(
            "send_message",
            "Send a message, optionally as a reply.",
            json!({
                "channel_id": channel,
                "content": {"type": "string"},
                "reply_to": id_prop("Message id to reply to"),
            }),
            &["channel_id", "content"],
        ),
        tool(
            "send_file",
            "Send a file from the machine Agenticord runs on, with optional text.",
            json!({
                "channel_id": channel,
                "path": {"type": "string", "description": "Absolute path of the file"},
                "content": {"type": "string"},
                "reply_to": id_prop("Message id to reply to"),
            }),
            &["channel_id", "path"],
        ),
        tool(
            "edit_message",
            "Edit one of your own messages.",
            json!({"channel_id": channel, "message_id": message, "content": {"type": "string"}}),
            &["channel_id", "message_id", "content"],
        ),
        tool(
            "delete_message",
            "Delete a message (yours, or any if you have Manage Messages).",
            json!({"channel_id": channel, "message_id": message}),
            &["channel_id", "message_id"],
        ),
        tool(
            "add_reaction",
            "React to a message. emoji is a unicode emoji or name:id for a custom one.",
            json!({"channel_id": channel, "message_id": message, "emoji": {"type": "string"}}),
            &["channel_id", "message_id", "emoji"],
        ),
        tool(
            "remove_reaction",
            "Remove your reaction from a message.",
            json!({"channel_id": channel, "message_id": message, "emoji": {"type": "string"}}),
            &["channel_id", "message_id", "emoji"],
        ),
        tool(
            "clear_messages",
            "Delete recent messages in a channel, only yours by default. Dry run unless confirm is true. At most 100 per call, spaced out to stay under rate limits.",
            json!({
                "channel_id": channel,
                "limit": {"type": "integer", "description": "How many matching messages, max 100, default 25"},
                "only_mine": {"type": "boolean", "description": "Default true"},
                "confirm": {"type": "boolean", "description": "Must be true to delete; otherwise it only previews"},
            }),
            &["channel_id"],
        ),
        tool(
            "open_dm",
            "Open (or find) the DM channel with a user and return its channel id.",
            json!({"user_id": id_prop("User id")}),
            &["user_id"],
        ),
        tool(
            "get_user",
            "A user's profile: names, bio, pronouns.",
            json!({"user_id": id_prop("User id"), "guild_id": id_prop("Server id, for server profile data")}),
            &["user_id"],
        ),
        tool(
            "search_members",
            "Find server members by name prefix.",
            json!({
                "guild_id": id_prop("Server id"),
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "1 to 100, default 10"},
            }),
            &["guild_id", "query"],
        ),
        tool(
            "set_status",
            "Set presence (online, idle, dnd, invisible) and/or a custom status text. Empty custom_status clears it.",
            json!({
                "status": {"type": "string", "description": "online, idle, dnd or invisible"},
                "custom_status": {"type": "string"},
            }),
            &[],
        ),
        tool(
            "recent_events",
            "Incoming DMs and mentions seen since Agenticord started (last 200). Use since to get only newer ones.",
            json!({"since": {"type": "integer", "description": "Return events with n greater than this"}}),
            &[],
        ),
        tool(
            "discord_api",
            "Call any Discord REST endpoint as the account (path relative to /api/v9, e.g. /guilds/ID/roles). Account-security endpoints are blocked.",
            json!({
                "method": {"type": "string", "description": "GET, POST, PUT, PATCH or DELETE"},
                "path": {"type": "string", "description": "Starts with /"},
                "query": {"type": "object", "description": "Query string parameters"},
                "body": {"description": "JSON body"},
            }),
            &["method", "path"],
        ),
    ]
}

// ---- argument helpers ------------------------------------------------------

fn id(args: &Value, key: &str) -> Result<String> {
    snowflake(args.get(key).ok_or_else(|| anyhow!("`{key}` is required"))?)
        .with_context(|| format!("bad `{key}`"))
}

fn opt_id(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(v) => Ok(Some(snowflake(v).with_context(|| format!("bad `{key}`"))?)),
    }
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn req_text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    text(args, key).ok_or_else(|| anyhow!("`{key}` is required"))
}

fn number(args: &Value, key: &str, default: u64, max: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default).clamp(1, max)
}

fn flag(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

// ---- output shaping --------------------------------------------------------

/// Drops nulls and empty arrays/strings so results stay small.
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, compact(v)))
                .filter(|(_, v)| !matches!(v, Value::Null) && v != &json!([]) && v != &json!(""))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(compact).collect()),
        other => other,
    }
}

fn author_name(user: &Value) -> Value {
    user["global_name"]
        .as_str()
        .or_else(|| user["username"].as_str())
        .map(Value::from)
        .unwrap_or(Value::Null)
}

fn shape_message(m: &Value) -> Value {
    let attachments: Vec<Value> = m["attachments"]
        .as_array()
        .map(|a| a.iter().map(|x| x["url"].clone()).collect())
        .unwrap_or_default();
    let reactions: Vec<Value> = m["reactions"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .map(|r| {
                    let emoji = r["emoji"]["name"].as_str().unwrap_or("?");
                    json!(format!("{emoji} x{}", r["count"]))
                })
                .collect()
        })
        .unwrap_or_default();
    let embeds = m["embeds"].as_array().map_or(0, Vec::len);
    let mut content = m["content"].as_str().unwrap_or("").to_owned();
    if content.chars().count() > CONTENT_CAP {
        content = clip(&content, CONTENT_CAP);
    }
    compact(json!({
        "id": m["id"],
        "time": m["timestamp"],
        "edited": m["edited_timestamp"],
        "author": author_name(&m["author"]),
        "author_id": m["author"]["id"],
        "bot": m["author"]["bot"].as_bool().filter(|b| *b),
        "content": content,
        "attachments": attachments,
        "embeds": if embeds > 0 { json!(embeds) } else { Value::Null },
        "reactions": reactions,
        "reply_to": m["message_reference"]["message_id"],
        "type": m["type"].as_u64().filter(|t| *t != 0),
    }))
}

fn channel_type(code: u64) -> &'static str {
    match code {
        0 => "text",
        1 => "dm",
        2 => "voice",
        3 => "group_dm",
        4 => "category",
        5 => "announcement",
        10..=12 => "thread",
        13 => "stage",
        14 => "directory",
        15 => "forum",
        16 => "media",
        _ => "other",
    }
}

// ---- audit log -------------------------------------------------------------

const WRITES: &[&str] = &[
    "send_message", "send_file", "edit_message", "delete_message", "add_reaction",
    "remove_reaction", "clear_messages", "open_dm", "set_status", "discord_api",
];

fn audit(name: &str, args: &Value, ok: bool) {
    if !WRITES.contains(&name) {
        return;
    }
    let mut shown = args.clone();
    if let Some(content) = shown.get_mut("content") {
        if let Some(text) = content.as_str() {
            *content = Value::from(clip(text, 120));
        }
    }
    let line = json!({"ts": util::now_secs(), "tool": name, "ok": ok, "args": shown});
    if let Ok(dir) = config::ensure_dir() {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("audit.jsonl"))
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

// ---- dispatch --------------------------------------------------------------

pub async fn call(ctx: &Ctx, name: &str, args: Value) -> Result<Value> {
    let result = run(ctx, name, &args).await;
    audit(name, &args, result.is_ok());
    result
}

async fn run(ctx: &Ctx, name: &str, a: &Value) -> Result<Value> {
    let d = &ctx.discord;
    match name {
        "discord_status" => {
            let me = d.me().await?;
            let state = ctx.shared.with(|s| {
                json!({
                    "gateway": s.status,
                    "last_error": s.last_error,
                    "servers": s.guilds,
                    "presence": s.presence,
                    "connected_for_secs": if s.connected_at > 0 { util::now_secs().saturating_sub(s.connected_at) } else { 0 },
                    "reconnects": s.reconnects,
                    "queued_events": s.events.len(),
                })
            });
            Ok(compact(json!({
                "user_id": me["id"],
                "username": me["username"],
                "name": me["global_name"],
                "email_verified": me["verified"],
                "premium": me["premium_type"],
                "web_build": d.fp.build_number,
                "gateway": state,
            })))
        }
        "list_guilds" => {
            let guilds = d.get("/users/@me/guilds", &[("with_counts", "true".into())]).await?;
            Ok(compact(Value::Array(
                guilds
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|g| {
                        json!({
                            "id": g["id"], "name": g["name"],
                            "owner": g["owner"].as_bool().filter(|o| *o),
                            "members": g["approximate_member_count"],
                        })
                    })
                    .collect(),
            )))
        }
        "list_channels" => {
            let guild = id(a, "guild_id")?;
            let channels = d.get(&format!("/guilds/{guild}/channels"), &[]).await?;
            let mut rows: Vec<&Value> = channels.as_array().into_iter().flatten().collect();
            let category_position = |c: &Value| -> i64 {
                match c["parent_id"].as_str() {
                    Some(parent) => rows_position(&channels, parent),
                    None => c["position"].as_i64().unwrap_or(0),
                }
            };
            rows.sort_by_key(|c| {
                let kind = c["type"].as_u64().unwrap_or(0);
                let group = if kind == 4 { c["position"].as_i64().unwrap_or(0) } else { category_position(c) };
                (group, kind != 4, c["position"].as_i64().unwrap_or(0))
            });
            Ok(compact(Value::Array(
                rows.into_iter()
                    .map(|c| {
                        json!({
                            "id": c["id"], "name": c["name"],
                            "type": channel_type(c["type"].as_u64().unwrap_or(99)),
                            "category_id": c["parent_id"],
                            "topic": c["topic"].as_str().map(|t| clip(t, 120)),
                        })
                    })
                    .collect(),
            )))
        }
        "list_dms" => {
            let limit = number(a, "limit", 50, 200) as usize;
            let channels = d.get("/users/@me/channels", &[]).await?;
            let mut rows: Vec<&Value> = channels.as_array().into_iter().flatten().collect();
            rows.sort_by(|x, y| {
                let key = |c: &Value| c["last_message_id"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                key(y).cmp(&key(x))
            });
            Ok(compact(Value::Array(
                rows.into_iter()
                    .take(limit)
                    .map(|c| {
                        let who: Vec<String> = c["recipients"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|u| author_name(u).as_str().unwrap_or("?").to_owned())
                            .collect();
                        let recipient_ids: Vec<Value> = c["recipients"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|u| u["id"].clone())
                            .collect();
                        json!({
                            "channel_id": c["id"],
                            "type": channel_type(c["type"].as_u64().unwrap_or(1)),
                            "name": c["name"],
                            "with": who.join(", "),
                            "user_ids": recipient_ids,
                            "last_message_id": c["last_message_id"],
                        })
                    })
                    .collect(),
            )))
        }
        "read_messages" => {
            let channel = id(a, "channel_id")?;
            let mut query = vec![("limit", number(a, "limit", 25, 100).to_string())];
            if let Some(before) = opt_id(a, "before")? {
                query.push(("before", before));
            }
            if let Some(after) = opt_id(a, "after")? {
                query.push(("after", after));
            }
            let messages = d.get(&format!("/channels/{channel}/messages"), &query).await?;
            Ok(Value::Array(messages.as_array().into_iter().flatten().map(shape_message).collect()))
        }
        "search_messages" => {
            let guild = opt_id(a, "guild_id")?;
            let channel = opt_id(a, "channel_id")?;
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(content) = text(a, "content") {
                query.push(("content", content.to_owned()));
            }
            if let Some(author) = opt_id(a, "author_id")? {
                query.push(("author_id", author));
            }
            if let Some(mentions) = opt_id(a, "mentions")? {
                query.push(("mentions", mentions));
            }
            if let Some(has) = text(a, "has") {
                query.push(("has", has.to_owned()));
            }
            if let (Some(_), Some(channel)) = (&guild, &channel) {
                query.push(("channel_id", channel.clone()));
            }
            if let Some(offset) = a.get("offset").and_then(Value::as_u64) {
                query.push(("offset", offset.to_string()));
            }
            let path = match (&guild, &channel) {
                (Some(g), _) => format!("/guilds/{g}/messages/search"),
                (None, Some(c)) => format!("/channels/{c}/messages/search"),
                _ => bail!("give guild_id or channel_id"),
            };
            let result = d.get(&path, &query).await?;
            let hits: Vec<Value> = result["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|group| group.as_array())
                .filter_map(|group| group.iter().find(|m| m["hit"].as_bool() == Some(true)).or(group.first()))
                .map(|m| {
                    let mut shaped = shape_message(m);
                    shaped["channel_id"] = m["channel_id"].clone();
                    shaped
                })
                .collect();
            Ok(json!({"total": result["total_results"], "messages": hits}))
        }
        "send_message" => {
            let channel = id(a, "channel_id")?;
            let content = req_text(a, "content")?;
            if content.trim().is_empty() {
                bail!("`content` is empty");
            }
            let mut body = json!({"content": content, "nonce": util::nonce(), "tts": false, "flags": 0});
            if let Some(reply) = opt_id(a, "reply_to")? {
                body["message_reference"] = json!({"message_id": reply, "channel_id": channel});
            }
            let sent = d.post(&format!("/channels/{channel}/messages"), &body).await?;
            Ok(json!({"sent": true, "message_id": sent["id"], "channel_id": sent["channel_id"]}))
        }
        "send_file" => {
            let channel = id(a, "channel_id")?;
            let path = req_text(a, "path")?;
            let reply = opt_id(a, "reply_to")?;
            let sent = d
                .send_file(&channel, Path::new(path), text(a, "content").unwrap_or(""), reply.as_deref())
                .await?;
            Ok(json!({"sent": true, "message_id": sent["id"], "channel_id": sent["channel_id"]}))
        }
        "edit_message" => {
            let (channel, message) = (id(a, "channel_id")?, id(a, "message_id")?);
            let body = json!({"content": req_text(a, "content")?});
            d.call(Method::PATCH, &format!("/channels/{channel}/messages/{message}"), &[], Some(&body)).await?;
            Ok(json!({"edited": true}))
        }
        "delete_message" => {
            let (channel, message) = (id(a, "channel_id")?, id(a, "message_id")?);
            d.call(Method::DELETE, &format!("/channels/{channel}/messages/{message}"), &[], None).await?;
            Ok(json!({"deleted": true}))
        }
        "add_reaction" | "remove_reaction" => {
            let (channel, message) = (id(a, "channel_id")?, id(a, "message_id")?);
            let emoji = pct(req_text(a, "emoji")?);
            let method = if name == "add_reaction" { Method::PUT } else { Method::DELETE };
            d.call(method, &format!("/channels/{channel}/messages/{message}/reactions/{emoji}/@me"), &[], None).await?;
            Ok(json!({"ok": true}))
        }
        "clear_messages" => clear_messages(d, a).await,
        "open_dm" => {
            let user = id(a, "user_id")?;
            let channel = d.post("/users/@me/channels", &json!({"recipients": [user]})).await?;
            Ok(json!({"channel_id": channel["id"]}))
        }
        "get_user" => {
            let user = id(a, "user_id")?;
            let mut query = vec![("with_mutual_guilds", "false".to_owned())];
            if let Some(guild) = opt_id(a, "guild_id")? {
                query.push(("guild_id", guild));
            }
            let profile = d.get(&format!("/users/{user}/profile"), &query).await?;
            Ok(compact(json!({
                "id": profile["user"]["id"],
                "username": profile["user"]["username"],
                "name": profile["user"]["global_name"],
                "bot": profile["user"]["bot"].as_bool().filter(|b| *b),
                "bio": profile["user"]["bio"],
                "pronouns": profile["user_profile"]["pronouns"],
                "server_nick": profile["guild_member"]["nick"],
                "joined_server": profile["guild_member"]["joined_at"],
            })))
        }
        "search_members" => {
            let guild = id(a, "guild_id")?;
            let query = vec![
                ("query", req_text(a, "query")?.to_owned()),
                ("limit", number(a, "limit", 10, 100).to_string()),
            ];
            let members = d.get(&format!("/guilds/{guild}/members/search"), &query).await?;
            Ok(compact(Value::Array(
                members
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        json!({
                            "id": m["user"]["id"], "username": m["user"]["username"],
                            "name": m["user"]["global_name"], "nick": m["nick"],
                            "bot": m["user"]["bot"].as_bool().filter(|b| *b),
                        })
                    })
                    .collect(),
            )))
        }
        "set_status" => set_status(ctx, a).await,
        "recent_events" => {
            let since = a.get("since").and_then(Value::as_u64).unwrap_or(0);
            let events: Vec<Value> = ctx.shared.with(|s| {
                s.events.iter().filter(|e| e["n"].as_u64().unwrap_or(0) > since).cloned().collect()
            });
            Ok(compact(Value::Array(events)))
        }
        "discord_api" => raw(d, a).await,
        other => bail!("unknown tool `{other}`"),
    }
}

fn rows_position(channels: &Value, id: &str) -> i64 {
    channels
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["id"].as_str() == Some(id))
        .and_then(|c| c["position"].as_i64())
        .unwrap_or(0)
}

async fn set_status(ctx: &Ctx, a: &Value) -> Result<Value> {
    let mut changed = Map::new();
    if let Some(status) = text(a, "status") {
        if !["online", "idle", "dnd", "invisible"].contains(&status) {
            bail!("status must be online, idle, dnd or invisible");
        }
        ctx.discord
            .call(Method::PATCH, "/users/@me/settings", &[], Some(&json!({"status": status})))
            .await?;
        if let Some(gateway) = &ctx.gateway {
            let _ = gateway.send(Cmd::Presence(status.to_owned()));
        }
        changed.insert("status".into(), status.into());
    }
    if let Some(custom) = text(a, "custom_status") {
        let body = if custom.is_empty() {
            json!({"custom_status": null})
        } else {
            json!({"custom_status": {"text": custom, "expires_at": null}})
        };
        ctx.discord.call(Method::PATCH, "/users/@me/settings", &[], Some(&body)).await?;
        changed.insert("custom_status".into(), custom.into());
    }
    if changed.is_empty() {
        bail!("give status and/or custom_status");
    }
    Ok(Value::Object(changed))
}

async fn clear_messages(d: &Discord, a: &Value) -> Result<Value> {
    let channel = id(a, "channel_id")?;
    let want = number(a, "limit", 25, CLEAR_CAP as u64) as usize;
    let only_mine = flag(a, "only_mine", true);
    let confirm = flag(a, "confirm", false);
    let me = d.my_id().await?;

    let mut found: Vec<Value> = Vec::new();
    let mut before: Option<String> = None;
    'pages: for _ in 0..10 {
        let mut query = vec![("limit", "100".to_owned())];
        if let Some(b) = &before {
            query.push(("before", b.clone()));
        }
        let page = d.get(&format!("/channels/{channel}/messages"), &query).await?;
        let page = page.as_array().cloned().unwrap_or_default();
        if page.is_empty() {
            break;
        }
        before = page.last().and_then(|m| m["id"].as_str()).map(str::to_owned);
        for m in &page {
            let deletable = matches!(m["type"].as_u64().unwrap_or(0), 0 | 19 | 20 | 23);
            let mine = m["author"]["id"].as_str() == Some(me.as_str());
            if deletable && (mine || !only_mine) {
                found.push(m.clone());
                if found.len() >= want {
                    break 'pages;
                }
            }
        }
        if page.len() < 100 {
            break;
        }
    }
    if !confirm {
        return Ok(json!({
            "dry_run": true,
            "would_delete": found.len(),
            "newest": found.first().map(shape_message),
            "oldest": found.last().map(shape_message),
            "hint": "call again with confirm: true to delete",
        }));
    }
    let (mut deleted, mut failed) = (0, 0);
    for m in &found {
        let message = m["id"].as_str().unwrap_or_default();
        match d.call(Method::DELETE, &format!("/channels/{channel}/messages/{message}"), &[], None).await {
            Ok(_) => deleted += 1,
            Err(error) => {
                failed += 1;
                if failed >= 5 {
                    bail!("stopped after {deleted} deletions, repeated errors: {error:#}");
                }
            }
        }
        tokio::time::sleep(util::jitter(Duration::from_millis(1100), 0.7, 1.5)).await;
    }
    Ok(json!({"deleted": deleted, "failed": failed}))
}

/// Paths whose misuse could lock the owner out or destroy the account.
const BLOCKED: &[&str] = &[
    "/auth/", "/users/@me/mfa", "/users/@me/delete", "/users/@me/disable", "/users/@me/relationships/",
    "/users/@me/connections", "/users/@me/billing", "/users/@me/remote-auth", "/oauth2/", "/applications",
    "/store/", "/users/@me/guilds/premium", "/users/@me/devices", "/users/@me/sessions",
];

async fn raw(d: &Discord, a: &Value) -> Result<Value> {
    let method = match req_text(a, "method")?.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        other => bail!("unsupported method {other}"),
    };
    let path = req_text(a, "path")?;
    if !path.starts_with('/') || path.contains("..") || path.contains("://") || path.contains('\\') {
        bail!("path must start with / and stay inside the API");
    }
    let lower = path.to_ascii_lowercase();
    let unsafe_mode = std::env::var("AGENTICORD_UNSAFE").is_ok_and(|v| v == "1");
    if !unsafe_mode && BLOCKED.iter().any(|b| lower.starts_with(b) || lower.starts_with(&format!("/v9{b}"))) {
        bail!("that endpoint touches account security and is blocked (set AGENTICORD_UNSAFE=1 to allow it)");
    }
    // Deleting a whole server or channel is not a reversible chat action.
    if !unsafe_mode && method == Method::DELETE {
        let parts: Vec<&str> = lower.trim_matches('/').split('/').collect();
        if matches!(parts.as_slice(), ["guilds", _] | ["channels", _]) {
            bail!("deleting a whole server or channel is blocked (set AGENTICORD_UNSAFE=1 to allow it)");
        }
    }
    let query: Vec<(String, String)> = a["query"]
        .as_object()
        .map(|q| {
            q.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let query_refs: Vec<(&str, String)> = query.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let body = a.get("body").filter(|b| !b.is_null());
    d.call(method, path, &query_refs, body).await
}
