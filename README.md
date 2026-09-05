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

## Protocol

Portal → agent: `{id, cmd: load_match|end_match|exec|status|load_backup|roster_edit, args}`.
Agent → portal: `{id, ok, output|error}` plus
`{type: "heartbeat", agent_version, rcon_ok, get5_status, status_output}` every 30s,
where `status_output` (0.2.0+) is the raw output of CS2's `status`, bounded to 8 KiB —
the portal parses it for the current map and the connected players.

`exec` runs one console command per frame. The agent refuses, with
`{ok: false, error: "refused: …"}`, anything containing `;` or control
characters, and any command whose verb is portal-owned (`rcon_password`,
`sv_password`, `tv_password`, `sv_rcon_*`, `matchzy_remote_*`,
`matchzy_demo_upload_*`, `matchzy_loadmatch*`, `matchzy_loadbackup_url`,
`logaddress_*`, `sv_downloadurl`, `host_writeconfig`), could hide another
command (`alias`, `exec`), or would end the process (`quit`, `exit`,
`_restart`, `killserver`, `crash`). The portal applies the same rule first;
this copy is defence in depth.
