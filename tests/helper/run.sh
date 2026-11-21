#!/usr/bin/env bash
# Tests for dist/portal-gs-update against a fake game tree and a fake
# LinuxGSM script (fake-lgsm.sh). Plain bash, no framework, no root, no
# network: `tests/helper/run.sh` from anywhere. Needs jq, flock, tar and
# python3 (zstd if present; the helper falls back to gzip without it).
#
#   tests/helper/run.sh            run everything
#   tests/helper/run.sh mods       run the tests whose name contains "mods"
#   KEEP=1 tests/helper/run.sh     keep the temp trees for inspection

set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
HELPER=${HELPER:-$here/../../dist/portal-gs-update}
FILTER=${1:-}
TOKEN='tok_SUPERSECRET_demo_token_0123456789'
BASE=$(mktemp -d "${TMPDIR:-/tmp}/gs-update-test.XXXXXX")
[ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$BASE"' EXIT

DEMO_BEGIN='// BEGIN ANSIBLE MANAGED — portal demo upload'
DEMO_END='// END ANSIBLE MANAGED — portal demo upload'
OVR_BEGIN='// BEGIN PORTAL MANAGED — matchzy overrides'
OVR_END='// END PORTAL MANAGED — matchzy overrides'

pass=0 fail=0 current='' current_failed=0
T='' GS='' CSGO='' FAKE='' RUN='' STATE='' CFG='' OUT='' RC=0

# --- assertions --------------------------------------------------------------

bad() {
    current_failed=1
    echo "    FAIL: $*"
}
eq() { # eq <what> <expected> <actual>
    [ "$2" = "$3" ] || bad "$1: expected '$2', got '$3'"
}
st() { jq -r "$@" "$RUN/state.json"; }
st_eq() { eq "state $1" "$2" "$(st "$1")"; }
has() { # has <file> <fixed string>
    grep -qF -- "$2" "$1" || bad "$1 should contain: $2"
}
hasnt() {
    if grep -qF -- "$2" "$1"; then bad "$1 should not contain: $2"; fi
}
calls() { tr '\n' ',' <"$FAKE/calls"; }

# No secret may reach anything the helper publishes or prints.
no_secrets() {
    local f
    for f in "$RUN"/state.json "$STATE"/*.json "$OUT"; do
        [ -f "$f" ] || continue
        hasnt "$f" "$TOKEN"
        hasnt "$f" 'Bearer '
        hasnt "$f" 'lgsm-rcon-secret'
        hasnt "$f" 'console-rcon-secret'
        hasnt "$f" 'OLDSECRET'
    done
}

# --- fixture -----------------------------------------------------------------

snippet_text() {
    printf '%s\n' \
        '// Written by portal-server-agent enroll. Server-scoped demo' \
        '// upload settings for csgo/cfg/MatchZy/config.cfg — do NOT' \
        '// put these in per-match cvars.' \
        'matchzy_demo_upload_url "https://portal.example/v1/demos"' \
        'matchzy_demo_upload_header_key "Authorization"' \
        "matchzy_demo_upload_header_value \"Bearer $TOKEN\""
}

setup() {
    T=$(mktemp -d "$BASE/t.XXXXXX")
    GS="$T/gs" CSGO="$T/gs/serverfiles/game/csgo" FAKE="$T/gs/fake"
    RUN="$T/run" STATE="$T/state" CFG="$CSGO/cfg/MatchZy/config.cfg" OUT="$T/out.txt"
    mkdir -p "$CSGO/addons/metamod" "$CSGO/addons/counterstrikesharp" "$CSGO/addons/matchzy" \
        "$CSGO/cfg/MatchZy" "$GS/serverfiles/steamapps" "$GS/log/console" "$FAKE" "$T/etc" "$RUN" "$STATE" \
        "$T/api/repos/GameServerManagers/LinuxGSM/releases" \
        "$T/api/repos/roflmuffin/CounterStrikeSharp/releases" \
        "$T/api/repos/shobhit-pathak/MatchZy/releases"
    cp "$here/fake-lgsm.sh" "$GS/cs2server"
    chmod +x "$GS/cs2server"

    printf '"AppState"\n{\n\t"appid"\t\t"730"\n\t"buildid"\t\t"1000"\n}\n' >"$GS/serverfiles/steamapps/appmanifest_730.acf"
    echo 1000 >"$FAKE/remote_build"
    # gameinfo.gi has CRLF line endings, like the real one.
    printf '"GameInfo"\r\n{\r\n\tFileSystem\r\n\t{\r\n\t\tSearchPaths\r\n\t\t{\r\n\t\t\tGame_LowViolence\tcsgo_lv\r\n\t\t\tGame\tcsgo/addons/metamod\r\n\t\t\tGame\tcsgo\r\n\t\t\tGame\tcsgo_imported\r\n\t\t}\r\n\t}\r\n}\r\n' >"$CSGO/gameinfo.gi"

    echo 1.0.370 >"$CSGO/addons/counterstrikesharp/VERSION"
    echo 0.9.2 >"$CSGO/addons/matchzy/VERSION"
    echo 'addons/counterstrikesharp' >"$CSGO/addons/metamod/metaplugins.ini"

    # The operator's config: upstream 0.9.2 defaults, three local changes,
    # then the managed block exactly as Ansible wrote it.
    {
        printf '%s\n' \
            '// MatchZy config' \
            'matchzy_kick_when_no_match_loaded false' \
            'matchzy_max_tech_pauses_allowed "3"   // operator bumped this' \
            'matchzy_tech_pause_duration 300' \
            'matchzy_demo_upload_url ""' \
            'matchzy_remote_log_header_value "Bearer OLDSECRET"' \
            'matchzy_minimum_ready_required 2'
        printf '%s\n' "$DEMO_BEGIN"
        snippet_text
        printf '%s\n' "$DEMO_END"
    } >"$CFG"
    chmod 0640 "$CFG"
    echo 'mp_overtime_enable 1' >"$CSGO/cfg/MatchZy/live.cfg"
    echo 'mp_warmuptime 60' >"$CSGO/cfg/MatchZy/warmup.cfg"
    echo '{"76561198000000000": ""}' >"$CSGO/cfg/MatchZy/admins.json"

    # What the next mods-update ships: MatchZy 0.9.3 with renamed cvars and a
    # pristine config (no block, operator changes gone).
    mkdir -p "$FAKE/mods_next/addons/matchzy" "$FAKE/mods_next/addons/counterstrikesharp" "$FAKE/mods_next/cfg/MatchZy"
    echo 0.9.3 >"$FAKE/mods_next/addons/matchzy/VERSION"
    echo 1.0.380 >"$FAKE/mods_next/addons/counterstrikesharp/VERSION"
    echo new >"$FAKE/mods_next/addons/matchzy/NewFile.dll"
    printf '%s\n' \
        '// MatchZy config' \
        'matchzy_kick_when_no_match_loaded true' \
        'matchzy_max_tech_pauses "2"' \
        'matchzy_tech_pause_time 300' \
        'matchzy_demo_upload_url ""' \
        'matchzy_remote_log_header_value ""' \
        'matchzy_minimum_ready_required 2' >"$FAKE/mods_next/cfg/MatchZy/config.cfg"
    echo 'mp_overtime_enable 0' >"$FAKE/mods_next/cfg/MatchZy/live.cfg"
    echo 'mp_warmuptime 60' >"$FAKE/mods_next/cfg/MatchZy/warmup.cfg"

    # The server is up, plugins loaded, from an earlier start.
    printf '%s\n' 'Loading map' 'CounterStrikeSharp API v1.0.370' \
        'CounterStrikeSharp.API Loaded Successfully.' '[MatchZy 0.9.2 LOADED]' >"$GS/log/console/cs2server-console.log"

    snippet_text >"$T/etc/matchzy_portal.cfg"
    cat >"$T/etc/gs-update.conf" <<EOF
# test config
LGSM_SCRIPT=$GS/cs2server
LGSM_USER="$(id -un)"
GAMEINFO_PATH=$CSGO/gameinfo.gi
CONSOLE_LOG=$GS/log/console/cs2server-console.log
MATCHZY_CONFIG_PATH='$CFG'
MATCHZY_CONFIG_OWNER=$(id -un)
VERIFY_TIMEOUT=2
SNAPSHOT_KEEP=2
EOF

    echo '{"tag_name": "v26.7.0"}' >"$T/api/repos/GameServerManagers/LinuxGSM/releases/latest"
    echo '{"tag_name": "v1.0.380"}' >"$T/api/repos/roflmuffin/CounterStrikeSharp/releases/latest"
    echo '{"tag_name": "0.9.3"}' >"$T/api/repos/shobhit-pathak/MatchZy/releases/latest"
    echo 'mmsource-2.0.0-git1491-linux.tar.gz' >"$T/mms-latest"
}

helper() { # helper <op> — sets RC, output in $OUT
    env -i PATH="$PATH" HOME="$T" \
        GS_UPDATE_CONF="$T/etc/gs-update.conf" \
        GS_UPDATE_RUN_DIR="$RUN" \
        GS_UPDATE_STATE_DIR="$STATE" \
        GS_UPDATE_SNIPPET="$T/etc/matchzy_portal.cfg" \
        GS_UPDATE_OVERRIDES="$T/etc/matchzy_overrides.cfg" \
        GS_UPDATE_LGSM_DIRECT=1 \
        GS_UPDATE_NO_SYSLOG=1 \
        GS_UPDATE_GITHUB_API="file://$T/api" \
        GS_UPDATE_MMS_URL="file://$T/mms-latest" \
        GS_UPDATE_VERIFY_POLL=0.2 \
        GS_UPDATE_PUMP_INTERVAL=0.05 \
        ${EXTRA_ENV:+$EXTRA_ENV} \
        "$HELPER" "$@" >"$OUT" 2>&1
    RC=$?
}

# What Ansible's blockinfile would do to the config with the task in
# roles/gameserver_agent: re-implemented here so "a later converge is a
# no-op" is checked against the module's rules, not against the helper's.
blockinfile_converge() { # blockinfile_converge <config> <snippet> → converged file on stdout
    python3 - "$1" "$2" <<'PY'
import sys
path, snippet = sys.argv[1], sys.argv[2]
marker = "// {mark} ANSIBLE MANAGED — portal demo upload"
m0, m1 = marker.replace("{mark}", "BEGIN"), marker.replace("{mark}", "END")
lines = open(path, encoding="utf-8").read().splitlines()
block = [m0] + open(snippet, encoding="utf-8").read().splitlines() + [m1]
n0 = n1 = None
for i, line in enumerate(lines):
    if line == m0: n0 = i
    if line == m1: n1 = i
if n0 is not None and n1 is not None:
    lo, hi = sorted((n0, n1))
    lines[lo:hi + 1] = block
else:
    lines += block
sys.stdout.write("\n".join(lines) + "\n")
PY
}

# --- tests -------------------------------------------------------------------

test_missing_conf_exits_78() {
    rm "$T/etc/gs-update.conf"
    helper check
    eq "exit status" 78 "$RC"
    has "$OUT" 'not configured'
    [ ! -e "$RUN/state.json" ] || bad "no state should be written"
}

test_bad_operation_exits_64() {
    helper 'mods; rm -rf /'
    eq "exit status" 64 "$RC"
    helper
    eq "exit status (no argument)" 64 "$RC"
}

test_check_reports_installed_and_latest() {
    echo 2000 >"$FAKE/remote_build"
    helper check
    eq "exit status" 0 "$RC"
    st_eq .operation check
    st_eq .status succeeded
    st_eq .phase done
    st_eq .before.lgsm v26.6.0
    st_eq .before.cs2_build 1000
    st_eq .before.cssharp 1.0.370
    st_eq .before.matchzy 0.9.2
    st_eq .before.metamod null
    st_eq .latest.lgsm v26.7.0
    st_eq .latest.cs2_build 2000
    st_eq .latest.metamod mmsource-2.0.0-git1491-linux.tar.gz
    st_eq .latest.cssharp 1.0.380
    st_eq .latest.matchzy 0.9.3
    st_eq .load.cssharp true
    st_eq .load.matchzy true
    st_eq .load.error null
    st_eq .demo_upload_block_present true
    st_eq .restarted false
    [[ $(st .job_id) =~ ^[0-9a-f-]{36}$ ]] || bad "job_id should be a generated uuid"
    eq "check.json game" true "$(jq -r .update_available.game "$STATE/check.json")"
    eq "check.json lgsm" true "$(jq -r .update_available.lgsm "$STATE/check.json")"
    eq "check.json mods" true "$(jq -r .update_available.mods "$STATE/check.json")"
    eq "check.json installed" 0.9.2 "$(jq -r .installed.matchzy "$STATE/check.json")"
    eq "state.json mode" 644 "$(stat -c %a "$RUN/state.json")"
    eq "calls" 'check-update,' "$(calls)"
    [ ! -e "$STATE/last-job.json" ] || bad "a check must not become the last job"
    eq "game tree untouched" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
    no_secrets
}

test_check_survives_network_failure_and_caches() {
    helper check
    st_eq .latest.matchzy 0.9.3
    # Upstream goes away: the 10-minute cache still answers.
    rm -rf "$T/api" "$T/mms-latest"
    helper check
    eq "exit status" 0 "$RC"
    st_eq .latest.matchzy 0.9.3
    st_eq .latest.metamod mmsource-2.0.0-git1491-linux.tar.gz
    # Cache expired and still no network: unknown, not a failed job.
    EXTRA_ENV=GS_UPDATE_CACHE_TTL=0 helper check
    eq "exit status" 0 "$RC"
    st_eq .status succeeded
    st_eq .latest.lgsm null
    st_eq .latest.matchzy null
    st_eq .latest.metamod null
    st_eq .latest.cs2_build 1000
    eq "check.json mods unknown" null "$(jq -r .update_available.mods "$STATE/check.json")"
    eq "check.json game" false "$(jq -r .update_available.game "$STATE/check.json")"
}

test_mods_update_reasserts_block_and_reports_config() {
    printf '%s\n' \
        'matchzy_minimum_ready_required 5' \
        'matchzy_demo_upload_url https://evil.example/steal' \
        'matchzy_remote_log_url https://evil.example/log' \
        'matchzy_loadmatch_url https://evil.example/m' \
        'matchzy_chat_prefix a;quit' \
        'matchzy_knife_enabled_default "false"' \
        'rcon_password hunter2' \
        'matchzy_whitelist_enabled_default false' >"$T/etc/matchzy_overrides.cfg"
    helper mods
    eq "exit status" 0 "$RC"
    st_eq .operation mods
    st_eq .status succeeded
    st_eq .phase done
    st_eq .restarted true
    st_eq .demo_upload_block_present true
    st_eq .load.matchzy true
    st_eq .before.matchzy 0.9.2
    st_eq .after.matchzy 0.9.3
    st_eq .after.cssharp 1.0.380
    eq "calls" 'stop,mods-update,start,' "$(calls)"
    eq "new plugin files landed" 0.9.3 "$(cat "$CSGO/addons/matchzy/VERSION")"

    # The whole file: the new upstream config, then the two blocks, byte for byte.
    {
        cat "$FAKE/mods_next/cfg/MatchZy/config.cfg"
        printf '%s\n' "$DEMO_BEGIN"
        cat "$T/etc/matchzy_portal.cfg"
        printf '%s\n' "$DEMO_END" "$OVR_BEGIN" \
            'matchzy_minimum_ready_required "5"' \
            'matchzy_whitelist_enabled_default "false"' "$OVR_END"
    } >"$T/expected.cfg"
    cmp -s "$T/expected.cfg" "$CFG" || bad "config.cfg is not the upstream file plus the two managed blocks: $(diff "$T/expected.cfg" "$CFG" | head -5)"
    blockinfile_converge "$CFG" "$T/etc/matchzy_portal.cfg" >"$T/converged.cfg"
    cmp -s "$T/converged.cfg" "$CFG" || bad "an Ansible blockinfile converge would change the file"
    eq "config mode" 640 "$(stat -c %a "$CFG")"

    eq "kick reverted" '{"name":"matchzy_kick_when_no_match_loaded","before":"false","after":"true"}' \
        "$(st -c '.config_report.cvars_changed[] | select(.name == "matchzy_kick_when_no_match_loaded")')"
    eq "secret cvar redacted" '{"name":"matchzy_remote_log_header_value","before":"[redacted]","after":"[redacted]"}' \
        "$(st -c '.config_report.cvars_changed[] | select(.name == "matchzy_remote_log_header_value")')"
    eq "cvars_changed count" 2 "$(st '.config_report.cvars_changed | length')"
    eq "cvars_removed" 'matchzy_max_tech_pauses_allowed,matchzy_tech_pause_duration' "$(st '.config_report.cvars_removed | join(",")')"
    eq "cvars_added" 'matchzy_max_tech_pauses,matchzy_tech_pause_time' "$(st '.config_report.cvars_added | join(",")')"
    eq "files_changed" 'config.cfg,live.cfg' "$(st '.config_report.files_changed | join(",")')"

    eq "installed.json matchzy" 0.9.3 "$(jq -r .matchzy "$STATE/installed.json")"
    eq "installed.json metamod" mmsource-2.0.0-git1491-linux.tar.gz "$(jq -r .metamod "$STATE/installed.json")"
    eq "last-job.json" "$(st .job_id)" "$(jq -r .job_id "$STATE/last-job.json")"
    eq "snapshots kept" 1 "$(find "$STATE/snapshots" -type f | wc -l)"
    eq "snapshot mode" 600 "$(stat -c %a "$(find "$STATE/snapshots" -type f | head -n 1)")"
    has "$OUT" 'ignoring override for portal-owned cvar matchzy_demo_upload_url'
    has "$OUT" '[redacted line]'
    hasnt "$CFG" 'evil.example'
    hasnt "$CFG" 'hunter2'
    hasnt "$CFG" 'quit'
    no_secrets

    # Snapshots are pruned to SNAPSHOT_KEEP (2 in the test config).
    helper mods
    helper mods
    eq "snapshots after three runs" 2 "$(find "$STATE/snapshots" -type f | wc -l)"
}

test_mods_update_that_fails_to_load_rolls_back() {
    touch "$FAKE/mods_next/addons/BROKEN"
    cp -p "$CFG" "$T/config.before"
    helper mods
    eq "exit status" 3 "$RC"
    st_eq .status rolled_back
    st_eq .phase rollback
    st_eq .failed_phase verify
    st_eq .restarted true
    st_eq .load.matchzy true
    st_eq .load.error null
    st_eq .demo_upload_block_present true
    [[ $(st .error) == *'MMS: Fatal error: plugin ABI mismatch'* ]] || bad "error should carry the failing console line, got: $(st .error)"
    eq "calls" 'stop,mods-update,start,stop,start,' "$(calls)"
    eq "previous plugin files restored" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
    [ ! -e "$CSGO/addons/BROKEN" ] || bad "files added by the failed update should be gone"
    [ ! -e "$CSGO/addons/matchzy/NewFile.dll" ] || bad "files added by the failed update should be gone"
    cmp -s "$T/config.before" "$CFG" || bad "config.cfg should be back to the pre-update file"
    eq "live.cfg restored" 'mp_overtime_enable 1' "$(cat "$CSGO/cfg/MatchZy/live.cfg")"
    st_eq .after.matchzy 0.9.2
    st_eq .config_report null
    st_eq .config_changed false
    [ ! -e "$STATE/installed.json" ] || bad "a rolled-back run must not record installed.json"
    eq "last-job.json status" rolled_back "$(jq -r .status "$STATE/last-job.json")"
    no_secrets
}

test_mods_verify_ignores_stale_console_lines() {
    # The new set loads nothing and prints nothing. The console log still
    # has the LOADED lines from before the update: they must not count.
    touch "$FAKE/mods_next/addons/SILENT"
    helper mods
    eq "exit status" 3 "$RC"
    st_eq .status rolled_back
    [[ $(st .error) == *'did not load within 2s'*'CounterStrikeSharp, MatchZy'* ]] || bad "unexpected error: $(st .error)"
    eq "previous plugin files restored" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
    # Same again with LinuxGSM rotating the console log at every start.
    : >"$FAKE/calls"
    touch "$FAKE/rotate"
    helper mods
    eq "exit status (rotating log)" 3 "$RC"
    st_eq .status rolled_back
    st_eq .load.matchzy true
}

test_mods_update_command_failure_rolls_back() {
    echo 1 >"$FAKE/mods_rc"
    helper mods
    eq "exit status" 3 "$RC"
    st_eq .status rolled_back
    st_eq .failed_phase mods-update
    eq "previous plugin files restored" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
}

test_game_update_repairs_crlf_gameinfo() {
    echo 2000 >"$FAKE/remote_build"
    touch "$FAKE/strip_gameinfo"
    cp "$CSGO/gameinfo.gi" "$T/gameinfo.before"
    helper game
    eq "exit status" 0 "$RC"
    st_eq .status succeeded
    st_eq .restarted true
    st_eq .before.cs2_build 1000
    st_eq .after.cs2_build 2000
    st_eq .load.matchzy true
    eq "calls" 'update,restart,' "$(calls)"
    cmp -s "$T/gameinfo.before" "$CSGO/gameinfo.gi" || bad "gameinfo.gi should be byte-identical to the patched original (CRLF, same indent)"
    grep -q $'^\t\t\tGame\tcsgo/addons/metamod\r$' "$CSGO/gameinfo.gi" || bad "Metamod line missing or not CRLF"
    eq "non-CRLF lines" 0 "$(grep -cv $'\r$' "$CSGO/gameinfo.gi")"
    eq "backups" 1 "$(find "$CSGO" -maxdepth 1 -name 'gameinfo.gi.bak-*' | wc -l)"
    no_secrets
}

test_game_update_without_change_does_not_restart() {
    helper game
    eq "exit status" 0 "$RC"
    st_eq .status succeeded
    st_eq .restarted false
    st_eq .after.cs2_build 1000
    eq "calls" 'update,' "$(calls)"
    eq "backups" 0 "$(find "$CSGO" -maxdepth 1 -name 'gameinfo.gi.bak-*' | wc -l)"
    no_secrets
}

test_game_force_update() {
    EXTRA_ENV=GS_UPDATE_FORCE=1 helper game
    eq "calls" 'force-update,' "$(calls)"
}

test_lgsm_update() {
    echo v26.7.0 >"$FAKE/next_lgsm"
    helper lgsm
    eq "exit status" 0 "$RC"
    st_eq .before.lgsm v26.6.0
    st_eq .after.lgsm v26.7.0
    st_eq .restarted false
    eq "calls" 'update-lgsm,' "$(calls)"
}

test_all_updates_everything_with_one_restart() {
    echo v26.7.0 >"$FAKE/next_lgsm"
    echo 2000 >"$FAKE/remote_build"
    touch "$FAKE/strip_gameinfo"
    helper all
    eq "exit status" 0 "$RC"
    st_eq .status succeeded
    st_eq .after.lgsm v26.7.0
    st_eq .after.cs2_build 2000
    st_eq .after.matchzy 0.9.3
    st_eq .restarted true
    st_eq .demo_upload_block_present true
    eq "calls" 'update-lgsm,stop,update,mods-update,start,' "$(calls)"
    grep -q 'csgo/addons/metamod' "$CSGO/gameinfo.gi" || bad "gameinfo.gi not repaired"
    no_secrets
}

test_repair_config_is_idempotent() {
    # Someone ran mods-update over SSH: the block is gone.
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG"
    chmod 0644 "$CFG"
    helper repair-config
    eq "exit status" 0 "$RC"
    st_eq .status succeeded
    st_eq .config_changed true
    st_eq .demo_upload_block_present true
    st_eq .restarted false
    st_eq .needs_reload false
    eq "files_changed" config.cfg "$(st '.config_report.files_changed | join(",")')"
    eq "calls" 'send exec MatchZy/config.cfg,' "$(calls)"
    eq "reload sent" 'exec MatchZy/config.cfg' "$(cat "$FAKE/sent")"
    eq "config mode" 640 "$(stat -c %a "$CFG")"
    blockinfile_converge "$CFG" "$T/etc/matchzy_portal.cfg" >"$T/converged.cfg"
    cmp -s "$T/converged.cfg" "$CFG" || bad "an Ansible blockinfile converge would change the file"

    cp -p "$CFG" "$T/config.first"
    local inode mtime
    inode=$(stat -c %i "$CFG") mtime=$(stat -c %Y.%y "$CFG")
    sleep 0.05
    helper repair-config
    eq "exit status (second run)" 0 "$RC"
    st_eq .config_changed false
    eq "files_changed (second run)" '' "$(st '.config_report.files_changed | join(",")')"
    cmp -s "$T/config.first" "$CFG" || bad "second run changed the file"
    eq "file not rewritten" "$inode $mtime" "$(stat -c %i "$CFG") $(stat -c %Y.%y "$CFG")"
    no_secrets
}

test_repair_config_overrides_block_follows_file() {
    echo 'matchzy_minimum_ready_required 5' >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    st_eq .config_changed true
    eq "last lines" "$OVR_BEGIN|matchzy_minimum_ready_required \"5\"|$OVR_END|" "$(tail -n 3 "$CFG" | tr '\n' '|')"
    eq "demo block before overrides" "$DEMO_END" "$(tail -n 4 "$CFG" | head -n 1)"
    # Overrides removed in the portal: the block goes, the demo block stays.
    : >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    st_eq .config_changed true
    hasnt "$CFG" "$OVR_BEGIN"
    eq "last line" "$DEMO_END" "$(tail -n 1 "$CFG")"
    st_eq .demo_upload_block_present true
}

test_repair_config_moves_a_stale_block_and_keeps_operator_lines() {
    # Stale token in the block, an operator line after it, no final newline.
    sed -i "s/$TOKEN/tok_STALE/" "$CFG"
    printf 'matchzy_autostart_mode 1' >>"$CFG"
    helper repair-config
    st_eq .config_changed true
    st_eq .demo_upload_block_present true
    hasnt "$CFG" tok_STALE
    has "$CFG" 'matchzy_autostart_mode 1'
    eq "one block" 1 "$(grep -cF "$DEMO_BEGIN" "$CFG")"
    eq "block is last" "$DEMO_END" "$(tail -n 1 "$CFG")"
    hasnt "$RUN/state.json" tok_STALE
}

test_repair_config_flags_reload_when_send_fails() {
    echo 1 >"$FAKE/send_rc"
    rm "$T/etc/matchzy_portal.cfg"
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG"
    helper repair-config
    eq "exit status" 0 "$RC"
    st_eq .needs_reload true
    # No enrollment snippet: nothing to assert, nothing invented.
    st_eq .demo_upload_block_present false
    st_eq .config_changed false
}

test_lock_contention_fails_fast() {
    helper check
    cp "$RUN/state.json" "$T/state.before"
    flock "$RUN/lock" sleep 3 &
    local holder=$!
    sleep 0.3
    helper mods
    eq "exit status" 75 "$RC"
    has "$OUT" 'lock held'
    cmp -s "$T/state.before" "$RUN/state.json" || bad "a refused run must not touch state.json"
    eq "calls" 'check-update,' "$(calls)"
    kill "$holder" 2>/dev/null
    wait "$holder" 2>/dev/null
}

test_lock_refuses_to_follow_a_planted_link() {
    echo precious >"$T/victim"
    ln -s "$T/victim" "$RUN/lock"
    helper check
    eq "exit status" 0 "$RC"
    eq "link target untouched" precious "$(cat "$T/victim")"
    [ -f "$RUN/lock" ] && [ ! -L "$RUN/lock" ] || bad "lock should now be a regular file"
}

test_request_file_supplies_the_job_id() {
    local id=0b0e3c5e-7f0a-4c1d-9b7e-1a2b3c4d5e6f
    echo "{\"job_id\": \"$id\", \"operation\": \"repair-config\", \"trigger\": \"auto_repair\"}" >"$RUN/request.json"
    helper repair-config
    eq "exit status" 0 "$RC"
    st_eq .job_id "$id"
    st_eq .trigger auto_repair
    [ ! -e "$RUN/request.json" ] || bad "request.json should be consumed"

    # A request for another operation is not ours: left alone, id generated.
    echo "{\"job_id\": \"$id\", \"operation\": \"mods\"}" >"$RUN/request.json"
    helper check
    eq "exit status" 0 "$RC"
    [ "$(st .job_id)" != "$id" ] || bad "check must not take a mods request's job id"
    st_eq .trigger local
    [ -e "$RUN/request.json" ] || bad "another operation's request should be left in place"

    # A job id that is not a uuid never reaches the state file.
    echo '{"job_id": "$(touch /tmp/pwned)", "operation": "check"}' >"$RUN/request.json"
    cp "$RUN/state.json" "$T/state.before"
    helper check
    eq "exit status" 65 "$RC"
    cmp -s "$T/state.before" "$RUN/state.json" || bad "a rejected request must not touch state.json"
    [ ! -e "$RUN/request.json" ] || bad "a rejected request should be removed"

    echo 'not json' >"$RUN/request.json"
    helper check
    eq "exit status (garbage)" 65 "$RC"

    # Stale request (helper never ran for it): discarded.
    echo "{\"job_id\": \"$id\", \"operation\": \"check\"}" >"$RUN/request.json"
    touch -d '10 minutes ago' "$RUN/request.json"
    helper check
    eq "exit status (stale)" 0 "$RC"
    [ "$(st .job_id)" != "$id" ] || bad "a stale request must not be honoured"
}

test_log_tail_is_bounded() {
    # A chatty LinuxGSM: far more output than the tail may carry.
    sed -i 's/^    check-update)$/    check-update)\n        for i in $(seq 1 400); do echo "steamcmd progress line $i of many many many"; done/' "$GS/cs2server"
    helper check
    eq "exit status" 0 "$RC"
    local n
    n=$(st '.log_tail | length')
    [ "$n" -le 4096 ] || bad "log_tail is $n bytes"
    [ "$n" -gt 1000 ] || bad "log_tail suspiciously short ($n)"
    has "$OUT" 'steamcmd progress line 1 of many'
}

# --- runner ------------------------------------------------------------------

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    case $t in *"$FILTER"*) ;; *) continue ;; esac
    current=$t current_failed=0
    setup
    "$t"
    if [ "$current_failed" = 0 ]; then
        pass=$((pass + 1))
        echo "ok   $current"
    else
        fail=$((fail + 1))
        echo "FAIL $current"
        echo "    --- helper output (last run) ---"
        sed 's/^/    /' "$OUT" 2>/dev/null | tail -n 40
    fi
done

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ] && [ "$pass" -gt 0 ]
