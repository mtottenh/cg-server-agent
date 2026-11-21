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
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// How long a started unit gets before the agent asks systemd whether it is
/// still there. After that it is failed only once systemd says the unit is
/// no longer active — a job waiting for the lock is not a failed job.
const PENDING_GRACE: Duration = Duration::from_secs(10);
/// A `running` state whose helper is gone for this long is dead (SIGKILL,
/// OOM). The grace covers the helper's own final write + exit.
const DEAD_HELPER_GRACE: Duration = Duration::from_secs(5);
/// How often systemd is asked about a unit, and how long its answer is
/// trusted.
const UNIT_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const UNIT_PROBE_MAX_AGE: Duration = Duration::from_secs(10);
/// Auto-repair of the managed config blocks: at most once per this long,
/// doubling after every repair that did not leave the config healthy.
const AUTO_REPAIR_INTERVAL: Duration = Duration::from_secs(600);
const AUTO_REPAIR_MAX_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// How often the agent asks the helper to re-check the MatchZy config.
const VERIFY_INTERVAL: Duration = Duration::from_secs(300);
/// A `config.json` older than this says nothing about the config now.
const CONFIG_MAX_AGE: Duration = Duration::from_secs(900);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);
/// A pending reload is retried this often…
const RELOAD_RETRY: Duration = Duration::from_secs(10);
/// …unless MatchZy has not answered `get5_status` for this long (the plugin
/// stack is not loaded): then only this often.
const RELOAD_UNLOADED_AFTER: Duration = Duration::from_secs(180);
const RELOAD_UNLOADED_RETRY: Duration = Duration::from_secs(300);
/// Job ids the portal has acknowledged, newest last.
const ACKED_KEPT: usize = 8;

const MAX_OVERRIDES: usize = 64;
const MAX_OVERRIDE_VALUE_BYTES: usize = 256;
const MAX_OVERRIDE_NAME_BYTES: usize = 128;

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
    /// The agent's own: `acked-jobs`, `unacked-result.json`, `pending-job.json`,
    /// `reload-pending`, `auto-repair.json`.
    pub agent_state_dir: PathBuf,
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
            agent_state_dir: "/var/lib/portal-server-agent".into(),
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
    /// Consecutive definite "not active" answers from systemd.
    inactive_seen: u32,
}

/// What `systemctl is-active` said about a unit instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// active, activating, deactivating or reloading.
    Active,
    /// A definite `inactive` or `failed`.
    Inactive,
    /// No usable answer: systemctl failed, timed out, or was not asked.
    Unknown,
}

/// What to do about a pending MatchZy config reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadStep {
    /// No match is loaded: run `exec MatchZy/config.cfg` now.
    Exec,
    /// Not now (a match is loaded, or MatchZy did not answer): ask again.
    Wait,
    /// Nothing left to do: MatchZy has just been loaded afresh and read the
    /// file itself.
    Done,
}

/// What the helper last found in the MatchZy config file
/// (`config.json`, written by `verify-config` and at the end of every job).
/// The two findings are `None` when unknown: no report, a stale one, or
/// nothing to compare against (no enrollment snippet / no overrides file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigHealth {
    /// The demo-upload block is in place and nothing after it overrides it.
    pub demo_upload_wired: Option<bool>,
    /// The portal's MatchZy overrides are in the config.
    pub overrides_applied: Option<bool>,
    /// `AUTO_REPAIR` in gs-update.conf: may the agent repair on its own.
    pub auto_repair: bool,
}

impl Default for ConfigHealth {
    fn default() -> Self {
        Self {
            demo_upload_wired: None,
            overrides_applied: None,
            auto_repair: true,
        }
    }
}

impl ConfigHealth {
    /// Something is definitely wrong that `repair-config` is there to fix.
    pub fn needs_repair(&self) -> bool {
        self.demo_upload_wired == Some(false) || self.overrides_applied == Some(false)
    }

    /// Known to be fine: at least one finding, none of them bad.
    fn healthy(&self) -> bool {
        !self.needs_repair()
            && (self.demo_upload_wired.is_some() || self.overrides_applied.is_some())
    }
}

#[derive(Default)]
struct RepairState {
    last: Option<Instant>,
    /// Auto-repairs since the config was last seen healthy.
    streak: u32,
}

