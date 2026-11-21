//! Game-server updates through LinuxGSM: the agent's half.
//!
//! Design: docs/gameserver-lgsm-updates-design.md §6 (portal repo). The
//! root-owned helper `portal-gs-update` does the work and publishes a state
//! file; the agent only
//!
//! * starts the helper's systemd template unit (polkit lets `portal-agent`
//!   start exactly `portal-gs-update@<op>.service`, nothing else),
//! * hands it the portal's job id through `request.json` — never as an
//!   argument, nothing from the portal reaches a shell,
//! * relays the state file as `update_progress` / `update_result` /
//!   `update_state` frames, and
//! * writes the portal-managed MatchZy overrides file the helper folds into
//!   the MatchZy config.
//!
//! The helper runs outside this process, so a job survives an agent
//! restart, a dropped WebSocket and an agent upgrade. Everything here is
//! therefore reconstructed from files, not from memory.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Advertised in the heartbeat when the helper and its config exist.
pub const CAPABILITY: &str = "lgsm_update";

/// Operations `update_start` accepts. `check` has its own command.
pub const OPERATIONS: [&str; 5] = ["lgsm", "game", "mods", "all", "repair-config"];

/// Minimum gap between `update_progress` frames for one job.
const PROGRESS_DEBOUNCE: Duration = Duration::from_secs(1);
/// How often the watcher looks at the state file.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// A started unit that has not written state after this long never ran
/// (lock held by cron, unit failed, polkit said no).
const PENDING_TIMEOUT: Duration = Duration::from_secs(60);
/// A `running` state whose helper process is gone for this long is dead
/// (SIGKILL, OOM). The grace covers the helper's own final write + exit.
const DEAD_HELPER_GRACE: Duration = Duration::from_secs(5);
/// Auto-repair of the managed config blocks: at most once per this long.
const AUTO_REPAIR_INTERVAL: Duration = Duration::from_secs(600);
/// How often the agent asks the helper to re-check the MatchZy config.
const VERIFY_INTERVAL: Duration = Duration::from_secs(300);
/// A `config.json` older than this says nothing about the config now.
const CONFIG_MAX_AGE: Duration = Duration::from_secs(900);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_OVERRIDES: usize = 64;
const MAX_OVERRIDE_VALUE_CHARS: usize = 256;
const MAX_OVERRIDE_NAME_CHARS: usize = 128;

/// Every path and program this module touches. `Default` is production;
/// tests point it at a temp directory and a fake `systemctl`.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Helper config; its presence is how a host opts in.
    pub conf: PathBuf,
    pub helper: PathBuf,
    /// Shared with the helper: `request.json` (ours), `state.json` and
    /// `config.json` (its).
    pub run_dir: PathBuf,
    /// The helper's: `check.json`, `installed.json`, `last-job.json`.
    pub state_dir: PathBuf,
    /// Portal-managed MatchZy overrides, written here, read by the helper.
    pub overrides: PathBuf,
    pub systemctl: PathBuf,
    /// Job id of the last finished job delivered to the portal.
    pub sent_marker: PathBuf,
    pub proc_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            conf: "/etc/portal/gs-update.conf".into(),
            helper: "/usr/sbin/portal-gs-update".into(),
            run_dir: "/run/portal-gs-update".into(),
            state_dir: "/var/lib/portal-gs-update".into(),
            overrides: "/etc/portal/agent/matchzy_overrides.cfg".into(),
            systemctl: "systemctl".into(),
            sent_marker: "/var/lib/portal-server-agent/last-sent-job".into(),
            proc_dir: "/proc".into(),
        }
    }
}

/// A unit we started whose helper has not published state yet.
#[derive(Clone, Debug)]
struct Pending {
    job_id: String,
    operation: String,
    since: Instant,
}

/// What the helper last found in the MatchZy config file
/// (`config.json`, written by `verify-config` and at the end of every job).
/// Each is `None` when unknown: no report, a stale one, or nothing to
/// compare against (no enrollment snippet / no overrides file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigHealth {
    /// The demo-upload block is in place and nothing after it overrides it.
    pub demo_upload_wired: Option<bool>,
    /// The portal's MatchZy overrides are in the config.
    pub overrides_applied: Option<bool>,
}

impl ConfigHealth {
    /// Something is definitely wrong that `repair-config` fixes.
    pub fn needs_repair(&self) -> bool {
        self.demo_upload_wired == Some(false) || self.overrides_applied == Some(false)
    }
}

pub struct Updater {
    paths: Paths,
    pending: Mutex<Option<Pending>>,
    last_auto_repair: Mutex<Option<Instant>>,
    last_verify: Mutex<Option<Instant>>,
    /// In-memory copy of the sent marker (the file may be unwritable in dev).
    last_sent: Mutex<Option<String>>,
    /// A result that could not be sent and is recorded nowhere else.
    undelivered: Mutex<Option<Value>>,
}

// --- small pure helpers ------------------------------------------------------

/// `^[0-9a-f-]{36}$` — the same check the helper applies.
pub fn valid_job_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// A random (version 4) UUID, for jobs the agent starts on its own.
pub fn new_job_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn unit_name(operation: &str) -> String {
    format!("portal-gs-update@{operation}.service")
}

/// `systemctl` arguments to start one operation. `--no-block`: the job runs
/// for minutes and must never hold up the command loop. `--no-ask-password`:
/// polkit either allows it or it fails; there is nobody to prompt.
pub fn systemctl_args(operation: &str) -> Vec<String> {
    vec![
        "--no-ask-password".into(),
        "start".into(),
        "--no-block".into(),
        unit_name(operation),
    ]
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "rolled_back")
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `contents` to `path` through a temp file in the same directory.
fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{:08x}.tmp", rand::random::<u32>()));
    let result = (|| -> Result<()> {
        std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// --- MatchZy overrides -------------------------------------------------------

/// Why an override cvar name is not acceptable, if it is not.
fn override_name_refusal(name: &str) -> Option<&'static str> {
    let rest = match name.strip_prefix("matchzy_") {
        Some(r) if !r.is_empty() => r,
        _ => return Some("name must match ^matchzy_[a-z0-9_]+$"),
    };
    if name.len() > MAX_OVERRIDE_NAME_CHARS
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Some("name must match ^matchzy_[a-z0-9_]+$");
    }
    // The portal owns these: demo upload and webhooks carry its tokens, the
    // load commands are the match lifecycle. Same list as `exec` refuses.
    if name.starts_with("matchzy_demo_upload_")
        || name.starts_with("matchzy_remote_")
        || name.starts_with("matchzy_loadmatch")
        || name == "matchzy_loadbackup_url"
    {
        return Some("cvar is portal-owned");
    }
    None
}

