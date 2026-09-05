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
        metrics::gauge!("agent_ws_connected").set(0.0);
        metrics::counter!("agent_reconnects_total").increment(1);
        let jitter = rand::rng().random_range(0..=backoff_secs);
        let wait = backoff_secs + jitter;
        metrics::gauge!("agent_backoff_seconds").set(wait as f64);
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
    metrics::gauge!("agent_ws_connected").set(1.0);
    metrics::gauge!("agent_backoff_seconds").set(0.0);

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
                metrics::counter!("agent_heartbeats_sent_total").increment(1);
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
/// The most of CS2's `status` output a heartbeat carries. A full server
/// prints well under this; the bound keeps a runaway console from bloating
/// every heartbeat.
const STATUS_OUTPUT_MAX: usize = 8 * 1024;

async fn heartbeat_frame(config: &RunConfig) -> serde_json::Value {
    match rcon::exec(&config.rcon_addr, &config.rcon_password, "get5_status").await {
        Ok(output) => {
            // get5_status prints a single JSON object; tolerate surrounding
            // console noise by extracting the outermost braces.
            let status = extract_json(&output);
            // CS2's own `status` is what knows the map and who is connected
            // (get5_status has neither). Forwarded raw and bounded: the
            // format is CS2's, and the portal parses it in one place.
            let status_output = rcon::exec(&config.rcon_addr, &config.rcon_password, "status")
                .await
                .ok()
                .map(|o| truncate_chars(&o, STATUS_OUTPUT_MAX));
            json!({
                "type": "heartbeat",
                "agent_version": env!("CARGO_PKG_VERSION"),
                "rcon_ok": true,
                "get5_status": status,
                "status_output": status_output,
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

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &s[..end])
}

/// Why a raw `exec` command is refused, if it is.
///
/// The portal holds the RCON password, the webhook and demo URLs and the
/// match lifecycle; a portal bug or a hostile admin session must not be
/// able to rotate the password, repoint the webhooks or drop the box out
/// from under a match through the passthrough. The portal applies the same
/// rule first; this copy is defence in depth. One command per frame: `;`
/// and control characters are refused so the check sees a single verb.
pub fn exec_refusal(command: &str) -> Option<String> {
    if command.chars().any(|c| c.is_control()) {
        return Some("control characters are not allowed".to_string());
    }
    if command.contains(';') {
        return Some("one command per request; ';' is not allowed".to_string());
    }
    let verb = command
        .trim_start()
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if verb.is_empty() {
        return Some("empty command".to_string());
    }
    const PORTAL_OWNED: &[&str] = &[
        "rcon_password",
        "sv_password",
        "tv_password",
        "matchzy_loadmatch_url",
        "matchzy_loadmatch",
        "matchzy_loadbackup_url",
        "sv_downloadurl",
        "host_writeconfig",
    ];
    const PORTAL_OWNED_PREFIXES: &[&str] = &[
        "sv_rcon_",
        "matchzy_remote_",
        "matchzy_demo_upload_",
        "logaddress_",
    ];
    const NO_ESCAPE: &[&str] = &["alias", "exec"];
    const PROCESS: &[&str] = &["quit", "exit", "_restart", "killserver", "crash"];
    if PORTAL_OWNED.contains(&verb.as_str())
        || PORTAL_OWNED_PREFIXES.iter().any(|p| verb.starts_with(p))
    {
        return Some(format!("{verb} is portal-owned"));
    }
    if NO_ESCAPE.contains(&verb.as_str()) {
        return Some(format!("{verb} could run commands the check cannot see"));
    }
    if PROCESS.contains(&verb.as_str()) {
        return Some(format!("{verb} would end the server process"));
    }
    None
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
    // Bounded `command` label: the known portal verbs only, never the raw
    // string (a hostile portal frame must not mint metric series).
    let command = match frame.cmd.as_str() {
        c @ ("load_match" | "end_match" | "status" | "load_backup" | "roster_edit" | "exec") => c,
        _ => "unknown",
    }
    .to_owned();
    let start = std::time::Instant::now();
    let result = execute_command(config, frame).await;
    metrics::histogram!("agent_rcon_duration_seconds", "command" => command.clone())
        .record(start.elapsed().as_secs_f64());
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!(
        "agent_rcon_commands_total",
        "command" => command,
        "outcome" => outcome,
    )
    .increment(1);
    match result {
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
            let command = args["command"]
                .as_str()
                .context("missing command")?
                .to_string();
            if let Some(reason) = exec_refusal(&command) {
                anyhow::bail!("refused: {reason}");
            }
            command
        }
        other => anyhow::bail!("unknown command: {other}"),
    };

    tracing::info!(cmd = %frame.cmd, "executing");
    rcon::exec(&config.rcon_addr, &config.rcon_password, &command).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_console_commands_pass() {
        for c in [
            "status",
            "mp_warmuptime 60",
            "changelevel de_mirage",
            "host_workshop_map 3070284539",
            "say \"gl hf\"",
        ] {
            assert_eq!(exec_refusal(c), None, "{c}");
        }
    }

    #[test]
    fn portal_owned_cvars_are_refused_whatever_the_case() {
        for c in [
            "rcon_password hunter2",
            "RCON_PASSWORD x",
            "sv_password x",
            "tv_password x",
            "sv_rcon_whitelist_address 1.2.3.4",
            "matchzy_remote_log_url https://evil",
            "matchzy_demo_upload_url https://evil",
            "matchzy_loadmatch_url https://evil",
            "logaddress_add 1.2.3.4:1234",
        ] {
            assert!(
                exec_refusal(c).is_some_and(|r| r.contains("portal-owned")),
                "{c}"
            );
        }
    }

    #[test]
    fn escapes_and_process_commands_are_refused() {
        assert!(exec_refusal("alias x rcon_password y").is_some());
        assert!(exec_refusal("exec server.cfg").is_some());
        assert!(exec_refusal("quit").is_some());
        assert!(exec_refusal("_restart").is_some());
        assert!(exec_refusal("mp_warmuptime 60; rcon_password x").is_some());
        assert!(exec_refusal("status\r").is_some());
        assert!(exec_refusal("   ").is_some());
    }

    #[test]
    fn truncation_keeps_char_boundaries() {
        let s = "é".repeat(10);
        let t = truncate_chars(&s, 5);
        assert!(t.starts_with("éé"));
        assert!(t.ends_with("[truncated]"));
        assert_eq!(truncate_chars("short", 100), "short");
    }
}