pub struct Updater {
    paths: Paths,
    pending: Mutex<Option<Pending>>,
    repair: Mutex<RepairState>,
    last_verify: Mutex<Option<Instant>>,
    /// Job ids the portal acknowledged (`update_ack`), persisted.
    acked: Mutex<Vec<String>>,
    /// The latest result that exists nowhere on the helper's disk (a
    /// failure the agent synthesized, a failed check) and is not yet
    /// acknowledged. Persisted, so it is reported once, not once per agent
    /// restart.
    extra: Mutex<Option<Value>>,
    /// The running server still has to re-read the MatchZy config.
    reload_pending: AtomicBool,
    /// (job id, unit active, when asked): systemd's word on the helper.
    unit_probe: Mutex<Option<(String, bool, Instant)>>,
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

fn is_synthesized(result: &Value) -> bool {
    result.get("agent_synthesized") == Some(&Value::Bool(true))
}

/// Is this result in the helper's `last-job.json`? Those are replayed from
/// there; everything else (a failed check, a failure the agent made up)
/// only exists in the agent.
fn recorded_by_helper(result: &Value) -> bool {
    str_field(result, "operation") != "check" && !is_synthesized(result)
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

/// May the MatchZy config be re-executed on the running server now? Only
/// when MatchZy says no match is loaded: `exec MatchZy/config.cfg` resets
/// settings a live match depends on. Anything else — a match in any state,
/// no answer, an answer we cannot read — means "not now, ask again".
pub fn reload_allowed(get5_status: Option<&Value>) -> bool {
    get5_status
        .and_then(|s| s.get("gamestate"))
        .and_then(Value::as_str)
        == Some("none")
}

/// Decide a pending reload from MatchZy's answer to `get5_status` (`None`:
/// RCON worked but MatchZy did not answer — the plugin stack is not loaded)
/// and for how long it had not been answering before this.
pub fn reload_step(get5_status: Option<&Value>, unanswered_for: Option<Duration>) -> ReloadStep {
    match get5_status {
        None => ReloadStep::Wait,
        // It was gone for minutes and is back: a plugin stack that has just
        // been loaded executed MatchZy/config.cfg on its way up. Doing it
        // again — possibly into a match that is already being set up — would
        // only be a risk.
        Some(_) if unanswered_for.is_some_and(|d| d >= RELOAD_UNLOADED_AFTER) => ReloadStep::Done,
        status if reload_allowed(status) => ReloadStep::Exec,
        Some(_) => ReloadStep::Wait,
    }
}

/// How long to wait before the next attempt at a pending reload. Every 10
/// seconds while there is a server to talk to; every 5 minutes once MatchZy
/// has been silent for 3 — there is no point asking a server without its
/// plugin stack six times a minute for as long as it stays that way.
pub fn reload_retry_delay(unanswered_for: Option<Duration>) -> Duration {
    if unanswered_for.is_some_and(|d| d >= RELOAD_UNLOADED_AFTER) {
        RELOAD_UNLOADED_RETRY
    } else {
        RELOAD_RETRY
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `Instant` that corresponds to a wall-clock time recorded by an
/// earlier agent process (`now` if that is not representable).
fn instant_at(unix_secs: u64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_secs(unix_now().saturating_sub(unix_secs)))
        .unwrap_or(now)
}

/// The wall-clock time of an `Instant` in the recent past.
fn unix_at(at: Instant) -> u64 {
    unix_now().saturating_sub(Instant::now().saturating_duration_since(at).as_secs())
}

// --- MatchZy overrides -------------------------------------------------------

/// Why an override cvar name is not acceptable, if it is not.
fn override_name_refusal(name: &str) -> Option<&'static str> {
    let rest = match name.strip_prefix("matchzy_") {
        Some(r) if !r.is_empty() => r,
        _ => return Some("name must match ^matchzy_[a-z0-9_]+$"),
    };
    if name.len() > MAX_OVERRIDE_NAME_BYTES
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Some("name must match ^matchzy_[a-z0-9_]+$ (at most 128 characters)");
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
///
/// Values are printable ASCII (0x20–0x7E) without `"`, `;` or `\`. Not
/// "no control characters": U+2028, U+0085, NBSP and friends are not
/// `char::is_control`, and what a console or a config parser makes of them
/// is not something to find out in production. The helper applies the same
/// rules, byte for byte, to the file (`render_overrides` in
/// portal-gs-update); keep the two in step.
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
            let shown: String = name
                .chars()
                .take(64)
                .filter(|c| c.is_ascii_graphic())
                .collect();
            bail!("override {shown:?} refused: {reason}");
        }
        let value = match value {
            Value::String(s) => s.trim_matches(' ').to_string(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => bail!("override {name}: value must be a string, number or boolean"),
        };
        if value.len() > MAX_OVERRIDE_VALUE_BYTES {
            bail!("override {name}: value longer than {MAX_OVERRIDE_VALUE_BYTES} characters");
        }
        if !value
            .bytes()
            .all(|b| (0x20..=0x7e).contains(&b) && !matches!(b, b'"' | b';' | b'\\'))
        {
            bail!(
                "override {name}: value must be printable ASCII without quotes, semicolons or backslashes"
            );
        }
        out.push((name.clone(), value));
    }
    out.sort();
    Ok(out)
}

/// The overrides file: one `cvar value` per line, one space between them.
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

/// Which results have gone out on the current connection. A job that ended
/// while the channel was down is replayed on connect *and* still sits in
/// the watcher's queue; the portal should get it once per connection.
#[derive(Default)]
pub struct SentThisConnection(HashSet<String>);

impl SentThisConnection {
    /// `true` the first time a result is offered, `false` after that. A
    /// synthesized failure and the real result that later replaces it are
    /// two different results.
    pub fn first_time(&mut self, result: &Value) -> bool {
        self.0.insert(format!(
            "{}:{}",
            str_field(result, "job_id"),
            is_synthesized(result)
        ))
    }
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
    /// Job seen `running` with no helper, and since when.
    dead_since: Option<(String, Instant)>,
    /// Job whose failure was made up here. Should the helper turn out to
    /// have been alive after all, its real result still goes out.
    synthesized: Option<String>,
}

impl Tracker {
    /// Start from what is already on disk. A job that finished before this
    /// process started is not news to the tracker (whether the *portal* has
    /// it is `Updater::unacked_results`' business). Neither is a stale
    /// `running` state whose failure an earlier agent process already
    /// reported (`already_reported`): it must not be reported again after
    /// every restart.
    pub fn primed(state: Option<&Value>, already_reported: bool) -> Self {
        let mut tracker = Self::default();
        if let Some(state) = state {
            let job = str_field(state, "job_id").to_string();
            let status = str_field(state, "status");
            if is_terminal(status) {
                tracker.done = Some(job);
            } else if status == "running" && already_reported {
                tracker.done = Some(job.clone());
                tracker.synthesized = Some(job);
            }
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
        if job_id.is_empty() {
            return Vec::new();
        }
        if self.done.as_deref() == Some(job_id) {
            // Declared dead, yet it finished: the truth supersedes our guess.
            if is_terminal(status) && self.synthesized.as_deref() == Some(job_id) {
                self.synthesized = None;
                // …in the form it would have had: a check that succeeded
                // reports through `update_state`, never as a result.
                return if is_check && status == "succeeded" {
                    vec![Event::CheckDone]
                } else {
                    vec![Event::Result(state.clone())]
                };
            }
            return Vec::new();
        }

        if is_terminal(status) {
            self.done = Some(job_id.to_string());
            self.dead_since = None;
            self.synthesized = None;
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
                        self.synthesized = Some(job_id.to_string());
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
        let acked = std::fs::read_to_string(paths.agent_state_dir.join("acked-jobs"))
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|l| valid_job_id(l))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let extra = read_json(&paths.agent_state_dir.join("unacked-result.json"));
        // What an earlier agent process (before a restart, a deb upgrade)
        // still owed: a job it had started, a reload, its repair backoff.
        let pending = read_json(&paths.agent_state_dir.join("pending-job.json")).and_then(|p| {
            let job_id = str_field(&p, "job_id");
            let operation = str_field(&p, "operation");
            (valid_job_id(job_id) && (operation == "check" || OPERATIONS.contains(&operation)))
                .then(|| Pending {
                    job_id: job_id.to_string(),
                    operation: operation.to_string(),
                    since: instant_at(p.get("started_at").and_then(Value::as_u64).unwrap_or(0)),
                    inactive_seen: 0,
                })
        });
        let repair = read_json(&paths.agent_state_dir.join("auto-repair.json"))
            .map(|r| RepairState {
                last: r.get("last_at").and_then(Value::as_u64).map(instant_at),
                streak: r
                    .get("streak")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
            })
            .unwrap_or_default();
        let reload_pending = paths.agent_state_dir.join("reload-pending").exists();
        Self {
            paths,
            pending: Mutex::new(pending),
            repair: Mutex::new(repair),
            last_verify: Mutex::new(None),
            acked: Mutex::new(acked),
            extra: Mutex::new(extra),
            reload_pending: AtomicBool::new(reload_pending),
            unit_probe: Mutex::new(None),
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

    // --- is the helper alive -------------------------------------------------

    /// Ask systemd about an operation's unit instance. Needs no privilege.
    /// Only a definite answer counts either way: a systemctl that fails or
    /// times out (D-Bus hiccup, systemd reloading) says nothing about the
    /// unit.
    pub async fn unit_state(&self, operation: &str) -> UnitState {
        let mut command = tokio::process::Command::new(&self.paths.systemctl);
        command
            .args(["is-active", &unit_name(operation)])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(SYSTEMCTL_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => match String::from_utf8_lossy(&output.stdout).trim() {
                "active" | "activating" | "deactivating" | "reloading" => UnitState::Active,
                "inactive" | "failed" => UnitState::Inactive,
                _ => UnitState::Unknown,
            },
            _ => UnitState::Unknown,
        }
    }

    /// Keep systemd's answer about the job in `state` fresh. Called by the
    /// watcher; everything else reads the cached answer.
    pub async fn refresh_liveness(&self, state: &Value) {
        if str_field(state, "status") != "running" {
            return;
        }
        let job = str_field(state, "job_id");
        {
            let probe = self.unit_probe.lock().unwrap();
            if let Some((id, _, at)) = probe.as_ref()
                && id == job
                && at.elapsed() < UNIT_PROBE_INTERVAL
            {
                return;
            }
        }
        let active = self.unit_state(str_field(state, "operation")).await == UnitState::Active;
        *self.unit_probe.lock().unwrap() = Some((job.to_string(), active, Instant::now()));
    }

    /// Is the helper that wrote this state still running? First systemd's
    /// word on the unit (works with `hidepid`, cannot be fooled by a
    /// recycled pid); then, for a job run from cron or by hand — which is
    /// not the unit — its pid in `/proc`, checked against the command line.
    pub fn helper_alive(&self, state: &Value) -> bool {
        {
            let probe = self.unit_probe.lock().unwrap();
            if let Some((id, active, at)) = probe.as_ref()
                && id == str_field(state, "job_id")
                && *active
                && at.elapsed() < UNIT_PROBE_MAX_AGE
            {
                return true;
            }
        }
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

    // --- starting jobs -------------------------------------------------------

    /// Set or clear the claim on a started unit, on disk too: an agent that
    /// is restarted seconds after starting a job (a deb upgrade does that)
    /// must still tell the portal how that job ended.
    fn store_pending(&self, slot: &mut Option<Pending>, value: Option<Pending>) {
        match &value {
            Some(p) => self.persist(
                "pending-job.json",
                &json!({
                    "job_id": p.job_id, "operation": p.operation, "started_at": unix_at(p.since),
                })
                .to_string(),
            ),
            None => {
                let _ = std::fs::remove_file(self.paths.agent_state_dir.join("pending-job.json"));
            }
        }
        *slot = value;
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
                // `created_at`: the helper only honours a request from the
                // last minute, so one orphaned by a failed start is never
                // picked up by a later run.
                &json!({
                    "job_id": job_id, "operation": operation, "trigger": trigger,
                    "created_at": unix_now(),
                })
                .to_string(),
                0o644,
            )
            .context("writing the update request")?;
            let claim = Pending {
                job_id: job_id.to_string(),
                operation: operation.to_string(),
                since: Instant::now(),
                inactive_seen: 0,
            };
            self.store_pending(&mut pending, Some(claim));
        }

        let started = self.systemctl_start(operation).await;
        if let Err(e) = started {
            let mut pending = self.pending.lock().unwrap();
            if pending.as_ref().is_some_and(|p| p.job_id == job_id) {
                self.store_pending(&mut pending, None);
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

    /// The operation of a unit we started that has had its grace period and
    /// still shows no state: time to ask systemd about it.
    pub fn pending_overdue(&self, now: Instant) -> Option<String> {
        let pending = self.pending.lock().unwrap();
        pending
            .as_ref()
            .filter(|p| now.duration_since(p.since) >= PENDING_GRACE)
            .map(|p| p.operation.clone())
    }

    /// Resolve a unit we started. `unit` is systemd's answer for it
    /// (`Unknown` when not asked, or when systemctl gave no usable answer).
    /// Returns a result to report, if there is one:
    ///
    /// * the helper published state for the job → nothing, the watcher has it;
    /// * the helper finished but could not publish (run directory full): its
    ///   own record from `last-job.json`;
    /// * the grace period is over and systemd has said twice in a row,
    ///   definitely, that the unit is not active, and it left no trace (it
    ///   never got the lock, the request was refused, the unit failed): a
    ///   synthesized failure;
    /// * otherwise — still within the grace period, the unit still active
    ///   (e.g. waiting for the lock), or no clear answer — keep waiting.
    pub fn settle_pending(
        &self,
        state: Option<&Value>,
        now: Instant,
        unit: UnitState,
    ) -> Option<Value> {
        let mut pending = self.pending.lock().unwrap();
        let p = pending.as_mut()?;
        if state.is_some_and(|s| str_field(s, "job_id") == p.job_id) {
            self.store_pending(&mut pending, None);
            return None;
        }
        if let Some(last) = read_json(&self.paths.state_dir.join("last-job.json"))
            && str_field(&last, "job_id") == p.job_id
            && is_terminal(str_field(&last, "status"))
        {
            self.store_pending(&mut pending, None);
            return Some(last);
        }
        match unit {
            UnitState::Active => p.inactive_seen = 0,
            UnitState::Inactive => p.inactive_seen += 1,
            UnitState::Unknown => {}
        }
        if now.duration_since(p.since) < PENDING_GRACE || p.inactive_seen < 2 {
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
                "the update helper ended without reporting anything (it could not get the lock, or the unit failed — see `journalctl -u '{}'`)",
                unit_name(&p.operation)
            ),
        });
        self.store_pending(&mut pending, None);
        // Ours, and nobody is coming for it.
        let _ = std::fs::remove_file(self.paths.run_dir.join("request.json"));
        Some(failure)
    }

    // --- overrides and reports -----------------------------------------------

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

    // --- delivery: at least once, until acknowledged -------------------------

    pub fn is_acked(&self, job_id: &str) -> bool {
        self.acked.lock().unwrap().iter().any(|j| j == job_id)
    }

    fn persist(&self, name: &str, contents: &str) {
        let _ = std::fs::create_dir_all(&self.paths.agent_state_dir);
        if let Err(e) = write_atomic(&self.paths.agent_state_dir.join(name), contents, 0o644) {
            tracing::debug!(error = %e, file = name, "could not persist agent state");
        }
    }

    /// The portal has this job's result (`update_ack`). Sending a frame is
    /// not the same as the portal having stored it, so only this stops the
    /// replay on reconnect. Persisted, so it also holds across restarts.
    pub fn ack(&self, job_id: &str) {
        {
            let mut acked = self.acked.lock().unwrap();
            acked.retain(|j| j != job_id);
            acked.push(job_id.to_string());
            let excess = acked.len().saturating_sub(ACKED_KEPT);
            acked.drain(..excess);
            self.persist("acked-jobs", &(acked.join("\n") + "\n"));
        }
        let mut extra = self.extra.lock().unwrap();
        if extra
            .as_ref()
            .is_some_and(|r| str_field(r, "job_id") == job_id)
        {
            *extra = None;
            let _ = std::fs::remove_file(self.paths.agent_state_dir.join("unacked-result.json"));
        }
    }

    /// Remember a result that exists nowhere on the helper's disk until the
    /// portal acknowledges it. One slot: the most recent.
    pub fn keep_extra(&self, result: &Value) {
        *self.extra.lock().unwrap() = Some(result.clone());
        self.persist("unacked-result.json", &result.to_string());
    }

    /// The helper's real result arrived for a job whose failure we had made
    /// up: the guess is no longer owed to anyone.
    fn drop_extra_for(&self, job_id: &str) {
        let mut extra = self.extra.lock().unwrap();
        if extra
            .as_ref()
            .is_some_and(|r| str_field(r, "job_id") == job_id)
        {
            *extra = None;
            let _ = std::fs::remove_file(self.paths.agent_state_dir.join("unacked-result.json"));
        }
    }

    /// Has a failure for this job already been synthesized (and kept, or
    /// even acknowledged) by an earlier agent process?
    fn already_reported(&self, job_id: &str) -> bool {
        self.is_acked(job_id)
            || self
                .extra
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| str_field(r, "job_id") == job_id)
    }

    /// Results the portal has not acknowledged, to (re)send on connect: the
    /// last finished job from the helper's record, and the latest result
    /// that only the agent has. Bounded to those two — never a backlog.
    pub fn unacked_results(&self) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(last) = read_json(&self.paths.state_dir.join("last-job.json")) {
            let job_id = str_field(&last, "job_id");
            if !job_id.is_empty()
                && is_terminal(str_field(&last, "status"))
                && !self.is_acked(job_id)
            {
                out.push(last);
            }
        }
        if let Some(extra) = self.extra.lock().unwrap().clone() {
            let job_id = str_field(&extra, "job_id");
            if !self.is_acked(job_id) && out.iter().all(|r| str_field(r, "job_id") != job_id) {
                out.push(extra);
            }
        }
        out
    }

    // --- config health, verify, auto-repair, reload --------------------------

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
            // Only an explicit `false` switches it off.
            auto_repair: report.get("auto_repair") != Some(&Value::Bool(false)),
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
    /// the operator has not switched it off (`AUTO_REPAIR=false`), and
    /// nothing is running. Rate-limited: 10 minutes after the first repair,
    /// doubling (to 6 hours at most) after each one that did not leave the
    /// config healthy — a repair that cannot work (a rejected overrides
    /// file, a config the game user cannot write) must not rewrite files and
    /// poke the server every 10 minutes forever. A healthy reading resets
    /// it. Claims the slot when it says yes.
    pub fn claim_auto_repair(&self, health: ConfigHealth, now: Instant) -> bool {
        let mut repair = self.repair.lock().unwrap();
        if health.healthy() && repair.streak != 0 {
            repair.streak = 0;
            self.persist_repair(&repair);
        }
        if !health.needs_repair()
            || !health.auto_repair
            || !self.capable()
            || self.running_job().is_some()
        {
            return false;
        }
        let interval = AUTO_REPAIR_INTERVAL
            .saturating_mul(1u32 << repair.streak.saturating_sub(1).min(16))
            .min(AUTO_REPAIR_MAX_INTERVAL);
        if repair
            .last
            .is_some_and(|at| now.duration_since(at) < interval)
        {
            return false;
        }
        repair.last = Some(now);
        repair.streak = repair.streak.saturating_add(1);
        self.persist_repair(&repair);
        true
    }

    /// The backoff survives an agent restart: a repair that cannot work must
    /// not get a fresh start every time the agent does.
    fn persist_repair(&self, repair: &RepairState) {
        self.persist(
            "auto-repair.json",
            &json!({ "streak": repair.streak, "last_at": repair.last.map(unix_at) }).to_string(),
        );
    }

    /// The running server has to re-read the MatchZy config (a
    /// `repair-config` changed the file under it). Stays set until
    /// `reload_done`: across RCON outages, for as long as a match is loaded,
    /// and across agent restarts (it is on disk) — otherwise an upgrade of
    /// the agent right after a repair would leave the server on its old
    /// settings with the heartbeat saying all is well.
    pub fn request_reload(&self) {
        self.reload_pending.store(true, Ordering::SeqCst);
        self.persist("reload-pending", "");
    }

    pub fn reload_pending(&self) -> bool {
        self.reload_pending.load(Ordering::SeqCst)
    }

    pub fn reload_done(&self) {
        self.reload_pending.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_file(self.paths.agent_state_dir.join("reload-pending"));
    }

    /// Everything that follows from a result leaving the watcher, before it
    /// is queued for the portal.
    fn note_result(&self, result: &Value) {
        if result.get("needs_reload") == Some(&Value::Bool(true))
            && str_field(result, "status") == "succeeded"
        {
            self.request_reload();
        }
        if recorded_by_helper(result) {
            self.drop_extra_for(str_field(result, "job_id"));
        } else {
            self.keep_extra(result);
        }
    }
}

/// Watch the state file for the life of the process and feed events to the
/// connection loop. Runs in its own task: the loop must never wait on the
/// helper.
pub fn spawn_watcher(updater: Arc<Updater>) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let initial = updater.read_state();
        let reported = initial
            .as_ref()
            .is_some_and(|s| updater.already_reported(str_field(s, "job_id")));
        let mut tracker = Tracker::primed(initial.as_ref(), reported);
        let mut last_pending_probe: Option<Instant> = None;
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let state = updater.read_state();
            let now = Instant::now();
            let mut events = Vec::new();