/// Validate `{cvar: value}` from the portal into `(cvar, value)` pairs,
/// sorted by name. All-or-nothing: one bad entry rejects the whole set, so
/// the file on disk is always exactly what the portal last sent.
pub fn validate_overrides(overrides: &Value) -> Result<Vec<(String, String)>> {
    let map = overrides
        .as_object()
        .context("overrides must be an object of cvar: value")?;
    if map.len() > MAX_OVERRIDES {
        bail!("too many overrides ({} > {MAX_OVERRIDES})", map.len());
    }
    let mut out = Vec::with_capacity(map.len());
    for (name, value) in map {
        if let Some(reason) = override_name_refusal(name) {
            // Names are attacker-influenced: bound what we echo back.
            let shown: String = name.chars().take(64).filter(|c| !c.is_control()).collect();
            bail!("override {shown:?} refused: {reason}");
        }
        let value = match value {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => bail!("override {name}: value must be a string, number or boolean"),
        };
        if value.chars().count() > MAX_OVERRIDE_VALUE_CHARS {
            bail!("override {name}: value longer than {MAX_OVERRIDE_VALUE_CHARS} characters");
        }
        if value
            .chars()
            .any(|c| c.is_control() || c == '"' || c == ';')
        {
            bail!("override {name}: value contains quotes, semicolons or control characters");
        }
        out.push((name.clone(), value));
    }
    out.sort();
    Ok(out)
}

/// The overrides file: one `cvar value` per line.
pub fn render_overrides(overrides: &[(String, String)]) -> String {
    overrides
        .iter()
        .map(|(name, value)| format!("{name} {value}\n"))
        .collect()
}

// --- state file → frames -----------------------------------------------------

/// What the watcher hands the connection loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A running job moved on: `update_progress`.
    Progress(Value),
    /// A job reached a terminal status: `update_result` (the state file).
    Result(Value),
    /// A `check` finished: time for a fresh `update_state`.
    CheckDone,
}

pub fn progress_frame(state: &Value) -> Value {
    json!({
        "type": "update_progress",
        "job_id": state.get("job_id"),
        "operation": state.get("operation"),
        "phase": state.get("phase"),
        "status": state.get("status"),
        "trigger": state.get("trigger"),
        "log_tail": state.get("log_tail"),
    })
}

pub fn result_frame(state: &Value) -> Value {
    let mut frame = state.clone();
    if let Some(map) = frame.as_object_mut() {
        map.insert("type".into(), json!("update_result"));
    }
    frame
}

/// Turns successive readings of the state file into events: debounced
/// progress while a job runs, exactly one result when it ends. Pure — the
/// clock and the "is the helper alive" answer are passed in.
#[derive(Default)]
pub struct Tracker {
    /// (job, phase, status, log tail) of the last progress frame emitted.
    last_progress: Option<(String, String, String, String)>,
    last_progress_at: Option<Instant>,
    /// Job whose terminal state has been handled.
    done: Option<String>,
    /// Job seen `running` with no helper process, and since when.
    dead_since: Option<(String, Instant)>,
}

impl Tracker {
    /// Start from what is already on disk: a job that finished before this
    /// process started is not news to the tracker. (Whether the *portal*
    /// has it is the sent marker's business — see `Updater::unsent_result`.)
    pub fn primed(state: Option<&Value>) -> Self {
        let mut tracker = Self::default();
        if let Some(state) = state
            && is_terminal(str_field(state, "status"))
        {
            tracker.done = Some(str_field(state, "job_id").to_string());
        }
        tracker
    }

    pub fn observe(
        &mut self,
        state: Option<&Value>,
        helper_alive: bool,
        now: Instant,
    ) -> Vec<Event> {
        let Some(state) = state else {
            return Vec::new();
        };
        let job_id = str_field(state, "job_id");
        let status = str_field(state, "status");
        let is_check = str_field(state, "operation") == "check";
        if job_id.is_empty() || self.done.as_deref() == Some(job_id) {
            return Vec::new();
        }

        if is_terminal(status) {
            self.done = Some(job_id.to_string());
            self.dead_since = None;
            return if is_check && status == "succeeded" {
                vec![Event::CheckDone]
            } else {
                vec![Event::Result(state.clone())]
            };
        }
        if status != "running" {
            return Vec::new();
        }

        if !helper_alive {
            match &self.dead_since {
                Some((id, since)) if id == job_id => {
                    if now.duration_since(*since) >= DEAD_HELPER_GRACE {
                        self.done = Some(job_id.to_string());
                        self.dead_since = None;
                        let mut dead = state.clone();
                        dead["status"] = json!("failed");
                        dead["agent_synthesized"] = json!(true);
                        dead["failed_phase"] = state.get("phase").cloned().unwrap_or(Value::Null);
                        dead["error"] = json!(
                            "the update helper exited without recording a result (killed or crashed)"
                        );
                        return vec![Event::Result(dead)];
                    }
                }
                _ => self.dead_since = Some((job_id.to_string(), now)),
            }
            return Vec::new();
        }
        self.dead_since = None;

        // A check reports through `update_state`; no progress frames.
        if is_check {
            return Vec::new();
        }
        let signature = (
            job_id.to_string(),
            str_field(state, "phase").to_string(),
            status.to_string(),
            str_field(state, "log_tail").to_string(),
        );
        if self.last_progress.as_ref() == Some(&signature) {
            return Vec::new();
        }
        let new_job = self.last_progress.as_ref().map(|s| s.0.as_str()) != Some(job_id);
        let due = self
            .last_progress_at
            .is_none_or(|at| now.duration_since(at) >= PROGRESS_DEBOUNCE);
        if !(new_job || due) {
            // Changed, but too soon: the next reading picks it up.
            return Vec::new();
        }
        self.last_progress = Some(signature);
        self.last_progress_at = Some(now);
        vec![Event::Progress(progress_frame(state))]
    }
}

// --- the updater -------------------------------------------------------------

