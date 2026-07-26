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

## Dev mode

Against a local portal (`PORTAL_AGENT_INSECURE=true` on the API):

```bash
portal-server-agent \
    --url ws://localhost:3000/v1/gameserver/agent/ws \
    --dev-server-id <server-uuid> \
    --rcon-addr 127.0.0.1:27015 --rcon-password dev
```

## Protocol

Portal → agent: `{id, cmd: load_match|end_match|exec|status, args}`.
Agent → portal: `{id, ok, output|error}` plus
`{type: "heartbeat", agent_version, rcon_ok, get5_status}` every 30s.