            // A unit we started that shows no state: ask systemd, not a timer.
            let mut unit = UnitState::Unknown;
            if let Some(operation) = updater.pending_overdue(now)
                && last_pending_probe.is_none_or(|at| now.duration_since(at) >= UNIT_PROBE_INTERVAL)
            {
                last_pending_probe = Some(now);
                unit = updater.unit_state(&operation).await;
            }
            if let Some(result) = updater.settle_pending(state.as_ref(), now, unit) {
                events.push(Event::Result(result));
            }

            if let Some(state) = &state {
                updater.refresh_liveness(state).await;
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
                        updater.note_result(result);
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    Event::CheckDone => {
                        // A failure made up for this check is not owed any more.
                        if let Some(state) = &state {
                            updater.drop_extra_for(str_field(state, "job_id"));
                        }
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

    /// A fresh directory tree standing in for the host, with a fake
    /// `systemctl`; removed on drop.
    struct Host {
        root: PathBuf,
        paths: Paths,
    }

    /// `is-active` prints bin/unit-state (default `inactive`); anything else
    /// exits with bin/start-rc (default 0). Every call is logged.
    const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
d=$(dirname "$0")
echo "$*" >>"$d/calls"
case "$1" in
    is-active)
        s=$(cat "$d/unit-state" 2>/dev/null || echo inactive)
        # "error": what a D-Bus hiccup looks like — no answer, just a failure.
        [ "$s" != error ] || { echo "Failed to connect to bus" >&2; exit 1; }
        echo "$s"
        [ "$s" = active ]
        ;;
    *) exit "$(cat "$d/start-rc" 2>/dev/null || echo 0)" ;;
esac
"#;

