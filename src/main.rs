//! Agenticord: a tiny headless Discord user-account client for AI agents.

mod config;
mod discord;
mod fingerprint;
mod gateway;
mod login;
mod mcp;
mod state;
mod tools;
#[macro_use]
mod util;

use anyhow::{Context, Result, bail};
use discord::Discord;
use gateway::Cmd;
use serde_json::Value;
use state::Shared;
use std::{sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, sync::mpsc};
use tools::Ctx;

const DEFAULT_BIND: &str = "127.0.0.1:8790";

const HELP: &str = "agenticord: a tiny headless Discord client for AI agents

USAGE
  agenticord login                 log in by scanning a QR code with the Discord phone app
  agenticord login --token-stdin   log in with a token read from standard input
  agenticord logout                forget the saved login on this machine
  agenticord whoami                show which account the saved login is
  agenticord serve [--bind ADDR]   run the MCP server over HTTP (default 127.0.0.1:8790)
  agenticord stdio                 run the MCP server over stdin/stdout
  agenticord tools                 list the tools
  agenticord call TOOL [JSON]      run one tool and print the result
  agenticord mcp-info              show the URL, token and a command to add it to Claude Code

ENVIRONMENT
  AGENTICORD_HOME    state directory (default ~/.config/agenticord)
  AGENTICORD_TOKEN   Discord token, instead of the saved login
  AGENTICORD_BIND    default address for `serve`
  AGENTICORD_LOG     set to `debug` for request logging
  AGENTICORD_UNSAFE  set to 1 to let discord_api reach account-security endpoints
";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = run(args).await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("login") => login_command(&args[1..]).await,
        Some("logout") => {
            config::remove("token");
            println!("Removed the saved login from {}.", config::dir().display());
            println!("The token itself stays valid until you log out of that session in Discord.");
            Ok(())
        }
        Some("whoami") => {
            let discord = connect().await?;
            let me = discord.me().await?;
            println!(
                "{} (@{}) id {}",
                me["global_name"].as_str().unwrap_or(""),
                me["username"].as_str().unwrap_or("?"),
                me["id"].as_str().unwrap_or("?")
            );
            Ok(())
        }
        Some("serve") => serve(&args[1..]).await,
        Some("stdio") => {
            let ctx = offline_ctx().await?;
            mcp::serve_stdio(ctx).await
        }
        Some("tools") => {
            for tool in tools::list() {
                println!("{:<18} {}", tool["name"].as_str().unwrap_or(""), tool["description"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Some("call") => {
            let name = args.get(1).context("usage: agenticord call TOOL [JSON]")?;
            let arguments: Value = match args.get(2) {
                Some(text) => serde_json::from_str(text).context("the arguments are not valid JSON")?,
                None => serde_json::json!({}),
            };
            let ctx = offline_ctx().await?;
            let value = tools::call(&ctx, name, arguments).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Some("mcp-info") => {
            let token = config::mcp_token()?;
            let bind = bind_address(&[]);
            let port = bind.rsplit(':').next().unwrap_or("8790");
            println!("URL:    http://127.0.0.1:{port}/mcp");
            println!("Token:  {token}");
            println!();
            println!("Add to Claude Code:");
            println!("  claude mcp add --transport http --scope user agenticord http://127.0.0.1:{port}/mcp --header \"Authorization: Bearer {token}\"");
            println!();
            println!("Or run it as a stdio server (no daemon):");
            println!("  claude mcp add --scope user agenticord -- agenticord stdio");
            Ok(())
        }
        Some("--version" | "-V" | "version") => {
            println!("agenticord {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None | Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`; try `agenticord help`"),
    }
}

async fn connect() -> Result<Arc<Discord>> {
    let token = config::discord_token().context("not logged in; run `agenticord login` first")?;
    Discord::connect(Some(token)).await
}

/// A context with no gateway: enough for every tool except live events.
async fn offline_ctx() -> Result<Ctx> {
    Ok(Ctx { discord: connect().await?, shared: Arc::new(Shared::new()), gateway: None })
}

async fn login_command(args: &[String]) -> Result<()> {
    let token = if args.iter().any(|a| a == "--token-stdin") {
        let mut text = String::new();
        tokio::io::stdin().read_to_string(&mut text).await?;
        text
    } else {
        login::qr().await?
    };
    let who = login::save_token(&token).await?;
    println!("Logged in as {who}.");
    println!("Next: `agenticord serve`, then `agenticord mcp-info` for the Claude Code command.");
    Ok(())
}

fn bind_address(args: &[String]) -> String {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--bind" {
            if let Some(address) = iter.next() {
                return address.clone();
            }
        } else if let Some(address) = arg.strip_prefix("--bind=") {
            return address.to_owned();
        }
    }
    std::env::var("AGENTICORD_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn serve(args: &[String]) -> Result<()> {
    let bind = bind_address(args);
    let mcp_token = config::mcp_token()?;
    let discord = connect().await?;
    // Debugging aid: start without checking the token (the gateway will
    // then report Discord's verdict itself).
    if std::env::var("AGENTICORD_NO_VERIFY").is_err() {
        let me = discord.me().await.context("Discord rejected the saved login; run `agenticord login` again")?;
        info!(
            "account {} (@{}), web build {}",
            me["global_name"].as_str().unwrap_or(""),
            me["username"].as_str().unwrap_or("?"),
            discord.fp.build_number
        );
    }
    let shared = Arc::new(Shared::new());
    let (commands, receiver) = mpsc::unbounded_channel();
    let gateway = tokio::spawn(gateway::run(discord.clone(), shared.clone(), receiver));
    let ctx = Ctx { discord, shared, gateway: Some(commands.clone()) };
    mcp::serve_http(ctx, mcp_token, &bind, shutdown_signal()).await?;
    info!("shutting down");
    let _ = commands.send(Cmd::Close);
    let _ = tokio::time::timeout(Duration::from_secs(3), gateway).await;
    Ok(())
}