impl Updater {
    pub fn new(paths: Paths) -> Self {
        let last_sent = std::fs::read_to_string(&paths.sent_marker)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            paths,
            pending: Mutex::new(None),
            last_auto_repair: Mutex::new(None),
            last_verify: Mutex::new(None),
            last_sent: Mutex::new(last_sent),
            undelivered: Mutex::new(None),
        }
    }

    /// `["lgsm_update"]` when both the helper and its config exist.
    pub fn capabilities(&self) -> Vec<&'static str> {
        if self.capable() {
            vec![CAPABILITY]
        } else {
            Vec::new()
        }
    }

    pub fn capable(&self) -> bool {
        self.paths.conf.is_file() && self.paths.helper.is_file()
    }

    pub fn read_state(&self) -> Option<Value> {
        read_json(&self.paths.run_dir.join("state.json"))
    }

    /// Is the helper that wrote this state still running? Its pid is in the
    /// state; the command line guards against a recycled pid.
    pub fn helper_alive(&self, state: &Value) -> bool {
        let Some(pid) = state.get("pid").and_then(Value::as_u64) else {
            return false;
        };
        std::fs::read(self.paths.proc_dir.join(pid.to_string()).join("cmdline"))
            .map(|cmdline| String::from_utf8_lossy(&cmdline).contains("portal-gs-update"))
            .unwrap_or(false)
    }

    /// The job in flight, as the heartbeat reports it: one the helper is
    /// running, or one we started that it has not picked up yet.
    pub fn running_job(&self) -> Option<Value> {
        let state = self.read_state();
        let pending = self.pending.lock().unwrap();
        self.running_job_in(state.as_ref(), &pending)
    }

    fn running_job_in(&self, state: Option<&Value>, pending: &Option<Pending>) -> Option<Value> {
        if let Some(state) = state
            && str_field(state, "status") == "running"
            && self.helper_alive(state)
        {
            return Some(json!({
                "job_id": state.get("job_id"),
                "operation": state.get("operation"),
                "phase": state.get("phase"),
                "status": "running",
            }));
        }
        pending.as_ref().map(|p| {
            json!({
                "job_id": p.job_id,
                "operation": p.operation,
                "phase": "starting",
                "status": "running",
            })
        })
    }

    /// Start one helper operation. Returns as soon as systemd has accepted
    /// the start; the job itself is followed by the watcher.
    pub async fn start(&self, operation: &str, job_id: &str, trigger: &str) -> Result<()> {
        if operation != "check" && !OPERATIONS.contains(&operation) {
            bail!("unknown operation: {}", operation.escape_debug());
        }
        if !valid_job_id(job_id) {
            bail!("job_id must be a lowercase UUID");
        }
        if !self.capable() {
            bail!("refused: this host has no LinuxGSM update helper configured");
        }
        let request = self.paths.run_dir.join("request.json");
        {
            // Check and claim under one lock: two starts cannot both pass.
            let state = self.read_state();
            let mut pending = self.pending.lock().unwrap();
            if let Some(job) = self.running_job_in(state.as_ref(), &pending) {
                bail!(
                    "refused: update job {} ({}) is already running",
                    str_field(&job, "job_id"),
                    str_field(&job, "operation")
                );
            }
            write_atomic(
                &request,
                &json!({ "job_id": job_id, "operation": operation, "trigger": trigger })
                    .to_string(),
                0o644,
            )
            .context("writing the update request")?;
            *pending = Some(Pending {
                job_id: job_id.to_string(),
                operation: operation.to_string(),
                since: Instant::now(),
            });
        }

        let started = self.systemctl_start(operation).await;
        if let Err(e) = started {
            let mut pending = self.pending.lock().unwrap();
            if pending.as_ref().is_some_and(|p| p.job_id == job_id) {
                *pending = None;
            }
            let _ = std::fs::remove_file(&request);
            return Err(e);
        }
        tracing::info!(operation, job_id, trigger, "update helper started");
        Ok(())
    }

    async fn systemctl_start(&self, operation: &str) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.paths.systemctl);
        command
            .args(systemctl_args(operation))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(SYSTEMCTL_TIMEOUT, command.output())
            .await
            .context("systemctl start timed out")?
            .with_context(|| format!("running {}", self.paths.systemctl.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr: String = stderr.trim().chars().take(300).collect();
            bail!(
                "could not start {}: {}",
                unit_name(operation),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
        }
        Ok(())
    }

    /// Give up on a unit we started that never published state: clears the
    /// claim and returns the failure to report. Also clears the claim (no
    /// failure) once the helper has picked the job up.
    pub fn settle_pending(&self, state: Option<&Value>, now: Instant) -> Option<Value> {
        let mut pending = self.pending.lock().unwrap();
        let p = pending.as_ref()?;
        if state.is_some_and(|s| str_field(s, "job_id") == p.job_id) {
            *pending = None;
            return None;
        }
        if now.duration_since(p.since) < PENDING_TIMEOUT {
            return None;
        }
        let failure = json!({
            "job_id": p.job_id,
            "operation": p.operation,
            "status": "failed",
            "phase": "starting",
            "failed_phase": "starting",
            "agent_synthesized": true,
            "error": format!(
                "the update helper did not start within {}s (another operation holds the lock, or the unit failed — see `journalctl -u '{}'`)",
                PENDING_TIMEOUT.as_secs(),
                unit_name(&p.operation)
            ),
        });
        *pending = None;
        // Ours, and nobody is coming for it.
        let _ = std::fs::remove_file(self.paths.run_dir.join("request.json"));
        Some(failure)
    }

    /// Validate and persist the portal's MatchZy overrides. Returns how many
    /// were written. The helper applies them on its next config reconcile.
    pub fn write_overrides(&self, overrides: &Value) -> Result<usize> {
        let validated = validate_overrides(overrides)?;
        write_atomic(&self.paths.overrides, &render_overrides(&validated), 0o640)
            .context("writing the MatchZy overrides file")?;
        Ok(validated.len())
    }

    /// `update_state`: the last check and the recorded installed versions.
    pub fn update_state_frame(&self) -> Value {
        json!({
            "type": "update_state",
            "check": read_json(&self.paths.state_dir.join("check.json")),
            "installed": read_json(&self.paths.state_dir.join("installed.json")),
        })
    }

    pub fn already_sent(&self, job_id: &str) -> bool {
        self.last_sent.lock().unwrap().as_deref() == Some(job_id)
    }

    /// An `update_result` reached the portal. Results the helper recorded
    /// (they are in `last-job.json`) move the sent marker; a check's failure
    /// or a failure the agent synthesized has no such record and must not,
    /// or the previous real job would look unsent again.
    pub fn delivered(&self, result: &Value) {
        if str_field(result, "operation") != "check"
            && result.get("agent_synthesized") != Some(&Value::Bool(true))
        {
            self.mark_sent(str_field(result, "job_id"));
        }
    }

    /// Keep a result whose send failed so the next connection delivers it.
    /// Only needed for results that exist nowhere on disk; the helper's own
    /// are replayed from `last-job.json` by `unsent_result`.
    pub fn stash_undelivered(&self, result: Value) {
        *self.undelivered.lock().unwrap() = Some(result);
    }

    pub fn take_undelivered(&self) -> Option<Value> {
        self.undelivered.lock().unwrap().take()
    }

    /// Record that the portal has this job's result. Persisted so a job that
    /// was delivered is not replayed after an agent restart.
    pub fn mark_sent(&self, job_id: &str) {
        *self.last_sent.lock().unwrap() = Some(job_id.to_string());
        if let Some(dir) = self.paths.sent_marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = write_atomic(&self.paths.sent_marker, &format!("{job_id}\n"), 0o644) {
            tracing::debug!(error = %e, "could not persist the sent marker");
        }
    }

    /// The last finished job, if the portal has not been sent it: a job
    /// that ended while the channel was down, or while the agent was.
    pub fn unsent_result(&self) -> Option<Value> {
        let last = read_json(&self.paths.state_dir.join("last-job.json"))?;
        let job_id = str_field(&last, "job_id");
        if job_id.is_empty()
            || !is_terminal(str_field(&last, "status"))
            || self.already_sent(job_id)
        {
            return None;
        }
        Some(last)
    }

    /// The helper's last word on the MatchZy config. The running server
    /// cannot be asked instead: MatchZy's settings are console commands, not
    /// cvars, and read back empty over RCON.
    pub fn config_health(&self) -> ConfigHealth {
        let path = self.paths.run_dir.join("config.json");
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            // A timestamp in the future (clock stepped back) is not stale.
            .is_none_or(|age| age <= CONFIG_MAX_AGE);
        let Some(report) = read_json(&path).filter(|_| fresh) else {
            return ConfigHealth::default();
        };
        ConfigHealth {
            demo_upload_wired: report.get("demo_upload_wired").and_then(Value::as_bool),
            overrides_applied: report.get("overrides_applied").and_then(Value::as_bool),
        }
    }

    /// Ask the helper to re-read the MatchZy config (`verify-config`), if it
    /// is due: on every connect (`force`) and every `VERIFY_INTERVAL`. Never
    /// while a job runs — the job refreshes `config.json` itself when it
    /// ends. Not a job: no request file, no claim, no result frame.
    pub async fn verify_config(&self, force: bool, now: Instant) -> bool {
        if !self.capable() || self.running_job().is_some() {
            return false;
        }
        {
            let mut last = self.last_verify.lock().unwrap();
            if !force && last.is_some_and(|at| now.duration_since(at) < VERIFY_INTERVAL) {
                return false;
            }
            *last = Some(now);
        }
        match self.systemctl_start("verify-config").await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "could not start the MatchZy config check");
                false
            }
        }
    }

    /// Should the agent run `repair-config` on its own right now? Only when
    /// the helper found a managed block definitely missing or overridden,
    /// only when idle, at most once per `AUTO_REPAIR_INTERVAL`. Claims the
    /// slot when it says yes.
    pub fn claim_auto_repair(&self, health: ConfigHealth, now: Instant) -> bool {
        if !health.needs_repair() || !self.capable() || self.running_job().is_some() {
            return false;
        }
        let mut last = self.last_auto_repair.lock().unwrap();
        if last.is_some_and(|at| now.duration_since(at) < AUTO_REPAIR_INTERVAL) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// Watch the state file for the life of the process and feed events to the
/// connection loop. Runs in its own task: the loop must never wait on the
/// helper. `reload` is called when a finished repair left the running
/// server still to be told (`needs_reload`).
pub fn spawn_watcher<F>(updater: Arc<Updater>, reload: F) -> mpsc::Receiver<Event>
where
    F: Fn() + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut tracker = Tracker::primed(updater.read_state().as_ref());
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let state = updater.read_state();
            let now = Instant::now();
            let mut events = Vec::new();
            if let Some(failure) = updater.settle_pending(state.as_ref(), now) {
                events.push(Event::Result(failure));
            }
            let alive = state.as_ref().is_some_and(|s| updater.helper_alive(s));
            events.extend(tracker.observe(state.as_ref(), alive, now));
            for event in events {
                match event {
                    // Progress is disposable: with the channel full (portal
                    // unreachable) the next one supersedes it anyway.
                    Event::Progress(_) => {
                        let _ = tx.try_send(event);
                    }
                    Event::Result(ref result) => {
                        if result.get("needs_reload") == Some(&Value::Bool(true)) {
                            reload();
                        }
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    Event::CheckDone => {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory tree standing in for the host; removed on drop.
    struct Host {
        root: PathBuf,
        paths: Paths,
    }

    impl Host {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "portal-agent-test-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            for dir in ["etc/agent", "run", "state", "agent-state", "proc", "sbin"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            let paths = Paths {
                conf: root.join("etc/gs-update.conf"),
                helper: root.join("sbin/portal-gs-update"),
                run_dir: root.join("run"),
                state_dir: root.join("state"),
                overrides: root.join("etc/agent/matchzy_overrides.cfg"),
                systemctl: "true".into(),
                sent_marker: root.join("agent-state/last-sent-job"),
                proc_dir: root.join("proc"),
            };
            Self { root, paths }
        }

        /// Install the helper and its config: the host is update-capable.
        fn capable(self) -> Self {
            std::fs::write(&self.paths.conf, "LGSM_USER=cs2server\n").unwrap();
            std::fs::write(&self.paths.helper, "#!/bin/sh\n").unwrap();
            self
        }

        fn updater(&self) -> Updater {
            Updater::new(self.paths.clone())
        }

        fn write_state(&self, state: &Value) {
            std::fs::write(self.paths.run_dir.join("state.json"), state.to_string()).unwrap();
        }

        /// Pretend pid `pid` is a live helper process.
        fn helper_process(&self, pid: u64) {
            let dir = self.paths.proc_dir.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("cmdline"),
                b"/bin/bash\0/usr/sbin/portal-gs-update\0mods\0",
            )
            .unwrap();
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const JOB: &str = "0b0e3c5e-7f0a-4c1d-9b7e-1a2b3c4d5e6f";
    const JOB2: &str = "11111111-2222-4333-8444-555555555555";

    fn state(job: &str, operation: &str, status: &str, phase: &str, log: &str) -> Value {
        json!({
            "job_id": job, "operation": operation, "trigger": "portal", "pid": 4242,
            "status": status, "phase": phase, "log_tail": log, "needs_reload": false,
        })
    }

    // --- overrides -----------------------------------------------------------

    #[test]
    fn overrides_accept_plain_matchzy_cvars() {
        let v = validate_overrides(&json!({
            "matchzy_minimum_ready_required": 5,
            "matchzy_kick_when_no_match_loaded": true,
            "matchzy_chat_prefix": "  [CG] ",
            "matchzy_knife_enabled_default": "",
        }))
        .unwrap();
        assert_eq!(
            v,
            vec![
                ("matchzy_chat_prefix".to_string(), "[CG]".to_string()),
                (
                    "matchzy_kick_when_no_match_loaded".to_string(),
                    "true".to_string()
                ),
                ("matchzy_knife_enabled_default".to_string(), String::new()),
                (
                    "matchzy_minimum_ready_required".to_string(),
                    "5".to_string()
                ),
            ]
        );
        assert_eq!(
            render_overrides(&v[2..]),
            "matchzy_knife_enabled_default \nmatchzy_minimum_ready_required 5\n"
        );
        assert_eq!(validate_overrides(&json!({})).unwrap(), vec![]);
    }

    #[test]
    fn overrides_refuse_portal_owned_cvars() {
        for name in [
            "matchzy_demo_upload_url",
            "matchzy_demo_upload_header_value",
            "matchzy_remote_log_url",
            "matchzy_remote_backup_header_value",
            "matchzy_loadmatch",
            "matchzy_loadmatch_url",
            "matchzy_loadbackup_url",
        ] {
            let err = validate_overrides(&json!({ name: "x" })).unwrap_err();
            assert!(err.to_string().contains("portal-owned"), "{name}: {err}");
        }
    }

    #[test]
    fn overrides_refuse_names_outside_the_pattern() {
        for name in [
            "rcon_password",
            "sv_cheats",
            "matchzy_",
            "MATCHZY_x",
            "matchzy_x y",
            "matchzy_x;quit",
            "matchzy_x\nrcon_password",
            "matchzy_é",
            " matchzy_x",
        ] {
            assert!(
                validate_overrides(&json!({ name: "1" })).is_err(),
                "{name:?}"
            );
        }
        let long = format!("matchzy_{}", "a".repeat(200));
        assert!(validate_overrides(&json!({ long: "1" })).is_err());
    }

    #[test]
    fn overrides_refuse_values_that_could_escape_the_line() {
        for value in ["a\"b", "a;b", "a\nb", "a\rb", "a\tb", "a\u{0}b", "\u{7f}"] {
            assert!(
                validate_overrides(&json!({ "matchzy_chat_prefix": value })).is_err(),
                "{value:?}"
            );
        }
        let ok = "x".repeat(256);
        assert!(validate_overrides(&json!({ "matchzy_chat_prefix": ok })).is_ok());
        let long = "x".repeat(257);
        assert!(validate_overrides(&json!({ "matchzy_chat_prefix": long })).is_err());
        for value in [json!(null), json!([1]), json!({"a": 1})] {
            assert!(validate_overrides(&json!({ "matchzy_chat_prefix": value })).is_err());
        }
    }

    #[test]
    fn overrides_are_bounded_and_all_or_nothing() {
        let many: serde_json::Map<String, Value> = (0..65)
            .map(|i| (format!("matchzy_cvar_{i}"), json!(i)))
            .collect();
        assert!(validate_overrides(&Value::Object(many.clone())).is_err());
        let ok: serde_json::Map<String, Value> = many.into_iter().take(64).collect();
        assert_eq!(validate_overrides(&Value::Object(ok)).unwrap().len(), 64);
        assert!(validate_overrides(&json!(["matchzy_x"])).is_err());
        assert!(validate_overrides(&json!("matchzy_x 1")).is_err());

        // One bad entry: nothing is written, the old file stays.
        let host = Host::new();
        let updater = host.updater();
        assert_eq!(
            updater
                .write_overrides(&json!({ "matchzy_minimum_ready_required": 5 }))
                .unwrap(),
            1
        );
        assert!(
            updater
                .write_overrides(&json!({
                    "matchzy_minimum_ready_required": 4,
                    "matchzy_demo_upload_url": "https://evil",
                }))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&host.paths.overrides).unwrap(),
            "matchzy_minimum_ready_required 5\n"
        );
        // Clearing the overrides is an empty file, not a missing one.
        assert_eq!(updater.write_overrides(&json!({})).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&host.paths.overrides).unwrap(), "");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(host.paths.overrides.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // --- capability ----------------------------------------------------------

    #[test]
    fn capability_needs_both_the_helper_and_its_config() {
        let host = Host::new();
        assert!(host.updater().capabilities().is_empty());
        std::fs::write(&host.paths.helper, "#!/bin/sh\n").unwrap();
        assert!(host.updater().capabilities().is_empty(), "helper alone");
        std::fs::remove_file(&host.paths.helper).unwrap();
        std::fs::write(&host.paths.conf, "LGSM_USER=x\n").unwrap();
        assert!(host.updater().capabilities().is_empty(), "config alone");
        std::fs::write(&host.paths.helper, "#!/bin/sh\n").unwrap();
        assert_eq!(host.updater().capabilities(), vec!["lgsm_update"]);
    }

    // --- state file → frames -------------------------------------------------

    fn progress_phases(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Progress(f) => Some(f["phase"].as_str().unwrap().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn progress_is_debounced_to_one_frame_a_second() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);

        // First sight of a job: immediately.
        let e = t.observe(
            Some(&state(JOB, "mods", "running", "snapshot", "a")),
            true,
            at(0),
        );
        assert_eq!(progress_phases(&e), ["snapshot"]);
        assert_eq!(
            e[0],
            Event::Progress(json!({
                "type": "update_progress", "job_id": JOB, "operation": "mods",
                "phase": "snapshot", "status": "running", "trigger": "portal", "log_tail": "a",
            }))
        );
        // Unchanged: nothing, however long we wait.
        assert!(
            t.observe(
                Some(&state(JOB, "mods", "running", "snapshot", "a")),
                true,
                at(5000)
            )
            .is_empty()
        );
        // Changes inside the window are held…
        let e = t.observe(
            Some(&state(JOB, "mods", "running", "stop", "ab")),
            true,
            at(5100),
        );
        assert_eq!(
            progress_phases(&e),
            ["stop"],
            "5s since the last frame: due"
        );
        assert!(
            t.observe(
                Some(&state(JOB, "mods", "running", "mods-update", "abc")),
                true,
                at(5400)
            )
            .is_empty()
        );
        assert!(
            t.observe(
                Some(&state(JOB, "mods", "running", "mods-update", "abcd")),
                true,
                at(5900)
            )
            .is_empty()
        );
        // …and the latest reading goes out once the second is up.
        let e = t.observe(
            Some(&state(JOB, "mods", "running", "mods-update", "abcd")),
            true,
            at(6100),
        );
        assert_eq!(progress_phases(&e), ["mods-update"]);
        assert_eq!(
            e[0],
            Event::Progress(progress_frame(&state(
                JOB,
                "mods",
                "running",
                "mods-update",
                "abcd"
            )))
        );
        // A log-only change counts as a change.
        let e = t.observe(
            Some(&state(JOB, "mods", "running", "mods-update", "abcde")),
            true,
            at(7200),
        );
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn terminal_result_is_emitted_exactly_once() {
        let mut t = Tracker::default();
        let now = Instant::now();
        t.observe(
            Some(&state(JOB, "mods", "running", "verify", "")),
            true,
            now,
        );
        let done = state(JOB, "mods", "rolled_back", "rollback", "tail");
        // The helper has exited by the time we read its last write.
        let e = t.observe(Some(&done), false, now);
        assert_eq!(e, vec![Event::Result(done.clone())]);
        for _ in 0..3 {
            assert!(
                t.observe(Some(&done), false, now + Duration::from_secs(60))
                    .is_empty()
            );
        }
        let frame = result_frame(&done);
        assert_eq!(frame["type"], "update_result");
        assert_eq!(frame["status"], "rolled_back");
        assert_eq!(frame["job_id"], JOB);

        // The next job is news again, progress first.
        let e = t.observe(
            Some(&state(JOB2, "game", "running", "game-update", "")),
            true,
            now,
        );
        assert_eq!(progress_phases(&e), ["game-update"]);
        let e = t.observe(
            Some(&state(JOB2, "game", "succeeded", "done", "")),
            false,
            now,
        );
        assert!(matches!(e.as_slice(), [Event::Result(_)]));
    }

    #[test]
    fn a_job_that_ended_before_the_agent_started_is_not_re_emitted() {
        let done = state(JOB, "mods", "succeeded", "done", "");
        let mut t = Tracker::primed(Some(&done));
        assert!(t.observe(Some(&done), false, Instant::now()).is_empty());
        // But one still running when the agent starts is picked up.
        let running = state(JOB2, "mods", "running", "start", "");
        let mut t = Tracker::primed(Some(&running));
        assert_eq!(t.observe(Some(&running), true, Instant::now()).len(), 1);
        assert!(
            Tracker::primed(None)
                .observe(None, false, Instant::now())
                .is_empty()
        );
    }

    #[test]
    fn a_check_reports_through_update_state_not_progress() {
        let mut t = Tracker::default();
        let now = Instant::now();
        assert!(
            t.observe(
                Some(&state(JOB, "check", "running", "check", "x")),
                true,
                now
            )
            .is_empty()
        );
        let e = t.observe(
            Some(&state(JOB, "check", "succeeded", "done", "x")),
            false,
            now,
        );
        assert_eq!(e, vec![Event::CheckDone]);
        assert!(
            t.observe(
                Some(&state(JOB, "check", "succeeded", "done", "x")),
                false,
                now
            )
            .is_empty()
        );
        // A check that failed has an error worth showing.
        let failed = state(JOB2, "check", "failed", "check", "x");
        assert_eq!(
            t.observe(Some(&failed), false, now),
            vec![Event::Result(failed.clone())]
        );
    }

    #[test]
    fn a_running_state_with_no_helper_becomes_a_failure_after_a_grace_period() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let running = state(JOB, "mods", "running", "mods-update", "tail");
        t.observe(Some(&running), true, t0);
        // Helper gone, but it may just have written its last state.
        assert!(
            t.observe(Some(&running), false, t0 + Duration::from_secs(1))
                .is_empty()
        );
        assert!(
            t.observe(Some(&running), false, t0 + Duration::from_secs(4))
                .is_empty()
        );
        let e = t.observe(Some(&running), false, t0 + Duration::from_secs(7));
        let [Event::Result(dead)] = e.as_slice() else {
            panic!("expected one result, got {e:?}");
        };
        assert_eq!(dead["status"], "failed");
        assert_eq!(dead["failed_phase"], "mods-update");
        assert_eq!(dead["agent_synthesized"], true);
        assert_eq!(dead["job_id"], JOB);
        assert!(
            dead["error"]
                .as_str()
                .unwrap()
                .contains("without recording a result")
        );
        assert!(
            t.observe(Some(&running), false, t0 + Duration::from_secs(60))
                .is_empty()
        );

        // The usual race — final state lands inside the grace — is a normal result.
        let mut t = Tracker::default();
        let running = state(JOB2, "mods", "running", "verify", "");
        t.observe(Some(&running), true, t0);
        assert!(
            t.observe(Some(&running), false, t0 + Duration::from_secs(1))
                .is_empty()
        );
        let done = state(JOB2, "mods", "succeeded", "done", "");
        assert_eq!(
            t.observe(Some(&done), false, t0 + Duration::from_secs(2)),
            vec![Event::Result(done.clone())]
        );
    }

    // --- delivery marker / reconnect replay ----------------------------------

    #[test]
    fn a_job_that_finished_while_disconnected_is_replayed_once() {
        let host = Host::new();
        let updater = host.updater();
        assert_eq!(updater.unsent_result(), None, "nothing ever ran");

        let done = state(JOB, "mods", "succeeded", "done", "tail");
        std::fs::write(host.paths.state_dir.join("last-job.json"), done.to_string()).unwrap();
        // Since then a check overwrote state.json; the replay must not care.
        host.write_state(&state(JOB2, "check", "succeeded", "done", ""));

        assert_eq!(
            updater.unsent_result(),
            Some(done.clone()),
            "first connect: replay"
        );
        assert_eq!(
            updater.unsent_result(),
            Some(done.clone()),
            "send failed: still owed"
        );
        updater.mark_sent(JOB);
        assert_eq!(updater.unsent_result(), None, "delivered");
        assert!(updater.already_sent(JOB));

        // A failure the agent made up (no record on disk) or a failed check
        // must not move the marker, or JOB would be replayed again.
        updater.delivered(&json!({"job_id": JOB2, "operation": "mods", "status": "failed", "agent_synthesized": true}));
        updater.delivered(&state(JOB2, "check", "failed", "check", ""));
        assert!(updater.already_sent(JOB));
        assert_eq!(updater.unsent_result(), None);
        // …and one that could not be sent waits for the next connection.
        assert_eq!(updater.take_undelivered(), None);
        updater.stash_undelivered(json!({"job_id": JOB2}));
        assert_eq!(updater.take_undelivered(), Some(json!({"job_id": JOB2})));
        assert_eq!(updater.take_undelivered(), None);

        // Survives an agent restart…
        let restarted = host.updater();
        assert!(restarted.already_sent(JOB));
        assert_eq!(restarted.unsent_result(), None);
        // …and the next job is owed again.
        let next = state(JOB2, "game", "failed", "verify", "");
        std::fs::write(host.paths.state_dir.join("last-job.json"), next.to_string()).unwrap();
        assert_eq!(restarted.unsent_result(), Some(next.clone()));
        restarted.delivered(&next);
        assert_eq!(restarted.unsent_result(), None);
    }

    #[test]
    fn replay_ignores_garbage_and_unfinished_jobs() {
        let host = Host::new();
        let updater = host.updater();
        let last = host.paths.state_dir.join("last-job.json");
        std::fs::write(&last, "not json").unwrap();
        assert_eq!(updater.unsent_result(), None);
        std::fs::write(
            &last,
            state(JOB, "mods", "running", "start", "").to_string(),
        )
        .unwrap();
        assert_eq!(updater.unsent_result(), None);
        std::fs::write(&last, json!({"status": "failed"}).to_string()).unwrap();
        assert_eq!(updater.unsent_result(), None);
    }

    #[test]
    fn update_state_carries_check_and_installed_or_nulls() {
        let host = Host::new();
        let updater = host.updater();
        assert_eq!(
            updater.update_state_frame(),
            json!({"type": "update_state", "check": null, "installed": null})
        );
        std::fs::write(
            host.paths.state_dir.join("check.json"),
            r#"{"checked_at":"t"}"#,
        )
        .unwrap();
        std::fs::write(
            host.paths.state_dir.join("installed.json"),
            r#"{"matchzy":"0.9.3"}"#,
        )
        .unwrap();
        assert_eq!(
            updater.update_state_frame(),
            json!({"type": "update_state", "check": {"checked_at": "t"}, "installed": {"matchzy": "0.9.3"}})
        );
    }

    // --- starting jobs -------------------------------------------------------

    #[test]
    fn systemctl_is_asked_to_start_exactly_the_template_instance() {
        assert_eq!(
            systemctl_args("repair-config"),
            [
                "--no-ask-password",
                "start",
                "--no-block",
                "portal-gs-update@repair-config.service"
            ]
        );
        assert!(valid_job_id(JOB));
        assert!(valid_job_id(&new_job_id()));
        assert_ne!(new_job_id(), new_job_id());
        assert_eq!(new_job_id().as_bytes()[14], b'4');
        for bad in [
            "",
            "x",
            "0B0E3C5E-7F0A-4C1D-9B7E-1A2B3C4D5E6F",
            "0b0e3c5e-7f0a-4c1d-9b7e-1a2b3c4d5e6f0",
            "$(reboot)aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(!valid_job_id(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn start_writes_the_request_and_claims_the_job() {
        let host = Host::new().capable();
        let updater = host.updater();
        assert_eq!(updater.running_job(), None);
        updater.start("mods", JOB, "portal").await.unwrap();

        let request: Value = read_json(&host.paths.run_dir.join("request.json")).unwrap();
        assert_eq!(
            request,
            json!({"job_id": JOB, "operation": "mods", "trigger": "portal"})
        );
        // Before the helper has written anything the job already counts.
        assert_eq!(
            updater.running_job(),
            Some(
                json!({"job_id": JOB, "operation": "mods", "phase": "starting", "status": "running"})
            )
        );
    }

    #[tokio::test]
    async fn start_refuses_while_a_job_is_running() {
        let host = Host::new().capable();
        let updater = host.updater();
        host.write_state(&state(JOB, "mods", "running", "mods-update", ""));
        host.helper_process(4242);

        let err = updater
            .start("game", JOB2, "portal")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("refused: update job"), "{err}");
        assert!(err.contains(JOB) && err.contains("(mods)"), "{err}");
        assert!(
            !host.paths.run_dir.join("request.json").exists(),
            "no request for a refused start"
        );
        assert_eq!(
            updater.running_job(),
            Some(
                json!({"job_id": JOB, "operation": "mods", "phase": "mods-update", "status": "running"})
            )
        );

        // …and while one we started has not been picked up yet.
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("check", JOB, "portal").await.unwrap();
        let err = updater
            .start("mods", JOB2, "portal")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already running") && err.contains("(check)"),
            "{err}"
        );
        let request: Value = read_json(&host.paths.run_dir.join("request.json")).unwrap();
        assert_eq!(request["job_id"], JOB, "the first request is untouched");
    }

    #[tokio::test]
    async fn start_is_allowed_again_once_the_job_is_over_or_its_helper_is_dead() {
        // Finished job on disk.
        let host = Host::new().capable();
        host.write_state(&state(JOB, "mods", "succeeded", "done", ""));
        host.updater().start("game", JOB2, "portal").await.unwrap();

        // `running` on disk, but no such process: a stale state must not
        // lock the portal out forever.
        let host = Host::new().capable();
        host.write_state(&state(JOB, "mods", "running", "mods-update", ""));
        let updater = host.updater();
        assert_eq!(updater.running_job(), None);
        updater.start("game", JOB2, "portal").await.unwrap();

        // A recycled pid that is some other program does not count either.
        let host = Host::new().capable();
        host.write_state(&state(JOB, "mods", "running", "mods-update", ""));
        std::fs::create_dir_all(host.paths.proc_dir.join("4242")).unwrap();
        std::fs::write(
            host.paths.proc_dir.join("4242/cmdline"),
            b"/usr/bin/vim\0notes\0",
        )
        .unwrap();
        assert_eq!(host.updater().running_job(), None);
    }

    #[tokio::test]
    async fn start_validates_before_touching_anything() {
        let host = Host::new().capable();
        let updater = host.updater();
        for (operation, job) in [
            ("mods; reboot", JOB),
            ("", JOB),
            ("MODS", JOB),
            ("mods", "not-a-uuid"),
            ("mods", "0b0e3c5e-7f0a-4c1d-9b7e-1a2b3c4d5e6f; reboot"),
        ] {
            assert!(
                updater.start(operation, job, "portal").await.is_err(),
                "{operation:?} {job:?}"
            );
        }
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert_eq!(updater.running_job(), None);

        // No helper on this host: refused, clearly.
        let bare = Host::new();
        let err = bare
            .updater()
            .start("mods", JOB, "portal")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no LinuxGSM update helper"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_systemctl_releases_the_claim() {
        let mut host = Host::new().capable();
        host.paths.systemctl = "false".into();
        let updater = host.updater();
        let err = updater
            .start("mods", JOB, "portal")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("could not start portal-gs-update@mods.service"),
            "{err}"
        );
        assert_eq!(updater.running_job(), None);
        assert!(!host.paths.run_dir.join("request.json").exists());

        host.paths.systemctl = host.root.join("no-such-systemctl");
        let updater = host.updater();
        assert!(updater.start("mods", JOB, "portal").await.is_err());
        assert_eq!(updater.running_job(), None);
    }

    #[tokio::test]
    async fn a_unit_that_never_writes_state_is_reported_failed() {
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("mods", JOB, "portal").await.unwrap();
        let now = Instant::now();

        assert_eq!(
            updater.settle_pending(None, now + Duration::from_secs(30)),
            None
        );
        assert!(updater.running_job().is_some());
        // Someone else's state does not settle it.
        let other = state(JOB2, "game", "succeeded", "done", "");
        assert_eq!(
            updater.settle_pending(Some(&other), now + Duration::from_secs(30)),
            None
        );

        let failure = updater
            .settle_pending(Some(&other), now + Duration::from_secs(61))
            .unwrap();
        assert_eq!(failure["job_id"], JOB);
        assert_eq!(failure["operation"], "mods");
        assert_eq!(failure["status"], "failed");
        assert!(failure["error"].as_str().unwrap().contains("did not start"));
        assert_eq!(updater.running_job(), None);
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert_eq!(
            updater.settle_pending(None, now + Duration::from_secs(120)),
            None,
            "reported once"
        );

        // The normal path: the helper picks it up, the claim just goes away.
        updater.start("mods", JOB2, "portal").await.unwrap();
        let picked_up = state(JOB2, "mods", "running", "init", "");
        assert_eq!(
            updater.settle_pending(Some(&picked_up), now + Duration::from_secs(300)),
            None
        );
        assert_eq!(
            updater.running_job(),
            None,
            "no helper process in this test"
        );
    }

    // --- auto-repair ---------------------------------------------------------

    #[tokio::test]
    async fn auto_repair_is_claimed_once_per_ten_minutes_and_only_when_definitely_broken() {
        let health = |wired, applied| ConfigHealth {
            demo_upload_wired: wired,
            overrides_applied: applied,
        };
        let unwired = health(Some(false), None);
        let host = Host::new().capable();
        let updater = host.updater();
        let t0 = Instant::now();
        assert!(!updater.claim_auto_repair(health(Some(true), Some(true)), t0));
        assert!(
            !updater.claim_auto_repair(health(None, None), t0),
            "unknown is not broken"
        );
        assert!(!updater.claim_auto_repair(health(Some(true), None), t0));
        assert!(updater.claim_auto_repair(unwired, t0));
        assert!(!updater.claim_auto_repair(unwired, t0 + Duration::from_secs(30)));
        assert!(!updater.claim_auto_repair(unwired, t0 + Duration::from_secs(599)));
        assert!(updater.claim_auto_repair(unwired, t0 + Duration::from_secs(600)));
        // Overrides not applied is a reason too, under the same limit.
        let stale_overrides = health(Some(true), Some(false));
        assert!(!updater.claim_auto_repair(stale_overrides, t0 + Duration::from_secs(700)));
        assert!(updater.claim_auto_repair(stale_overrides, t0 + Duration::from_secs(1200)));

        // Never on top of a running job, and that does not use up the slot.
        updater.start("mods", JOB, "portal").await.unwrap();
        assert!(!updater.claim_auto_repair(unwired, t0 + Duration::from_secs(3000)));
        updater.settle_pending(None, Instant::now() + Duration::from_secs(61));
        assert!(updater.claim_auto_repair(unwired, t0 + Duration::from_secs(3000)));

        // Not on a host without the helper.
        assert!(!Host::new().updater().claim_auto_repair(unwired, t0));
    }

    // --- config health -------------------------------------------------------

    fn write_config(host: &Host, report: &Value) {
        std::fs::write(host.paths.run_dir.join("config.json"), report.to_string()).unwrap();
    }

    #[test]
    fn config_health_comes_from_the_helpers_report() {
        let host = Host::new();
        let updater = host.updater();
        assert_eq!(
            updater.config_health(),
            ConfigHealth::default(),
            "no report yet"
        );
        assert!(!updater.config_health().needs_repair());

        write_config(
            &host,
            &json!({
                "checked_at": "2026-11-21T04:00:00Z", "demo_upload_wired": true,
                "overrides_applied": null, "reason": null,
            }),
        );
        let health = updater.config_health();
        assert_eq!(health.demo_upload_wired, Some(true));
        assert_eq!(health.overrides_applied, None);
        assert!(!health.needs_repair());

        write_config(
            &host,
            &json!({"demo_upload_wired": false, "overrides_applied": true, "reason": "x"}),
        );
        assert_eq!(updater.config_health().demo_upload_wired, Some(false));
        assert!(updater.config_health().needs_repair());
        write_config(
            &host,
            &json!({"demo_upload_wired": null, "overrides_applied": false}),
        );
        assert_eq!(updater.config_health().demo_upload_wired, None);
        assert!(updater.config_health().needs_repair());

        // Garbage, or the wrong types, is "unknown" — never "broken".
        std::fs::write(host.paths.run_dir.join("config.json"), "{ not json").unwrap();
        assert_eq!(updater.config_health(), ConfigHealth::default());
        write_config(
            &host,
            &json!({"demo_upload_wired": "false", "overrides_applied": 0}),
        );
        assert_eq!(updater.config_health(), ConfigHealth::default());
    }

    #[test]
    fn a_stale_config_report_is_unknown() {
        let host = Host::new();
        let updater = host.updater();
        write_config(
            &host,
            &json!({"demo_upload_wired": false, "overrides_applied": false}),
        );
        let path = host.paths.run_dir.join("config.json");
        let set_age = |secs: u64| {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
        };
        set_age(14 * 60);
        assert_eq!(updater.config_health().demo_upload_wired, Some(false));
        // Older than 15 minutes: the helper has not been heard from; do not
        // report — or repair on the strength of — what it said back then.
        set_age(16 * 60);
        assert_eq!(updater.config_health(), ConfigHealth::default());
        assert!(!updater.config_health().needs_repair());
    }

    #[tokio::test]
    async fn verify_config_runs_on_connect_and_every_five_minutes_when_idle() {
        let host = Host::new().capable();
        let updater = host.updater();
        let t0 = Instant::now();
        assert!(updater.verify_config(false, t0).await, "first time");
        assert!(
            !updater
                .verify_config(false, t0 + Duration::from_secs(299))
                .await
        );
        assert!(
            updater
                .verify_config(true, t0 + Duration::from_secs(10))
                .await,
            "connect forces"
        );
        assert!(
            !updater
                .verify_config(false, t0 + Duration::from_secs(300))
                .await,
            "counted from the last run"
        );
        assert!(
            updater
                .verify_config(false, t0 + Duration::from_secs(310))
                .await
        );
        // It is not a job: nothing is claimed, no request is written.
        assert_eq!(updater.running_job(), None);
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert_eq!(
            systemctl_args("verify-config").last().unwrap(),
            "portal-gs-update@verify-config.service"
        );

        // Never while a job runs, not even on connect; and the turn is kept.
        updater.start("mods", JOB, "portal").await.unwrap();
        assert!(
            !updater
                .verify_config(true, t0 + Duration::from_secs(1000))
                .await
        );
        updater.settle_pending(None, Instant::now() + Duration::from_secs(61));
        assert!(
            updater
                .verify_config(false, t0 + Duration::from_secs(1000))
                .await
        );

        // Not on a host without the helper; a failing systemctl is not fatal.
        assert!(!Host::new().updater().verify_config(true, t0).await);
        let mut broken = Host::new().capable();
        broken.paths.systemctl = "false".into();
        assert!(!broken.updater().verify_config(true, t0).await);
    }
}