    impl Host {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "portal-agent-test-{}-{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            for dir in [
                "etc/agent",
                "run",
                "state",
                "agent-state",
                "proc",
                "sbin",
                "bin",
            ] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            let systemctl = root.join("bin/systemctl");
            // Written by a child process, never by this one: a file this
            // process holds open for writing can be inherited by another
            // test's fork and then fails to exec with ETXTBSY.
            let ok = std::process::Command::new("sh")
                .args(["-c", r#"printf '%s' "$2" >"$1" && chmod 755 "$1""#, "sh"])
                .arg(&systemctl)
                .arg(FAKE_SYSTEMCTL)
                .status()
                .unwrap()
                .success();
            assert!(ok, "could not install the fake systemctl");
            let paths = Paths {
                conf: root.join("etc/gs-update.conf"),
                helper: root.join("sbin/portal-gs-update"),
                run_dir: root.join("run"),
                state_dir: root.join("state"),
                overrides: root.join("etc/agent/matchzy_overrides.cfg"),
                systemctl,
                agent_state_dir: root.join("agent-state"),
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

        fn agent_file(&self, name: &str) -> PathBuf {
            self.paths.agent_state_dir.join(name)
        }

        fn write_state(&self, state: &Value) {
            std::fs::write(self.paths.run_dir.join("state.json"), state.to_string()).unwrap();
        }

        fn write_last_job(&self, state: &Value) {
            std::fs::write(
                self.paths.state_dir.join("last-job.json"),
                state.to_string(),
            )
            .unwrap();
        }

        /// What `systemctl is-active` answers.
        fn unit_state(&self, state: &str) {
            std::fs::write(self.root.join("bin/unit-state"), state).unwrap();
        }

        /// Make `systemctl start` fail.
        fn start_fails(&self) {
            std::fs::write(self.root.join("bin/start-rc"), "1").unwrap();
        }

        fn systemctl_calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.join("bin/calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
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
    const JOB3: &str = "99999999-8888-4777-8666-555555555555";

    fn state(job: &str, operation: &str, status: &str, phase: &str, log: &str) -> Value {
        json!({
            "job_id": job, "operation": operation, "trigger": "portal", "pid": 4242,
            "status": status, "phase": phase, "log_tail": log, "needs_reload": false,
        })
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    impl Updater {
        /// The unit is definitely gone: two consecutive `inactive` answers.
        fn settle_pending_gone(&self, now: Instant) -> Option<Value> {
            let first = self.settle_pending(None, now, UnitState::Inactive);
            first.or_else(|| self.settle_pending(None, now, UnitState::Inactive))
        }
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
        // Everything printable that is not a quote, semicolon or backslash.
        let all: String = (0x20u8..=0x7e)
            .map(char::from)
            .filter(|c| !matches!(c, '"' | ';' | '\\'))
            .collect();
        let v = validate_overrides(&json!({ "matchzy_x": format!("a{all}z") })).unwrap();
        assert_eq!(v[0].1, format!("a{all}z"));
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
            "matchzy_X",
            "matchzy_x y",
            "matchzy_x;quit",
            "matchzy_x\nrcon_password",
            "matchzy_é",
            "matchzy_x\u{2028}",
            " matchzy_x",
        ] {
            assert!(
                validate_overrides(&json!({ name: "1" })).is_err(),
                "{name:?}"
            );
        }
        let ok = format!("matchzy_{}", "a".repeat(120));
        assert_eq!(ok.len(), 128);
        assert!(validate_overrides(&json!({ ok: "1" })).is_ok());
        let long = format!("matchzy_{}", "a".repeat(121));
        assert!(validate_overrides(&json!({ long: "1" })).is_err());
    }

    #[test]
    fn overrides_refuse_anything_but_printable_ascii_in_values() {
        for value in [
            "a\"b",
            "a;b",
            "a\\",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{0}b",
            "\u{7f}",
            // Not `char::is_control`, and exactly why the rule is an
            // allow-list: line and paragraph separators, NEL, NBSP,
            // zero-width and bidi format characters, plain non-ASCII.
            "1\u{2028}quit",
            "1\u{2029}quit",
            "1\u{85}quit",
            "1\u{a0}quit",
            "1\u{200b}quit",
            "1\u{202e}quit",
            "1\u{feff}quit",
            "café",
        ] {
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
        let s = |phase: &str, log: &str| state(JOB, "mods", "running", phase, log);

        // First sight of a job: immediately.
        let e = t.observe(Some(&s("snapshot", "a")), true, at(0));
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
            t.observe(Some(&s("snapshot", "a")), true, at(5000))
                .is_empty()
        );
        let e = t.observe(Some(&s("stop", "ab")), true, at(5100));
        assert_eq!(progress_phases(&e), ["stop"], "5s since the last frame");
        // Changes inside the window are held…
        assert!(
            t.observe(Some(&s("mods-update", "abc")), true, at(5400))
                .is_empty()
        );
        assert!(
            t.observe(Some(&s("mods-update", "abcd")), true, at(5900))
                .is_empty()
        );
        // …and the latest reading goes out once the second is up.
        let e = t.observe(Some(&s("mods-update", "abcd")), true, at(6100));
        assert_eq!(
            e,
            vec![Event::Progress(progress_frame(&s("mods-update", "abcd")))]
        );
        // A log-only change counts as a change.
        let e = t.observe(Some(&s("mods-update", "abcde")), true, at(7200));
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
            assert!(t.observe(Some(&done), false, now + secs(60)).is_empty());
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
        let mut t = Tracker::primed(Some(&done), false);
        assert!(t.observe(Some(&done), false, Instant::now()).is_empty());
        // But one still running when the agent starts is picked up.
        let running = state(JOB2, "mods", "running", "start", "");
        let mut t = Tracker::primed(Some(&running), false);
        assert_eq!(t.observe(Some(&running), true, Instant::now()).len(), 1);
        assert!(
            Tracker::primed(None, false)
                .observe(None, false, Instant::now())
                .is_empty()
        );
    }

    #[test]
    fn a_check_reports_through_update_state_not_progress() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let running = state(JOB, "check", "running", "check", "x");
        assert!(t.observe(Some(&running), true, now).is_empty());
        let done = state(JOB, "check", "succeeded", "done", "x");
        assert_eq!(t.observe(Some(&done), false, now), vec![Event::CheckDone]);
        assert!(t.observe(Some(&done), false, now).is_empty());
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
        assert!(t.observe(Some(&running), false, t0 + secs(1)).is_empty());
        assert!(t.observe(Some(&running), false, t0 + secs(4)).is_empty());
        let e = t.observe(Some(&running), false, t0 + secs(7));
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
        assert!(t.observe(Some(&running), false, t0 + secs(60)).is_empty());

        // The guess was wrong (liveness could not be seen, e.g. hidepid and a
        // job run by hand): when the helper does finish, the real result
        // still goes out — once.
        let done = state(JOB, "mods", "succeeded", "done", "");
        assert_eq!(
            t.observe(Some(&done), false, t0 + secs(90)),
            vec![Event::Result(done.clone())]
        );
        assert!(t.observe(Some(&done), false, t0 + secs(91)).is_empty());

        // The usual race — final state lands inside the grace — is a normal result.
        let mut t = Tracker::default();
        let running = state(JOB2, "mods", "running", "verify", "");
        t.observe(Some(&running), true, t0);
        assert!(t.observe(Some(&running), false, t0 + secs(1)).is_empty());
        let done = state(JOB2, "mods", "succeeded", "done", "");
        assert_eq!(
            t.observe(Some(&done), false, t0 + secs(2)),
            vec![Event::Result(done.clone())]
        );
    }

    #[test]
    fn a_stale_running_state_is_reported_once_not_after_every_agent_restart() {
        let host = Host::new();
        let stale = state(JOB, "mods", "running", "mods-update", "");
        host.write_state(&stale);
        let t0 = Instant::now();

        // First agent process: synthesizes the failure and keeps it.
        let updater = host.updater();
        let reported = updater.already_reported(JOB);
        assert!(!reported);
        let mut t = Tracker::primed(Some(&stale), reported);
        t.observe(Some(&stale), false, t0);
        let e = t.observe(Some(&stale), false, t0 + secs(6));
        let [Event::Result(dead)] = e.as_slice() else {
            panic!("expected the synthesized failure, got {e:?}");
        };
        updater.note_result(dead);

        // Agent restarts; the state file still says `running`.
        let restarted = host.updater();
        assert!(restarted.already_reported(JOB));
        let mut t = Tracker::primed(Some(&stale), restarted.already_reported(JOB));
        assert!(t.observe(Some(&stale), false, t0).is_empty());
        assert!(t.observe(Some(&stale), false, t0 + secs(60)).is_empty());
        // Unacknowledged, it is still owed to the portal — from the kept copy.
        assert_eq!(restarted.unacked_results(), vec![dead.clone()]);

        // Acknowledged: gone for good, also across another restart.
        restarted.ack(JOB);
        assert!(restarted.unacked_results().is_empty());
        let again = host.updater();
        assert!(again.already_reported(JOB));
        assert!(again.unacked_results().is_empty());
        let mut t = Tracker::primed(Some(&stale), again.already_reported(JOB));
        assert!(t.observe(Some(&stale), false, t0 + secs(600)).is_empty());
    }

    #[test]
    fn a_check_that_finishes_after_being_declared_dead_reports_as_a_check() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let running = state(JOB, "check", "running", "check", "");
        t.observe(Some(&running), false, t0);
        let e = t.observe(Some(&running), false, t0 + secs(6));
        assert!(
            matches!(e.as_slice(), [Event::Result(r)] if r["agent_synthesized"] == true),
            "{e:?}"
        );
        // It was alive after all and succeeded: that is an `update_state`,
        // exactly as if it had never been doubted — not an `update_result`.
        let done = state(JOB, "check", "succeeded", "done", "");
        assert_eq!(
            t.observe(Some(&done), false, t0 + secs(20)),
            vec![Event::CheckDone]
        );
        assert!(t.observe(Some(&done), false, t0 + secs(21)).is_empty());
        // A check that really failed is still a result.
        let mut t = Tracker::default();
        let running = state(JOB2, "check", "running", "check", "");
        t.observe(Some(&running), false, t0);
        t.observe(Some(&running), false, t0 + secs(6));
        let failed = state(JOB2, "check", "failed", "check", "");
        assert_eq!(
            t.observe(Some(&failed), false, t0 + secs(20)),
            vec![Event::Result(failed.clone())]
        );
    }

    // --- liveness ------------------------------------------------------------

    #[tokio::test]
    async fn liveness_prefers_the_unit_state_and_falls_back_to_proc() {
        let host = Host::new().capable();
        let updater = host.updater();
        let running = state(JOB, "mods", "running", "mods-update", "");
        host.write_state(&running);

        // No /proc entry (hidepid, or simply gone) and systemd says inactive.
        updater.refresh_liveness(&running).await;
        assert!(!updater.helper_alive(&running));
        assert_eq!(updater.running_job(), None);
        assert_eq!(
            host.systemctl_calls(),
            ["is-active portal-gs-update@mods.service"]
        );

        // systemd says the unit is active: alive, whatever /proc shows.
        host.unit_state("active");
        let updater = host.updater();
        updater.refresh_liveness(&running).await;
        assert!(updater.helper_alive(&running));
        assert!(updater.running_job().is_some());
        // The answer is for that job only.
        let other = state(JOB2, "mods", "running", "start", "");
        assert!(!updater.helper_alive(&other));
        // Asked at most every couple of seconds, not on every poll.
        let calls = host.systemctl_calls().len();
        updater.refresh_liveness(&running).await;
        updater.refresh_liveness(&running).await;
        assert_eq!(host.systemctl_calls().len(), calls);
        // Nothing to ask about a finished job.
        updater
            .refresh_liveness(&state(JOB3, "mods", "succeeded", "done", ""))
            .await;
        assert_eq!(host.systemctl_calls().len(), calls);

        // A job run from cron or by hand is not the unit: /proc decides.
        host.unit_state("inactive");
        let updater = host.updater();
        updater.refresh_liveness(&running).await;
        assert!(!updater.helper_alive(&running));
        host.helper_process(4242);
        assert!(updater.helper_alive(&running));

        for (answer, state) in [
            ("active", UnitState::Active),
            ("activating", UnitState::Active),
            ("deactivating", UnitState::Active),
            ("inactive", UnitState::Inactive),
            ("failed", UnitState::Inactive),
            // Neither yes nor no: it must not be read as "the unit is gone".
            ("error", UnitState::Unknown),
            ("maintenance", UnitState::Unknown),
        ] {
            host.unit_state(answer);
            assert_eq!(host.updater().unit_state("mods").await, state, "{answer}");
        }
        let mut broken = Host::new().capable();
        broken.paths.systemctl = broken.root.join("no-such-systemctl");
        assert_eq!(
            broken.updater().unit_state("mods").await,
            UnitState::Unknown
        );
    }

    // --- delivery: ack and replay --------------------------------------------

    #[test]
    fn a_result_is_replayed_on_every_connect_until_acknowledged() {
        let host = Host::new();
        let updater = host.updater();
        assert!(updater.unacked_results().is_empty(), "nothing ever ran");

        let done = state(JOB, "mods", "succeeded", "done", "tail");
        host.write_last_job(&done);
        // Since then a check overwrote state.json; the replay must not care.
        host.write_state(&state(JOB2, "check", "succeeded", "done", ""));

        // Sent is not received: it stays owed however often it is sent.
        assert_eq!(updater.unacked_results(), vec![done.clone()]);
        assert_eq!(updater.unacked_results(), vec![done.clone()]);
        assert_eq!(host.updater().unacked_results(), vec![done.clone()]);

        updater.ack(JOB);
        assert!(updater.is_acked(JOB));
        assert!(updater.unacked_results().is_empty());
        // Survives an agent restart…
        let restarted = host.updater();
        assert!(restarted.is_acked(JOB));
        assert!(restarted.unacked_results().is_empty());
        // …and the next job is owed again.
        let next = state(JOB2, "game", "failed", "verify", "");
        host.write_last_job(&next);
        assert_eq!(restarted.unacked_results(), vec![next.clone()]);

        // Acknowledging some other job changes nothing — in particular it
        // must not make an already acknowledged one look unsent.
        restarted.ack(JOB3);
        assert_eq!(restarted.unacked_results(), vec![next.clone()]);
        host.write_last_job(&done);
        assert!(restarted.unacked_results().is_empty(), "JOB is still acked");
        host.write_last_job(&next);
        restarted.ack(JOB2);
        assert!(host.updater().unacked_results().is_empty());
    }

    #[test]
    fn only_results_the_helper_did_not_record_are_kept_by_the_agent() {
        let host = Host::new();
        let updater = host.updater();
        let real = state(JOB, "mods", "succeeded", "done", "");
        host.write_last_job(&real);

        // The helper's own result: replayed from last-job.json, never
        // stored a second time (that was the duplicate on reconnect).
        updater.note_result(&real);
        assert_eq!(updater.unacked_results(), vec![real.clone()]);
        assert!(
            !host
                .paths
                .agent_state_dir
                .join("unacked-result.json")
                .exists()
        );

        // A failed check and a synthesized failure exist only here.
        let failed_check = state(JOB2, "check", "failed", "check", "");
        updater.note_result(&failed_check);
        assert_eq!(
            updater.unacked_results(),
            vec![real.clone(), failed_check.clone()]
        );
        let synthesized = json!({"job_id": JOB3, "operation": "game", "status": "failed", "agent_synthesized": true});
        updater.note_result(&synthesized);
        // One slot: the most recent. Never a backlog.
        assert_eq!(
            updater.unacked_results(),
            vec![real.clone(), synthesized.clone()]
        );
        assert_eq!(
            host.updater().unacked_results(),
            vec![real.clone(), synthesized.clone()],
            "kept across a restart"
        );
        updater.ack(JOB3);
        assert_eq!(updater.unacked_results(), vec![real.clone()]);
        assert_eq!(host.updater().unacked_results(), vec![real.clone()]);

        // A synthesized failure for the job the helper then really finished:
        // only the real one remains owed.
        let guess = json!({"job_id": JOB, "operation": "mods", "status": "failed", "agent_synthesized": true});
        updater.note_result(&guess);
        assert_eq!(updater.unacked_results(), vec![real.clone()]);
        updater.note_result(&real);
        assert!(
            !host
                .paths
                .agent_state_dir
                .join("unacked-result.json")
                .exists()
        );
    }

    #[test]
    fn a_result_goes_out_once_per_connection() {
        let real = state(JOB, "mods", "succeeded", "done", "");
        let guess = json!({"job_id": JOB, "operation": "mods", "status": "failed", "agent_synthesized": true});
        // Replayed on connect, then the watcher's queued copy arrives.
        let mut sent = SentThisConnection::default();
        assert!(sent.first_time(&real));
        assert!(!sent.first_time(&real));
        assert!(!sent.first_time(&real));
        // The guess and the real result for one job are different results.
        assert!(sent.first_time(&guess));
        assert!(!sent.first_time(&guess));
        assert!(sent.first_time(&state(JOB2, "game", "failed", "verify", "")));
        // A new connection starts over: unacknowledged results are resent.
        assert!(SentThisConnection::default().first_time(&real));
    }

    #[test]
    fn replay_ignores_garbage_and_unfinished_jobs() {
        let host = Host::new();
        let updater = host.updater();
        let last = host.paths.state_dir.join("last-job.json");
        std::fs::write(&last, "not json").unwrap();
        assert!(updater.unacked_results().is_empty());
        host.write_last_job(&state(JOB, "mods", "running", "start", ""));
        assert!(updater.unacked_results().is_empty());
        host.write_last_job(&json!({"status": "failed"}));
        assert!(updater.unacked_results().is_empty());
        // A corrupt ack file is not a reason to resend everything forever,
        // nor to trust what is in it.
        std::fs::write(
            host.paths.agent_state_dir.join("acked-jobs"),
            format!("garbage\n{JOB}\n../../etc/passwd\n"),
        )
        .unwrap();
        let updater = host.updater();
        assert!(updater.is_acked(JOB));
        assert!(!updater.is_acked("garbage"));
        // Only the last few acknowledgements are kept.
        for i in 0..20 {
            updater.ack(&format!("00000000-0000-4000-8000-{i:012}"));
        }
        assert!(!updater.is_acked(JOB));
        assert!(updater.is_acked("00000000-0000-4000-8000-000000000019"));
        let kept = std::fs::read_to_string(host.paths.agent_state_dir.join("acked-jobs")).unwrap();
        assert_eq!(kept.lines().count(), 8);
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
    fn job_ids_and_unit_names() {
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

        let mut request: Value = read_json(&host.paths.run_dir.join("request.json")).unwrap();
        assert!(
            request
                .as_object_mut()
                .unwrap()
                .remove("created_at")
                .is_some()
        );
        assert_eq!(
            request,
            json!({"job_id": JOB, "operation": "mods", "trigger": "portal"})
        );
        assert_eq!(
            host.systemctl_calls(),
            ["--no-ask-password start --no-block portal-gs-update@mods.service"]
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
        assert!(host.systemctl_calls().is_empty());
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
            ("verify-config", JOB),
            ("mods", "not-a-uuid"),
            ("mods", "0b0e3c5e-7f0a-4c1d-9b7e-1a2b3c4d5e6f; reboot"),
        ] {
            assert!(
                updater.start(operation, job, "portal").await.is_err(),
                "{operation:?} {job:?}"
            );
        }
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert!(host.systemctl_calls().is_empty());
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
        host.start_fails();
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
    async fn a_started_unit_is_not_failed_while_systemd_says_it_is_active() {
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("mods", JOB, "portal").await.unwrap();
        let now = Instant::now();
        let settle = |at: u64, unit| updater.settle_pending(None, now + secs(at), unit);

        // Within the grace period nothing is decided, and systemd not asked.
        assert_eq!(updater.pending_overdue(now + secs(5)), None);
        assert_eq!(settle(5, UnitState::Unknown), None);
        // After it: ask about exactly that unit…
        assert_eq!(
            updater.pending_overdue(now + secs(11)).as_deref(),
            Some("mods")
        );
        // …and as long as it is active (waiting for the lock behind the
        // nightly cron, or running but unable to publish), keep waiting —
        // for hours if need be.
        for at in [11, 61, 7200] {
            assert_eq!(settle(at, UnitState::Active), None);
        }
        assert!(updater.running_job().is_some());
        // Someone else's state does not settle it.
        let other = state(JOB2, "game", "succeeded", "done", "");
        assert_eq!(
            updater.settle_pending(Some(&other), now + secs(61), UnitState::Active),
            None
        );

        // The unit is gone and left nothing: that is a failure — once
        // systemd has said so twice.
        assert_eq!(
            settle(7201, UnitState::Inactive),
            None,
            "once is not enough"
        );
        let failure = settle(7203, UnitState::Inactive).unwrap();
        assert_eq!(failure["job_id"], JOB);
        assert_eq!(failure["operation"], "mods");
        assert_eq!(failure["status"], "failed");
        assert_eq!(failure["agent_synthesized"], true);
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("without reporting")
        );
        assert_eq!(updater.running_job(), None);
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert_eq!(settle(9000, UnitState::Inactive), None, "reported once");
        assert_eq!(settle(9002, UnitState::Inactive), None, "reported once");

        // The normal path: the helper picks it up, the claim just goes away.
        updater.start("mods", JOB2, "portal").await.unwrap();
        let picked_up = state(JOB2, "mods", "running", "init", "");
        assert_eq!(
            updater.settle_pending(Some(&picked_up), now + secs(300), UnitState::Inactive),
            None
        );
        assert_eq!(updater.pending_overdue(now + secs(9999)), None);
    }

    #[tokio::test]
    async fn a_systemctl_error_is_not_proof_that_the_unit_is_gone() {
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("mods", JOB, "portal").await.unwrap();
        let now = Instant::now();
        let settle = |at: u64, unit| updater.settle_pending(None, now + secs(at), unit);

        // systemctl failing (D-Bus hiccup, daemon-reload) says nothing,
        // however often it happens.
        for at in [11, 13, 15, 600] {
            assert_eq!(settle(at, UnitState::Unknown), None, "{at}s");
        }
        // One "inactive" between "active" answers (a unit restart) is not
        // two in a row; an error in between does not make it so either.
        assert_eq!(settle(602, UnitState::Inactive), None);
        assert_eq!(settle(604, UnitState::Active), None);
        assert_eq!(settle(606, UnitState::Inactive), None);
        assert_eq!(settle(608, UnitState::Unknown), None);
        assert!(updater.running_job().is_some());
        assert!(settle(610, UnitState::Inactive).is_some());
    }

    #[tokio::test]
    async fn a_job_that_could_not_publish_is_reported_from_the_helpers_record() {
        // The run directory was full: no state.json ever appeared, but the
        // helper ran and recorded its result. That is the result — not "the
        // helper did not start".
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("repair-config", JOB, "portal").await.unwrap();
        let recorded = state(JOB, "repair-config", "succeeded", "done", "");
        host.write_last_job(&recorded);
        let got = updater.settle_pending(None, Instant::now() + secs(1), UnitState::Active);
        assert_eq!(got, Some(recorded.clone()));
        assert!(!is_synthesized(&recorded));
        assert_eq!(updater.running_job(), None);
        // An older job's record is not ours.
        updater
            .start("repair-config", JOB2, "portal")
            .await
            .unwrap();
        assert_eq!(
            updater.settle_pending(None, Instant::now() + secs(1), UnitState::Active),
            None
        );
    }

    #[tokio::test]
    async fn the_request_is_stamped_so_a_stale_one_is_never_adopted() {
        let host = Host::new().capable();
        let before = unix_now();
        host.updater().start("mods", JOB, "portal").await.unwrap();
        let request: Value = read_json(&host.paths.run_dir.join("request.json")).unwrap();
        let created = request["created_at"].as_u64().unwrap();
        assert!((before..=unix_now()).contains(&created), "{created}");
    }

    // --- surviving an agent restart ------------------------------------------

    #[tokio::test]
    async fn a_job_started_just_before_an_agent_restart_still_gets_a_result() {
        let host = Host::new().capable();
        host.updater().start("mods", JOB, "portal").await.unwrap();
        assert!(host.agent_file("pending-job.json").exists());

        // The agent is restarted (deb upgrade) before the helper wrote
        // anything. The new process knows about the job…
        let restarted = host.updater();
        assert_eq!(
            restarted.running_job(),
            Some(
                json!({"job_id": JOB, "operation": "mods", "phase": "starting", "status": "running"})
            )
        );
        let err = restarted
            .start("game", JOB2, "portal")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already running"), "{err}");
        // …keeps its age (the grace period is not started over)…
        let now = Instant::now();
        assert_eq!(
            restarted.pending_overdue(now + secs(11)).as_deref(),
            Some("mods")
        );
        // …and reports how it ended: here, the unit died without a trace.
        let failure = restarted.settle_pending_gone(now + secs(11)).unwrap();
        assert_eq!(failure["job_id"], JOB);
        assert!(!host.agent_file("pending-job.json").exists());
        assert_eq!(host.updater().running_job(), None, "settled for good");

        // Or the helper picked it up meanwhile: the claim just goes away.
        host.updater().start("mods", JOB2, "portal").await.unwrap();
        let restarted = host.updater();
        let picked_up = state(JOB2, "mods", "running", "stop", "");
        assert_eq!(
            restarted.settle_pending(Some(&picked_up), now, UnitState::Unknown),
            None
        );
        assert!(!host.agent_file("pending-job.json").exists());

        // A start that systemd refused leaves nothing behind either.
        let host = Host::new().capable();
        host.start_fails();
        assert!(host.updater().start("mods", JOB, "portal").await.is_err());
        assert!(!host.agent_file("pending-job.json").exists());
        // Garbage in the file is not a job.
        std::fs::write(
            host.agent_file("pending-job.json"),
            r#"{"job_id": "x; reboot", "operation": "mods", "started_at": 1}"#,
        )
        .unwrap();
        assert_eq!(host.updater().running_job(), None);
        std::fs::write(
            host.agent_file("pending-job.json"),
            json!({"job_id": JOB, "operation": "rm -rf", "started_at": 1}).to_string(),
        )
        .unwrap();
        assert_eq!(host.updater().running_job(), None);
    }

    #[test]
    fn a_pending_reload_survives_an_agent_restart() {
        let host = Host::new();
        let updater = host.updater();
        let mut repaired = state(JOB, "repair-config", "succeeded", "done", "");
        repaired["needs_reload"] = json!(true);
        updater.note_result(&repaired);
        assert!(updater.reload_pending());
        // Restarted before the server was idle enough to reload.
        let restarted = host.updater();
        assert!(restarted.reload_pending(), "lost with the process");
        restarted.reload_done();
        assert!(!restarted.reload_pending());
        assert!(!host.updater().reload_pending(), "done is done");
    }

    #[test]
    fn the_auto_repair_backoff_survives_an_agent_restart() {
        let broken = health(Some(true), Some(false));
        let host = Host::new().capable();
        let t0 = Instant::now();
        // Three repairs that did not help: the next is due 40 minutes later.
        let updater = host.updater();
        assert!(updater.claim_auto_repair(broken, t0));
        assert!(updater.claim_auto_repair(broken, t0 + secs(600)));
        assert!(updater.claim_auto_repair(broken, t0 + secs(1800)));
        assert!(!updater.claim_auto_repair(broken, t0 + secs(1800 + 2399)));

        // A restart must not hand it a fresh start. (The persisted time is
        // "now" for this test, the streak is what was reached.)
        let restarted = host.updater();
        let t1 = Instant::now();
        assert!(!restarted.claim_auto_repair(broken, t1));
        assert!(!restarted.claim_auto_repair(broken, t1 + secs(600)));
        assert!(!restarted.claim_auto_repair(broken, t1 + secs(2390)));
        assert!(restarted.claim_auto_repair(broken, t1 + secs(2405)));
        // A healthy reading resets it, on disk too.
        assert!(!restarted.claim_auto_repair(health(Some(true), Some(true)), t1 + secs(2500)));
        let again = host.updater();
        let t2 = Instant::now();
        assert!(again.claim_auto_repair(broken, t2 + secs(600)));
        assert!(
            again.claim_auto_repair(broken, t2 + secs(1200)),
            "back to 10 minutes"
        );
    }

    // --- auto-repair, reload -------------------------------------------------

    fn health(wired: Option<bool>, applied: Option<bool>) -> ConfigHealth {
        ConfigHealth {
            demo_upload_wired: wired,
            overrides_applied: applied,
            auto_repair: true,
        }
    }

    #[tokio::test]
    async fn auto_repair_only_when_definitely_broken_idle_and_allowed() {
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
        // The operator's switch (AUTO_REPAIR=false in gs-update.conf).
        let off = ConfigHealth {
            auto_repair: false,
            ..unwired
        };
        assert!(!updater.claim_auto_repair(off, t0));
        assert!(!updater.claim_auto_repair(off, t0 + secs(100_000)));

        assert!(updater.claim_auto_repair(unwired, t0));
        assert!(!updater.claim_auto_repair(unwired, t0 + secs(30)));
        // Overrides not applied is a reason too.
        let host = Host::new().capable();
        assert!(
            host.updater()
                .claim_auto_repair(health(Some(true), Some(false)), t0)
        );

        // Never on top of a running job, and that does not use up the slot.
        let host = Host::new().capable();
        let updater = host.updater();
        updater.start("mods", JOB, "portal").await.unwrap();
        assert!(!updater.claim_auto_repair(unwired, t0));
        updater.settle_pending_gone(Instant::now() + secs(61));
        assert!(updater.claim_auto_repair(unwired, t0));

        // Not on a host without the helper.
        assert!(!Host::new().updater().claim_auto_repair(unwired, t0));
    }

    #[test]
    fn auto_repair_backs_off_while_repairs_do_not_help_and_resets_when_healthy() {
        let broken = health(Some(true), Some(false));
        let host = Host::new().capable();
        let updater = host.updater();
        let t0 = Instant::now();
        let mut t = t0;
        let mut gaps = Vec::new();
        assert!(updater.claim_auto_repair(broken, t));
        // Poll once a minute, as heartbeats would, for three days.
        let mut last = t;
        for _ in 0..(3 * 24 * 60) {
            t += secs(60);
            if updater.claim_auto_repair(broken, t) {
                gaps.push(t.duration_since(last).as_secs() / 60);
                last = t;
            }
        }
        // 10 min, 20, 40, … then every 6 hours: not every 10 minutes forever.
        assert_eq!(&gaps[..7], [10, 20, 40, 80, 160, 320, 360]);
        assert!(gaps[7..].iter().all(|g| *g == 360), "{gaps:?}");

        // One healthy reading and the next breakage is repaired promptly.
        assert!(!updater.claim_auto_repair(health(Some(true), Some(true)), t));
        t += secs(600);
        assert!(updater.claim_auto_repair(broken, t));
        assert!(!updater.claim_auto_repair(broken, t + secs(599)));
        assert!(updater.claim_auto_repair(broken, t + secs(600)));
        // "Unknown" is not "healthy": it does not reset anything.
        assert!(!updater.claim_auto_repair(health(None, None), t + secs(700)));
        assert!(!updater.claim_auto_repair(broken, t + secs(1200)));
        assert!(updater.claim_auto_repair(broken, t + secs(1800)));
    }

    #[test]
    fn reload_waits_for_an_idle_server() {
        assert!(reload_allowed(Some(&json!({"gamestate": "none"}))));
        for busy in ["warmup", "knife", "live", "paused", "post_game", ""] {
            assert!(
                !reload_allowed(Some(&json!({"gamestate": busy}))),
                "{busy:?}"
            );
        }
        // No answer (RCON down, plugin stack not loaded) is not "idle".
        assert!(!reload_allowed(None));
        assert!(!reload_allowed(Some(&json!({}))));
        assert!(!reload_allowed(Some(&json!({"gamestate": null}))));
    }

    #[test]
    fn a_reload_backs_off_while_matchzy_is_not_loaded() {
        let idle = json!({"gamestate": "none"});
        let live = json!({"gamestate": "live"});
        // The normal cases: reload when idle, wait while a match is loaded.
        assert_eq!(reload_step(Some(&idle), None), ReloadStep::Exec);
        assert_eq!(reload_step(Some(&live), None), ReloadStep::Wait);
        assert_eq!(reload_step(None, None), ReloadStep::Wait);
        assert_eq!(reload_retry_delay(None), secs(10));
        // MatchZy silent (plugin stack not loaded): keep trying for a few
        // minutes, then stop asking every 10 seconds.
        assert_eq!(reload_step(None, Some(secs(60))), ReloadStep::Wait);
        assert_eq!(reload_retry_delay(Some(secs(60))), secs(10));
        assert_eq!(reload_retry_delay(Some(secs(179))), secs(10));
        assert_eq!(reload_retry_delay(Some(secs(180))), secs(300));
        assert_eq!(reload_retry_delay(Some(secs(86_400))), secs(300));
        assert_eq!(reload_step(None, Some(secs(86_400))), ReloadStep::Wait);
        // A short silence (a map change) changes nothing…
        assert_eq!(reload_step(Some(&idle), Some(secs(60))), ReloadStep::Exec);
        assert_eq!(reload_step(Some(&live), Some(secs(60))), ReloadStep::Wait);
        // …but when it answers again after a long one it has just been
        // loaded, and read the config doing so: nothing left to do, whatever
        // state it is in by now.
        assert_eq!(reload_step(Some(&idle), Some(secs(180))), ReloadStep::Done);
        assert_eq!(reload_step(Some(&live), Some(secs(3600))), ReloadStep::Done);
    }

    #[test]
    fn a_repair_leaves_a_reload_pending_until_it_is_done() {
        let host = Host::new();
        let updater = host.updater();
        assert!(!updater.reload_pending());
        let mut repaired = state(JOB, "repair-config", "succeeded", "done", "");
        // Not every result asks for one…
        updater.note_result(&repaired);
        assert!(!updater.reload_pending());
        // …a failed repair does not either…
        let mut failed = state(JOB2, "repair-config", "failed", "reconcile-config", "");
        failed["needs_reload"] = json!(true);
        updater.note_result(&failed);
        assert!(!updater.reload_pending());
        // …a successful one does, and it stays set (match in progress, RCON
        // down) until the reload has actually happened.
        repaired["needs_reload"] = json!(true);
        updater.note_result(&repaired);
        assert!(updater.reload_pending());
        assert!(updater.reload_pending());
        updater.reload_done();
        assert!(!updater.reload_pending());
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
        assert!(updater.config_health().auto_repair);

        write_config(
            &host,
            &json!({
                "checked_at": "2026-11-21T04:00:00Z", "demo_upload_wired": true,
                "overrides_applied": null, "reason": null, "auto_repair": true,
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
        assert!(
            updater.config_health().auto_repair,
            "absent (older helper) means allowed"
        );
        write_config(
            &host,
            &json!({"demo_upload_wired": null, "overrides_applied": false, "auto_repair": false}),
        );
        assert_eq!(updater.config_health().demo_upload_wired, None);
        assert!(updater.config_health().needs_repair());
        assert!(!updater.config_health().auto_repair);

        // Garbage, or the wrong types, is "unknown" — never "broken".
        std::fs::write(host.paths.run_dir.join("config.json"), "{ not json").unwrap();
        assert_eq!(updater.config_health(), ConfigHealth::default());
        write_config(
            &host,
            &json!({"demo_upload_wired": "false", "overrides_applied": 0, "auto_repair": "no"}),
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
        let set_age = |age: u64| {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(std::time::SystemTime::now() - secs(age))
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
        assert!(!updater.verify_config(false, t0 + secs(299)).await);
        assert!(
            updater.verify_config(true, t0 + secs(10)).await,
            "connect forces"
        );
        assert!(
            !updater.verify_config(false, t0 + secs(300)).await,
            "counted from the last run"
        );
        assert!(updater.verify_config(false, t0 + secs(310)).await);
        // It is not a job: nothing is claimed, no request is written.
        assert_eq!(updater.running_job(), None);
        assert!(!host.paths.run_dir.join("request.json").exists());
        assert_eq!(
            host.systemctl_calls(),
            ["--no-ask-password start --no-block portal-gs-update@verify-config.service"; 3]
        );

        // Never while a job runs, not even on connect; and the turn is kept.
        updater.start("mods", JOB, "portal").await.unwrap();
        assert!(!updater.verify_config(true, t0 + secs(1000)).await);
        updater.settle_pending_gone(Instant::now() + secs(61));
        assert!(updater.verify_config(false, t0 + secs(1000)).await);

        // Not on a host without the helper; a failing systemctl is not fatal.
        assert!(!Host::new().updater().verify_config(true, t0).await);
        let broken = Host::new().capable();
        broken.start_fails();
        assert!(!broken.updater().verify_config(true, t0).await);
    }
}
