# portal-server-agent

The game-server half of the portal's MatchZy integration
(`docs/matchzy-integration.md` in the portal repo, §5). Runs on a CS2 host
beside MatchZy, maintains an **outbound** mTLS WebSocket to the portal, and
executes match-setup commands via RCON on `127.0.0.1`. The portal never
connects inbound and never sees the RCON password.

## Host prerequisites

1. CS2 dedicated server started with `-usercon` and a strong `rcon_password`.
2. [Metamod:Source](https://www.sourcemm.net/) + [CounterStrikeSharp](https://github.com/roflmuffin/CounterStrikeSharp)
   + [MatchZy](https://github.com/shobhit-pathak/MatchZy) ≥ 0.8.15.
3. `tv_enable 1` (GOTV) if demo recording is wanted.
4. Recommended in `csgo/cfg/MatchZy/config.cfg`:
   `matchzy_kick_when_no_match_loaded true` (managed servers are match-only).

## Install

```bash
# 1. Install the deb — download from the latest release + verify:
#    https://github.com/mtottenh/cg-server-agent/releases
#    (sha256sum -c SHA256SUMS with both files in cwd)
sudo apt install ./portal-server-agent_*.deb
#    postinst creates the portal-agent system user, seeds
#    /etc/portal/portal-server-agent.env from the example, and enables
#    (but does not start) the unit.

# 2. Enroll: a portal admin mints a one-time token
#    (admin UI → Game Servers → Enrollment token, or
#     `portal-cli gameserver enroll-token <server-id>`)
sudo portal-server-agent enroll \
    --url https://portal.example.com \
    --token cgs_... \
    --dir /etc/portal/agent
#    Material lands in /etc/portal/agent owned by portal-agent (enroll
#    chowns it to the service user automatically when the deb is installed).

# 3. Configure
sudo $EDITOR /etc/portal/portal-server-agent.env   # URL + RCON password

# 4. Start
sudo systemctl start portal-server-agent
```

The server flips from `offline` to `available` in the portal admin UI on
the first heartbeat (~30s).

## How it authenticates

Enrollment generates a keypair locally and sends only a CSR; the portal
signs it with its private CA, binding the certificate CN to the server's
UUID. Every connection presents that client certificate; the portal's
reverse proxy verifies the chain and the API checks the serial against its
revocation registry. Rotation is automatic-in-design (re-enrollment over
the established channel — Phase 4); until then re-run `enroll` with a fresh
token before the 90-day expiry.

## Local metrics (optional)

Set `METRICS_ADDR=127.0.0.1:9469` to expose a loopback Prometheus
`/metrics` endpoint for this game host's operator:

| Metric | Meaning |
|---|---|
| `agent_ws_connected` | 1 while the portal WebSocket is up |
| `agent_reconnects_total` | Reconnect attempts (any disconnect) |
| `agent_backoff_seconds` | Current reconnect backoff (0 while connected) |
| `agent_heartbeats_sent_total` | Heartbeats delivered to the portal |
| `agent_rcon_commands_total{command,outcome}` | Portal-issued commands by verb and result |
| `agent_rcon_duration_seconds{command}` | Command execution time (RCON round-trips) |
| `agent_build_info{version}` | Installed agent version |

This endpoint is for local diagnostics only — the portal monitors agents
through its own heartbeat aggregation and never scrapes game hosts. Demo
uploads don't appear here because the agent never handles demo files:
MatchZy uploads them straight to the portal.

## Dev mode

Against a local portal (`PORTAL_AGENT_INSECURE=true` on the API):

```bash
portal-server-agent \
    --url ws://localhost:3000/v1/gameserver/agent/ws \
    --dev-server-id <server-uuid> \
    --rcon-addr 127.0.0.1:27015 --rcon-password dev
```

## Game-server updates (LinuxGSM hosts, 0.3.0+)

On a host managed by LinuxGSM the deb also installs `portal-gs-update`, the
one thing that drives LinuxGSM (design: `docs/gameserver-lgsm-updates-design.md`
in the portal repo). It runs as root, from three places: the portal (through
the agent), the nightly cron, and you over SSH.

```bash
sudo portal-gs-update check          # installed vs latest; touches nothing
sudo portal-gs-update lgsm           # update LinuxGSM itself
sudo portal-gs-update game           # CS2 build + gameinfo.gi repair; restarts only if something changed
sudo portal-gs-update mods           # Metamod / CounterStrikeSharp / MatchZy, with rollback
sudo portal-gs-update all            # lgsm, then game and mods with one restart
sudo portal-gs-update repair-config  # re-assert the managed MatchZy config blocks; no restart
sudo portal-gs-update verify-config  # are those blocks in place? writes config.json; changes nothing
```

**Opting in.** The helper does nothing until `/etc/portal/gs-update.conf`
exists (see `/etc/portal/gs-update.conf.example`; configuration management
renders it). Without it the helper exits 78 and the agent does not advertise
the `lgsm_update` capability, so the portal hides the feature.

**What `mods` guarantees.** It snapshots `addons/` and `cfg/MatchZy/`
(`/var/lib/portal-gs-update/snapshots`, newest 3 kept), stops the server,
runs `mods-update`, puts the managed config blocks back, starts the server
and then waits for the console to show that CounterStrikeSharp and MatchZy
actually loaded. If they did not — or `mods-update` itself failed — it
restores the snapshot, starts the previous set and reports `rolled_back`
with the console line that caused it.

**The MatchZy config.** `mods-update` overwrites `cfg/MatchZy/config.cfg`,
and with it the demo-upload token. The helper re-asserts two blocks at the
end of that file:

- `// BEGIN/END ANSIBLE MANAGED — portal demo upload` — the contents of
  `/etc/portal/agent/matchzy_portal.cfg`, byte-identical to what the Ansible
  `blockinfile` task writes, so a later converge changes nothing;
- `// BEGIN/END PORTAL MANAGED — matchzy overrides` — `cvar "value"` lines
  from `/etc/portal/agent/matchzy_overrides.cfg`, which the agent writes
  when the portal sends `set_matchzy_overrides`.

Anything else an operator changed in the overwritten files is lost by the
update; the job result lists it (`config_report`).

Whether the blocks are in place is checked **in the file**, by the helper
(`verify-config`, and at the end of every job), which writes
`/run/portal-gs-update/config.json`. The running server cannot be asked:
MatchZy's settings are console commands, not cvars, and read back empty
over RCON. The agent starts `verify-config` on every connect and every
5 minutes, reports the result in its heartbeat, and — if someone ran
`./cs2server mods-update` by hand and the demo-upload block is gone, or the
portal's overrides are not in the file — starts `repair-config` itself. It
restarts nothing. It is rate-limited: 10 minutes after the first repair,
doubling up to 6 hours while repairs do not leave the config healthy, back
to 10 minutes once it is. `AUTO_REPAIR=false` in `gs-update.conf` switches
it off (the agent then only reports). After a `repair-config` the agent
makes the running server re-read the file with `exec MatchZy/config.cfg`
over its local RCON — but only while MatchZy reports no match loaded
(`get5_status` gamestate `none`); until then, and while RCON is down, the
reload stays pending and is retried every 10 seconds.

**When a job is interrupted.** A job waits up to 30 s for the lock (a
`verify-config` holds it for a moment); `verify-config` itself never waits.
If the helper is stopped mid-job (SIGTERM, unit timeout) it kills the
LinuxGSM command it was running and then puts back what it took apart: if
plugin files may be half-updated it restores the snapshot, and if it had
stopped the server it starts it again and checks that the plugins loaded.
The result says what was done. The unit uses `KillMode=process` — the CS2
tmux server lives in the unit's cgroup and must survive the helper — so
this clean-up is the helper's job, not systemd's.

**Privileges.** The agent stays unprivileged (`NoNewPrivileges`, no sudo, no
access to `/home`). It starts `portal-gs-update@<operation>.service`
through systemd; a polkit rule
(`/usr/share/polkit-1/rules.d/50-portal-gs-update.rules`) allows the
`portal-agent` user to *start* exactly those seven instances (the six
operations and `verify-config`) and nothing else. The operation is the only input; the portal's job id travels in
`/run/portal-gs-update/request.json`. The job runs outside the agent, so it
survives an agent restart or upgrade and a dropped connection.

| Path | Owner | What |
|---|---|---|
| `/etc/portal/gs-update.conf` | config management | where LinuxGSM lives; presence = opted in |
| `/run/portal-gs-update/state.json` | helper | the current/last operation, rewritten at every phase |
| `/run/portal-gs-update/config.json` | helper | `{checked_at, demo_upload_wired, overrides_applied, reason}` — the managed blocks as found in the MatchZy config |
| `/run/portal-gs-update/request.json` | agent | `{job_id, operation, trigger}` for the unit about to start |
| `/run/portal-gs-update/lock` | helper | one operation at a time (a second one exits 75) |
| `/var/lib/portal-gs-update/check.json` | helper | last `check` |
| `/var/lib/portal-gs-update/installed.json` | helper | versions recorded by the last successful `mods`/`all` |
| `/var/lib/portal-gs-update/last-job.json` | helper | last finished non-check job (replayed to the portal until acknowledged) |
| `/var/lib/portal-server-agent/` | agent | `acked-jobs` (results the portal acknowledged), `unacked-result.json` (the one result only the agent has) |
| `/var/lib/portal-gs-update/snapshots/` | helper, 0700 | plugin snapshots for rollback |
| `/etc/portal/agent/matchzy_overrides.cfg` | agent | portal-managed MatchZy overrides |

Helper exit codes: 0 succeeded, 1 failed, 3 rolled back, 64 bad usage,
65 bad `request.json`, 69 missing dependency, 75 another operation still
holds the lock (after 30 s for a job; at once for `verify-config`), 78 not
configured, 143 stopped by SIGTERM (after recovery). Full output goes to
the journal (`journalctl -t portal-gs-update`). Lines mentioning
`matchzy_demo_upload_header_value`, `rcon_password`, `sv_password`,
`tv_password` or `Bearer ` are redacted — whole line, whatever bytes it
contains — before they are written anywhere.

The helper is root code next to two less privileged users, so: everything
in the game tree is read and written as the LinuxGSM user (files are
replaced by rename); the agent-writable inputs (`matchzy_portal.cfg`,
`matchzy_overrides.cfg`, `request.json`) are read as the agent user, in one
open that refuses symlinks and FIFOs and stops at a size cap; and nothing
in `/run/portal-gs-update` is ever read by root.

Tests: `tests/helper/run.sh` (fake game tree + fake LinuxGSM; no root).

## Protocol

JSON text frames over the WebSocket. Everything added in 0.3.0 is additive:
a portal that predates it ignores the new heartbeat fields and frame types,
and never sends the new commands.

### Portal → agent: commands

`{id, cmd, args}` → `{id, ok: true, output}` or `{id, ok: false, error}`.
`output` is always a string.

| `cmd` | `args` | `output` |
|---|---|---|
| `load_match` | `{url, header_name?, header_value?}` | console output |
| `end_match` | — | console output |
| `status` | — | `get5_status` output |
| `load_backup` | `{url, header_name?, header_value?}` | console output |
| `roster_edit` | `{remove: [steamid64], add: [{steamid64, team, name}]}` | console output |
| `exec` | `{command}` | console output |
| `update_check` | — | JSON, below |
| `update_start` | `{operation, job_id}` | JSON, below |
| `update_status` | — | JSON, below |
| `update_ack` | `{job_id}` | JSON, below |
| `set_matchzy_overrides` | `{overrides: {cvar: value}}` | JSON, below |

`exec` runs one console command per frame. The agent refuses, with
`{ok: false, error: "refused: …"}`, anything containing `;` or control
characters, and any command whose verb is portal-owned (`rcon_password`,
`sv_password`, `tv_password`, `sv_rcon_*`, `matchzy_remote_*`,
`matchzy_demo_upload_*`, `matchzy_loadmatch*`, `matchzy_loadbackup_url`,
`logaddress_*`, `sv_downloadurl`, `host_writeconfig`), could hide another
command (`alias`, `exec`), or would end the process (`quit`, `exit`,
`_restart`, `killserver`, `crash`). The portal applies the same rule first;
this copy is defence in depth.

#### Update commands (0.3.0+)

All four answer immediately — the work happens in the helper and is
reported by the pushed frames below. For these, `output` is a JSON document
encoded as a string. Except for `update_status`, they fail with
`ok: false` and
`error: "refused: this host has no LinuxGSM update helper configured"` when
the agent does not advertise `lgsm_update`.

**`update_check`** — starts `check`. Output:
`{"started": true, "job_id": "<uuid the agent generated>", "operation": "check"}`.
The result arrives as an `update_state` frame.

**`update_start`** — `args.operation` is one of `lgsm`, `game`, `mods`,
`all`, `repair-config`; `args.job_id` is the portal's job id and must match
`^[0-9a-f-]{36}$` (a lowercase UUID). Output:
`{"started": true, "job_id": "<job_id>", "operation": "<operation>"}`.
Errors:

- `refused: update job <job_id> (<operation>) is already running` — one job
  at a time, including a `check` and jobs started by cron or over SSH;
- `unknown operation (expected one of: lgsm, game, mods, all, repair-config)`;
- `job_id must be a lowercase UUID`;
- `could not start portal-gs-update@<operation>.service: …` — systemd or
  polkit said no.

`started: true` means systemd accepted the start. The helper may then wait
up to 30 s for the lock. If no state appears, the agent asks systemd about
the unit (after 10 s, then every 2 s): while it is active the job is simply
still `starting`; once the unit is gone without a trace (it never got the
lock, the request was refused, the unit failed) the agent sends an
`update_result` with `status: "failed"`, `phase: "starting"` and
`agent_synthesized: true`.

**`update_status`** — output is the current content of `state.json`
(the object described under `update_result`, without `type`), or `null` if
no operation has run since boot.

**`update_ack`** — `args.job_id` (`^[0-9a-f-]{36}$`): the portal has stored
the `update_result` for this job. Output: `{"acked": "<job_id>"}`. Always
succeeds for a well-formed id, known or not, repeated or not. Until a
result is acknowledged the agent sends it again on every (re)connect — see
"Delivery" below. Works without the `lgsm_update` capability.

**`set_matchzy_overrides`** — replaces the whole override set; `{}` clears
it. Rules, all-or-nothing (one bad entry rejects the request and leaves the
previous file in place):

- at most 64 entries;
- name matches `^matchzy_[a-z0-9_]+$` (≤ 128 chars) and is not
  `matchzy_demo_upload_*`, `matchzy_remote_*`, `matchzy_loadmatch*` or
  `matchzy_loadbackup_url`;
- value is a string, number or boolean; as text it is ≤ 256 bytes of
  printable ASCII (0x20–0x7E) without `"`, `;` or `\` — so no newlines,
  tabs, or any non-ASCII character (U+2028 and its relatives included).
  Leading and trailing spaces of strings are dropped.

The helper applies the same rules to the file. Should a line in it not pass
(only possible if the file was written by something other than the agent),
that line is left out and `overrides_applied` is `false` with the reason.

The agent writes the file and starts `repair-config` to apply it. Output:
`{"written": <n>, "started": true, "job_id": "<uuid>", "operation": "repair-config"}`,
or, when another job is running,
`{"written": <n>, "started": false, "job_id": null, "reason": "refused: update job … is already running"}`
— the file is in place and is applied by the next `mods`, `all` or
`repair-config`.

### Agent → portal: heartbeat

Every 30 s:

```json
{
  "type": "heartbeat",
  "agent_version": "0.3.0",
  "rcon_ok": true,
  "get5_status": { },
  "status_output": "…",
  "capabilities": ["lgsm_update"],
  "demo_upload_wired": true,
  "overrides_applied": null,
  "reload_pending": false,
  "update_job": {"job_id": "…", "operation": "mods", "phase": "verify", "status": "running"}
}
```

- `get5_status`, `status_output` — only when `rcon_ok`. `status_output`
  (0.2.0+) is the raw output of CS2's `status`, bounded to 8 KiB; the portal
  parses it for the current map and the connected players.
- `capabilities` (0.3.0+) — `["lgsm_update"]` when both
  `/etc/portal/gs-update.conf` and `/usr/sbin/portal-gs-update` exist,
  otherwise `[]`.
- `demo_upload_wired` (0.3.0+) — from the helper's last look at the MatchZy
  config **file** (`config.json`; independent of `rcon_ok`). `true`: the
  demo-upload block is present, byte-identical to the enrollment snippet,
  and no later line sets a `matchzy_demo_upload_*` command over it.
  `false`: the block is missing, differs, or is overridden. `null`: unknown
  — no enrollment snippet on this host, no capability, or no report newer
  than 15 minutes (the agent refreshes it on connect and every 5 minutes,
  so `null` right after a connect clears within one heartbeat).
- `overrides_applied` (0.3.0+) — same source. `true`: the overrides block
  matches what `set_matchzy_overrides` last wrote and no later line sets
  one of those commands; `false`: it does not; `null`: no overrides were
  ever set on this host, or unknown as above.
- When either is `false` the agent starts `repair-config` itself (trigger
  `auto_repair`), never while a job runs, unless the host's
  `gs-update.conf` says `AUTO_REPAIR=false`. First repair at once, the next
  no sooner than 10 minutes later, doubling to at most 6 hours while the
  config does not come back healthy.
- `reload_pending` (0.3.0+) — `true` while a `repair-config` has changed
  the MatchZy config file and the running server has not re-read it yet
  (the agent waits for RCON and for no match to be loaded).
- `update_job` (0.3.0+) — the operation in flight, or `null`. `phase` is
  `starting` between the agent starting the unit and the helper's first
  state write. `status` is always `running` here.

### Agent → portal: update frames (0.3.0+)

**`update_progress`** — while a job other than `check` runs, whenever
`state.json` changed, at most one per second per job (the first one for a
job is sent at once). Dropped, not queued, while the portal is unreachable.

```json
{"type": "update_progress", "job_id": "…", "operation": "mods",
 "phase": "mods-update", "status": "running", "trigger": "portal",
 "log_tail": "…last ≤ 4 KiB of the job log…"}
```

**`update_result`** — once per job, when it reaches a terminal status. It is
`state.json` plus `type`:

```json
{
  "type": "update_result",
  "job_id": "…", "operation": "mods",
  "trigger": "portal",
  "status": "succeeded",
  "phase": "done", "failed_phase": null,
  "started_at": "2026-11-21T04:00:00Z", "finished_at": "2026-11-21T04:03:10Z",
  "before": {"lgsm": "v26.6.0", "cs2_build": "20012345", "metamod": null, "cssharp": "1.0.370", "matchzy": "0.9.2"},
  "after":  {"lgsm": "v26.6.0", "cs2_build": "20012345", "metamod": "mmsource-2.0.0-git1491-linux.tar.gz", "cssharp": "1.0.380", "matchzy": "0.9.3"},
  "latest": null,
  "load": {"metamod": true, "cssharp": true, "matchzy": true, "error": null},
  "demo_upload_block_present": true,
  "restarted": true,
  "needs_reload": false,
  "config_changed": true,
  "config_report": {
    "cvars_changed": [{"name": "matchzy_kick_when_no_match_loaded", "before": "false", "after": "true"}],
    "cvars_removed": ["matchzy_max_tech_pauses_allowed"],
    "cvars_added": ["matchzy_max_tech_pauses"],
    "files_changed": ["config.cfg", "live.cfg"]
  },
  "error": null,
  "log_tail": "…",
  "pid": 12345
}
```

- `status`: `succeeded` | `failed` | `rolled_back` (`running` only in
  `update_status` and `update_progress`).
- `phase`: `init`, `check`, `lgsm-update`, `snapshot`, `stop`,
  `game-update`, `mods-update`, `reconcile-config`, `gameinfo`, `start`,
  `restart`, `verify`, `rollback`, `record`, `done`; plus
  `starting` (agent-side, before the helper has written anything). `done`
  only on success; otherwise it is where the job stopped, and
  `failed_phase` is the phase that broke (for `rolled_back`: the phase
  that triggered the rollback).
- `trigger`: `portal` (`update_start`, `update_check`), `overrides`
  (`set_matchzy_overrides`), `auto_repair` (the agent found demo upload
  unwired or the overrides not applied), `local` (cron or SSH). **The portal therefore receives results
  for job ids it did not create** — `local`, `auto_repair`, `overrides` —
  and must treat them as information, not errors. Delivery is at-least-once:
  key on `job_id`.
- `before` / `after`: installed versions; each field `null` when unknown.
  `metamod` is the release archive name and is only known after a `mods` or
  `all` run through the helper.
- `latest`: only set by `check`.
- `load`: what the console showed after the last (re)start in this job;
  `error` is the `MMS: Fatal error` line if there was one. `null` when the
  job never restarted the server.
- `demo_upload_block_present`: the block in `config.cfg` is identical to
  the enrollment snippet at the end of this job (`demo_upload_wired` in
  the heartbeat additionally requires that nothing after it overrides it).
- `needs_reload`: `true` on every successful `repair-config` — the file is
  right but the running server has not re-read it. Informational: the
  agent acts on it itself by running `exec MatchZy/config.cfg` over its
  local RCON once no match is loaded (`reload_pending` in the heartbeat
  shows it waiting). `false` for every other operation (`mods` and `all`
  restart the server, which reads the file).
- `state_publish_failed`: `true` if the helper could not write `state.json`
  during the job (run directory full). The job ran regardless; the result
  then reaches the portal from the helper's own record.
- `config_report`: what the update did to operator settings, with the
  managed blocks excluded. A value is `"[redacted]"` when the setting's
  name (any case) or the value itself mentions `matchzy_demo_upload_`,
  `matchzy_remote_`, `rcon_password`, `sv_password`, `tv_password` or
  `Bearer `. `null` for jobs that do not touch
  the config (`lgsm`, `game`, `check`) and for `rolled_back` (the old files
  are back); for `repair-config` only `files_changed` can be non-empty.
- `error`: one line; for `rolled_back` it is the reason for the rollback.
- `agent_synthesized: true` (only then present): the result was made up by
  the agent because the helper never produced one — its unit ended without
  writing any state, or the helper disappeared mid-job (systemd says the
  unit is no longer active and its process is gone). Reported once, also
  across agent restarts. If the helper turns out to have finished after
  all, its real result follows under the same `job_id` and supersedes it.
- A job interrupted by SIGTERM reports `failed` with an `error` saying what
  the helper put back (snapshot restored or not, server started or not).

A failed `check` is also reported as an `update_result` (so the error is
visible); a successful one only through `update_state`.

**`update_state`** — after every successful `check`, and once on every
(re)connect when the agent has the capability:

```json
{
  "type": "update_state",
  "check": {
    "checked_at": "2026-11-21T04:00:07Z", "job_id": "…",
    "installed": {"lgsm": "v26.6.0", "cs2_build": "20012345", "metamod": null, "cssharp": "1.0.370", "matchzy": "0.9.2"},
    "latest":    {"lgsm": "v26.7.0", "cs2_build": "20012399", "metamod": "mmsource-2.0.0-git1491-linux.tar.gz", "cssharp": "1.0.380", "matchzy": "0.9.3"},
    "update_available": {"lgsm": true, "game": true, "metamod": null, "cssharp": true, "matchzy": true, "mods": true},
    "load": {"metamod": true, "cssharp": true, "matchzy": true, "error": null},
    "demo_upload_block_present": true
  },
  "installed": {"lgsm": "…", "cs2_build": "…", "metamod": "…", "cssharp": "…", "matchzy": "…", "recorded_at": "…", "job_id": "…"}
}
```

`check` and `installed` are `null` until a first check / a first successful
`mods` run. Any `latest` field is `null` when its upstream lookup failed
(that does not fail the check); the matching `update_available` entry is
then `null` too. `update_available.mods` is `true` if any plugin differs,
`null` if none could be compared. Upstream lookups are cached on the host
for 10 minutes.

**Delivery.** A frame written to the socket is not a result the portal has
stored, so results are delivered at least once, keyed on `job_id`:

- A result is sent when the job ends, and again **on every (re)connect**
  until the portal acknowledges it with `update_ack`. Acknowledgements
  survive agent restarts.
- What is replayed is bounded: the last finished job (from the helper's
  `last-job.json`) and the latest result only the agent has (a synthesized
  failure or a failed check). Never a backlog — an older unacknowledged
  result is dropped when a newer one of the same kind exists.
- Within one connection a result is sent once.
- A portal that never sends `update_ack` (one that predates it) receives
  the last result again on each reconnect and must tolerate that.

On every connect the agent sends, in order: `update_state` (when it has the
capability), the unacknowledged results, then live frames. Progress frames
from before the connection are not replayed.
