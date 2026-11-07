//! The Model Context Protocol, served two ways: JSON-RPC over HTTP (one POST
//! per request, no streaming) and over stdin/stdout.

use crate::{info, tools::{self, Ctx}};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Versions this server can speak, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Longest tool answer returned, in characters.
const OUTPUT_CAP: usize = 60_000;

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one JSON-RPC message. Notifications get no answer.
pub async fn handle(ctx: &Ctx, request: Value) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            let asked = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "agenticord", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Acts as the user's Discord account. Prefer read tools first. Messages are sent from the real account, so confirm before sending or deleting on the user's behalf unless told otherwise.",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools::list()}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let args = request["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = match tools::call(ctx, name, args).await {
                Ok(Value::String(text)) => (text, false),
                Ok(value) => (value.to_string(), false),
                Err(error) => (format!("Error: {error:#}"), true),
            };
            let text = if text.chars().count() > OUTPUT_CAP {
                format!("{}\n[truncated]", crate::util::clip(&text, OUTPUT_CAP))
            } else {
                text
            };
            json!({"content": [{"type": "text", "text": text}], "isError": is_error})
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

async fn handle_any(ctx: &Ctx, body: Value) -> Option<Value> {
    match body {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(answer) = handle(ctx, item).await {
                    out.push(answer);
                }
            }
            (!out.is_empty()).then_some(Value::Array(out))
        }
        single => handle(ctx, single).await,
    }
}

// ---- HTTP ------------------------------------------------------------------

struct App {
    ctx: Ctx,
    token: String,
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| {
            // Compare without stopping at the first difference.
            given.len() == token.len()
                && given.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
        })
}

async fn post_mcp(State(app): State<Arc<App>>, headers: HeaderMap, body: String) -> Response {
    if !authorized(&headers, &app.token) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    }
    let Ok(body) = serde_json::from_str::<Value>(&body) else {
        let answer = error(Value::Null, -32700, "parse error");
        return ([(header::CONTENT_TYPE, "application/json")], answer.to_string()).into_response();
    };
    match handle_any(&app.ctx, body).await {
        Some(answer) => ([(header::CONTENT_TYPE, "application/json")], answer.to_string()).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn method_not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "this server answers POST only").into_response()
}

pub async fn serve_http(ctx: Ctx, token: String, bind: &str, shutdown: impl Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
    let app = Arc::new(App { ctx, token });
    let router = Router::new()
        .route("/mcp", post(post_mcp).get(method_not_allowed))
        .route("/health", get(|| async { "ok" }))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("MCP server listening on http://{bind}/mcp");
    axum::serve(listener, router).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

// ---- stdio -----------------------------------------------------------------

pub async fn serve_stdio(ctx: Ctx) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(body) => handle_any(&ctx, body).await,
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
        };
        if let Some(answer) = answer {
            stdout.write_all(answer.to_string().as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
