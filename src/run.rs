//! The agent run loop: outbound WSS to the portal, heartbeats, commands.
//!
//! Protocol per docs/matchzy-integration.md §5.2. The portal sends
//! `{id, cmd, args}` frames; we answer `{id, ok, output|error}` and push
//! `{type: "heartbeat", ...}` every `heartbeat_secs`.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::rcon;

pub struct RunConfig {
    pub portal_url: String,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub ca_path: Option<String>,
    pub rcon_addr: String,
    pub rcon_password: String,
    pub heartbeat_secs: u64,
    /// Dev mode: plain ws:// with an X-Dev-Server-Id header instead of mTLS.
    pub dev_server_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommandFrame {
    id: String,
    cmd: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

pub async fn run(config: RunConfig) -> Result<()> {
    let mut backoff_secs = 1u64;
    loop {
        match connect_and_serve(&config).await {
            Ok(()) => {
                tracing::info!("connection closed by portal; reconnecting");
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, "connection failed");
            }
        }
        let jitter = rand::rng().random_range(0..=backoff_secs);
        let wait = backoff_secs + jitter;
        tracing::info!(seconds = wait, "reconnecting after backoff");
        tokio::time::sleep(Duration::from_secs(wait)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

async fn connect_and_serve(config: &RunConfig) -> Result<()> {
    let mut request = config
        .portal_url
        .as_str()
        .into_client_request()
        .context("invalid portal URL")?;

    let connector = if let Some(dev_id) = &config.dev_server_id {
        request
            .headers_mut()
            .insert("X-Dev-Server-Id", dev_id.parse().context("invalid dev id")?);
        None
    } else {
        let (cert, key, ca) = match (&config.cert_path, &config.key_path, &config.ca_path) {
            (Some(c), Some(k), Some(a)) => (c, k, a),
            _ => anyhow::bail!(
                "PORTAL_AGENT_CERT/KEY/CA are required (or --dev-server-id for dev mode)"
            ),
        };
        Some(tokio_tungstenite::Connector::Rustls(
            crate::tls::client_config(cert, key, ca)?,
        ))
    };

    let (ws, _response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
            .await
            .context("websocket connect")?;
    tracing::info!(url = %config.portal_url, "connected to portal");

    let (mut sink, mut stream) = ws.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(config.heartbeat_secs));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let frame = heartbeat_frame(config).await;
                if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                    return Ok(());
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(reply) = handle_frame(config, text.as_str()).await
                            && sink.send(Message::Text(reply.to_string().into())).await.is_err()
                        {
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("websocket read"),
                }
            }
        }
    }
}

/// Build the heartbeat frame: RCON reachability + raw `get5_status` JSON.
async fn heartbeat_frame(config: &RunConfig) -> serde_json::Value {
    match rcon::exec(&config.rcon_addr, &config.rcon_password, "get5_status").await {
        Ok(output) => {
            // get5_status prints a single JSON object; tolerate surrounding
            // console noise by extracting the outermost braces.
            let status = extract_json(&output);
            json!({
                "type": "heartbeat",
                "agent_version": env!("CARGO_PKG_VERSION"),
                "rcon_ok": true,
                "get5_status": status,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "rcon unreachable");
            json!({
                "type": "heartbeat",
                "agent_version": env!("CARGO_PKG_VERSION"),
                "rcon_ok": false,
            })
        }
    }
}

fn extract_json(output: &str) -> Option<serde_json::Value> {
    let start = output.find('{')?;
    let end = output.rfind('}')?;
    serde_json::from_str(&output[start..=end]).ok()
}

/// Quote an argument for the CS2 console: reject strings that could break
/// out of the quoting — the portal only ever sends URLs and header values,
/// so embedded quotes/newlines/semicolons are treated as attacks.
fn console_quote(arg: &str) -> Result<String> {
    if arg.contains('"') || arg.contains('\n') || arg.contains(';') {
        anyhow::bail!("argument contains console metacharacters");
    }
    Ok(format!("\"{arg}\""))
}

async fn handle_frame(config: &RunConfig, text: &str) -> Option<serde_json::Value> {
    let frame: CommandFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(error = %e, "unparseable portal frame");
            return None;
        }
    };
    let id = frame.id.clone();
    match execute_command(config, frame).await {
        Ok(output) => Some(json!({ "id": id, "ok": true, "output": output })),
        Err(e) => Some(json!({ "id": id, "ok": false, "error": e.to_string() })),
    }
}

