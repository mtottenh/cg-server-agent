//! The agent run loop: outbound WSS to the portal, heartbeats, commands.
//!
//! Protocol per docs/matchzy-integration.md §5.2. The portal sends
//! `{id, cmd, args}` frames; we answer `{id, ok, output|error}` and push
//! `{type: "heartbeat", ...}` every `heartbeat_secs`, plus the update
//! frames the watcher in `update.rs` produces (README, "Protocol").

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::rcon;
use crate::update::{self, Event, SentThisConnection, Updater};

/// Makes a running server re-read the MatchZy config after `repair-config`
/// changed the file under it (`needs_reload`). Agent-internal: it never goes
/// through `exec_refusal`, which keeps refusing `exec` from the portal.
const RELOAD_MATCHZY_CONFIG: &str = "exec MatchZy/config.cfg";

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
    let updater = Arc::new(Updater::new(update::Paths::default()));
    // The watcher outlives every connection: update jobs run outside this
    // process and finish whether or not the portal is reachable.
    let mut events = update::spawn_watcher(updater.clone());
    tokio::spawn(reload_when_idle(
        updater.clone(),
        config.rcon_addr.clone(),
        config.rcon_password.clone(),
    ));

    let mut backoff_secs = 1u64;
    loop {
        match connect_and_serve(&config, &updater, &mut events).await {
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

/// After a `repair-config` the file is right but the running server has not
/// read it. Re-executing the MatchZy config resets settings a live match
/// depends on, so it waits until MatchZy reports no match loaded; and it is
/// retried for as long as RCON is unreachable (every 10 s; every 5 min once
/// MatchZy itself has been silent for a few minutes — see `reload_step`). Independent of the portal
/// connection: a repair the agent started on its own while offline still
/// takes effect.
async fn reload_when_idle(updater: Arc<Updater>, rcon_addr: String, rcon_password: String) {
    // Since when MatchZy has not been answering `get5_status` although RCON
    // works (the plugin stack is not loaded).
    let mut unanswered_since: Option<Instant> = None;
    loop {
        let unanswered_for = unanswered_since.map(|at| at.elapsed());
        tokio::time::sleep(update::reload_retry_delay(unanswered_for)).await;
        if !updater.reload_pending() {
            unanswered_since = None;
            continue;
        }
        let status = match rcon::exec(&rcon_addr, &rcon_password, "get5_status").await {
            Ok(output) => extract_json(&output),
            Err(e) => {
                tracing::debug!(error = %e, "MatchZy config reload deferred: rcon unreachable");
                continue;
            }
        };
        let step = update::reload_step(status.as_ref(), unanswered_for);
        if status.is_some() {
            unanswered_since = None;
        } else if unanswered_since.is_none() {
            unanswered_since = Some(Instant::now());
        }
        match step {
            update::ReloadStep::Wait => {
                tracing::debug!(
                    "MatchZy config reload deferred: a match is loaded, or MatchZy is not answering"
                );
            }
            update::ReloadStep::Done => {
                updater.reload_done();
                tracing::info!(
                    "MatchZy was loaded afresh and has read its config; no reload needed"
                );
            }
            update::ReloadStep::Exec => {
                match rcon::exec(&rcon_addr, &rcon_password, RELOAD_MATCHZY_CONFIG).await {
                    Ok(_) => {
                        updater.reload_done();
                        tracing::info!("reloaded the MatchZy config over RCON");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not reload the MatchZy config; will retry");
                    }
                }
            }
        }
    }
}

/// Send one JSON frame; `false` means the connection is gone.
async fn send_json<S>(sink: &mut S, frame: &serde_json::Value) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    sink.send(Message::Text(frame.to_string().into()))
        .await
        .is_ok()
}

/// Forward one watcher event to the portal; `false` means the connection
/// is gone. A result that does not get through is not lost: the helper's own
/// are replayed from its record, the agent's from the copy it keeps, on
/// every connect until the portal acknowledges them (`update_ack`).
async fn deliver<S>(
    sink: &mut S,
    updater: &Updater,
    sent: &mut SentThisConnection,
    event: Event,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    match event {
        Event::Progress(frame) => send_json(sink, &frame).await,
        Event::CheckDone => send_json(sink, &updater.update_state_frame()).await,
        // Once per connection: it may already have gone out in the replay
        // on connect while the watcher's copy was still queued.
        Event::Result(result) => {
            !sent.first_time(&result) || send_json(sink, &update::result_frame(&result)).await
        }
    }
}

async fn connect_and_serve(
    config: &RunConfig,
    updater: &Updater,
    events: &mut mpsc::Receiver<Event>,
) -> Result<()> {
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

    // On every (re)connect: where updates stand, and any job that finished
    // while the channel was down. Old portals ignore frame types they do
    // not know.
    let mut sent = SentThisConnection::default();
    if updater.capable() && !send_json(&mut sink, &updater.update_state_frame()).await {
        return Ok(());
    }
    // Everything the portal has not acknowledged (at most the last job and
    // the last agent-made result), every time: a frame that was written to
    // the socket is not a result the portal has stored.
    for result in updater.unacked_results() {
        if sent.first_time(&result) && !send_json(&mut sink, &update::result_frame(&result)).await {
            return Ok(());
        }
    }
    // Have the helper look at the MatchZy config now, so the heartbeats of
    // this connection report it from a fresh reading.
    updater.verify_config(true, Instant::now()).await;

    // Whatever the watcher queued while we were disconnected: progress from
    // back then is stale (the job may be long over), the rest still counts.
    while let Ok(event) = events.try_recv() {
        if !matches!(event, Event::Progress(_))
            && !deliver(&mut sink, updater, &mut sent, event).await
        {
            return Ok(());
        }
    }

    loop {
        tokio::select! {
            Some(event) = events.recv() => {
                if !deliver(&mut sink, updater, &mut sent, event).await {
                    return Ok(());
                }
            }
            _ = heartbeat.tick() => {
                let frame = heartbeat_frame(config, updater).await;
                if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                    return Ok(());
                }
                metrics::counter!("agent_heartbeats_sent_total").increment(1);
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(reply) = handle_frame(config, updater, text.as_str()).await
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

async fn heartbeat_frame(config: &RunConfig, updater: &Updater) -> serde_json::Value {
    let mut frame = status_frame(config).await;
    let now = Instant::now();
    // What the helper last found in the MatchZy config file.
    let health = updater.config_health();
    // Someone ran `mods-update` by hand and the demo-upload block is gone
    // (or the portal's overrides are not in the file): put it back now
    // rather than lose demos until an admin notices. It restarts nothing,
    // and the job is reported like any other (trigger `auto_repair`).
    if updater.claim_auto_repair(health, now) {
        let job_id = update::new_job_id();
        tracing::warn!(
            %job_id,
            demo_upload_wired = ?health.demo_upload_wired,
            overrides_applied = ?health.overrides_applied,
            "managed MatchZy config blocks are not in place; starting repair-config"
        );
        if let Err(e) = updater.start("repair-config", &job_id, "auto_repair").await {
            tracing::warn!(error = %e, "could not start the automatic config repair");
        }
    } else {
        // Keep the reading fresh (every 5 minutes, never during a job).
        updater.verify_config(false, now).await;
    }
    frame["capabilities"] = json!(updater.capabilities());
    frame["demo_upload_wired"] = json!(health.demo_upload_wired);
    frame["overrides_applied"] = json!(health.overrides_applied);
    frame["reload_pending"] = json!(updater.reload_pending());
    frame["update_job"] = json!(updater.running_job());
    frame
}

async fn status_frame(config: &RunConfig) -> serde_json::Value {
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

async fn handle_frame(
    config: &RunConfig,
    updater: &Updater,
    text: &str,
) -> Option<serde_json::Value> {
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
        c @ ("load_match"
        | "end_match"
        | "status"
        | "load_backup"
        | "roster_edit"
        | "exec"
        | "update_check"
        | "update_start"
        | "update_status"
        | "update_ack"
        | "set_matchzy_overrides") => c,
        _ => "unknown",
    }
    .to_owned();
    let start = std::time::Instant::now();
    let result = execute_command(config, updater, frame).await;
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

/// The update commands: none of them touches RCON, all of them return at
/// once (the helper runs outside this process; its progress arrives as
/// pushed frames). `output` is a JSON document.
async fn execute_update_command(updater: &Updater, frame: &CommandFrame) -> Result<String> {
    tracing::info!(cmd = %frame.cmd, "executing");
    match frame.cmd.as_str() {
        "update_check" => {
            let job_id = update::new_job_id();
            updater.start("check", &job_id, "portal").await?;
            Ok(json!({ "started": true, "job_id": job_id, "operation": "check" }).to_string())
        }
        "update_start" => {
            let args = frame.args.as_ref().context("update_start requires args")?;
            let operation = args["operation"].as_str().context("missing operation")?;
            let job_id = args["job_id"].as_str().context("missing job_id")?;
            anyhow::ensure!(
                update::OPERATIONS.contains(&operation),
                "unknown operation (expected one of: {})",
                update::OPERATIONS.join(", ")
            );
            updater.start(operation, job_id, "portal").await?;
            Ok(json!({ "started": true, "job_id": job_id, "operation": operation }).to_string())
        }
        "update_status" => Ok(json!(updater.read_state()).to_string()),
        "update_ack" => {
            // The portal has stored this job's result: stop replaying it.
            let args = frame.args.as_ref().context("update_ack requires args")?;
            let job_id = args["job_id"].as_str().context("missing job_id")?;
            anyhow::ensure!(
                update::valid_job_id(job_id),
                "job_id must be a lowercase UUID"
            );
            updater.ack(job_id);
            Ok(json!({ "acked": job_id }).to_string())
        }
        "set_matchzy_overrides" => {
            let args = frame
                .args
                .as_ref()
                .context("set_matchzy_overrides requires args")?;
            let overrides = args.get("overrides").context("missing overrides")?;
            anyhow::ensure!(
                updater.capable(),
                "refused: this host has no LinuxGSM update helper configured"
            );
            let written = updater.write_overrides(overrides)?;
            // Apply now if nothing else is running; otherwise the file waits
            // for the next config reconcile (any mods/all/repair-config).
            let job_id = update::new_job_id();
            Ok(
                match updater.start("repair-config", &job_id, "overrides").await {
                    Ok(()) => json!({
                        "written": written, "started": true,
                        "job_id": job_id, "operation": "repair-config",
                    }),
                    Err(e) => json!({
                        "written": written, "started": false,
                        "job_id": null, "reason": e.to_string(),
                    }),
                }
                .to_string(),
            )
        }
        other => anyhow::bail!("unknown command: {other}"),
    }
}

async fn execute_command(
    config: &RunConfig,
    updater: &Updater,
    frame: CommandFrame,
) -> Result<String> {
    if matches!(
        frame.cmd.as_str(),
        "update_check" | "update_start" | "update_status" | "update_ack" | "set_matchzy_overrides"
    ) {
        return execute_update_command(updater, &frame).await;
    }
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