async fn execute_command(config: &RunConfig, frame: CommandFrame) -> Result<String> {
    let command = match frame.cmd.as_str() {
        "load_match" => {
            let args = frame.args.as_ref().context("load_match requires args")?;
            let url = args["url"].as_str().context("missing url")?;
            let header_name = args["header_name"].as_str().unwrap_or("");
            let header_value = args["header_value"].as_str().unwrap_or("");
            if header_name.is_empty() {
                format!("matchzy_loadmatch_url {}", console_quote(url)?)
            } else {
                format!(
                    "matchzy_loadmatch_url {} {} {}",
                    console_quote(url)?,
                    console_quote(header_name)?,
                    console_quote(header_value)?
                )
            }
        }
        "end_match" => "css_endmatch".to_string(),
        "status" => "get5_status".to_string(),
        "load_backup" => {
            let args = frame.args.as_ref().context("load_backup requires args")?;
            let url = args["url"].as_str().context("missing url")?;
            let header_name = args["header_name"].as_str().unwrap_or("");
            let header_value = args["header_value"].as_str().unwrap_or("");
            if header_name.is_empty() {
                format!("matchzy_loadbackup_url {}", console_quote(url)?)
            } else {
                format!(
                    "matchzy_loadbackup_url {} {} {}",
                    console_quote(url)?,
                    console_quote(header_name)?,
                    console_quote(header_value)?
                )
            }
        }
        "roster_edit" => {
            // Remove-then-add so the listed count never exceeds
            // players_per_team; outputs are concatenated.
            let args = frame.args.as_ref().context("roster_edit requires args")?;
            let mut outputs = Vec::new();
            if let Some(remove) = args["remove"].as_array() {
                for steam in remove {
                    let steam = steam
                        .as_str()
                        .context("remove entries are steamid64 strings")?;
                    anyhow::ensure!(
                        steam.chars().all(|c| c.is_ascii_digit()),
                        "invalid steamid64 in remove list"
                    );
                    let out = rcon::exec(
                        &config.rcon_addr,
                        &config.rcon_password,
                        &format!("matchzy_removeplayer {steam}"),
                    )
                    .await?;
                    outputs.push(out);
                }
            }
            if let Some(add) = args["add"].as_array() {
                for entry in add {
                    let steam = entry["steamid64"]
                        .as_str()
                        .context("add entries need steamid64")?;
                    anyhow::ensure!(
                        steam.chars().all(|c| c.is_ascii_digit()),
                        "invalid steamid64 in add list"
                    );
                    let team = match entry["team"].as_str().unwrap_or("team1") {
                        "team2" => "team2",
                        "spec" => "spec",
                        _ => "team1",
                    };
                    // Display names are arbitrary user data: sanitize
                    // instead of rejecting (a ';' in a name must not park
                    // the substitution forever — review minor).
                    let raw_name = entry["name"].as_str().unwrap_or("player");
                    let name: String = raw_name
                        .chars()
                        .filter(|c| !matches!(c, '"' | ';' | '\n' | '\r'))
                        .collect();
                    let name = if name.trim().is_empty() {
                        "player"
                    } else {
                        name.trim()
                    };
                    let out = rcon::exec(
                        &config.rcon_addr,
                        &config.rcon_password,
                        &format!("matchzy_addplayer {steam} {team} \"{name}\""),
                    )
                    .await?;
                    outputs.push(out);
                }
            }
            return Ok(outputs.join("\n"));
        }
        "exec" => {
            let args = frame.args.as_ref().context("exec requires args")?;
            args["command"]
                .as_str()
                .context("missing command")?
                .to_string()
        }
        other => anyhow::bail!("unknown command: {other}"),
    };

    tracing::info!(cmd = %frame.cmd, "executing");
    rcon::exec(&config.rcon_addr, &config.rcon_password, &command).await
}
