#!/usr/bin/env bash
# Tests for dist/gs-update/portal-gs-update against a fake game tree and a fake
# LinuxGSM script (fake-lgsm.sh). Plain bash, no framework, no root, no
# network: `tests/helper/run.sh` from anywhere. Needs jq, flock, tar and
# python3 (zstd if present; the helper falls back to gzip without it).
#
#   tests/helper/run.sh            run everything
#   tests/helper/run.sh mods       run the tests whose name contains "mods"
#   KEEP=1 tests/helper/run.sh     keep the temp trees for inspection

set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
HELPER=${HELPER:-$here/../../dist/gs-update/portal-gs-update}
FILTER=${1:-}
TOKEN='tok_SUPERSECRET_demo_token_0123456789'
BASE=$(mktemp -d "${TMPDIR:-/tmp}/gs-update-test.XXXXXX")
[ "${KEEP:-0}" = 1 ] || trap 'rm -rf "$BASE"' EXIT

DEMO_BEGIN='// BEGIN ANSIBLE MANAGED — portal demo upload'
DEMO_END='// END ANSIBLE MANAGED — portal demo upload'
OVR_BEGIN='// BEGIN PORTAL MANAGED — matchzy overrides'
OVR_END='// END PORTAL MANAGED — matchzy overrides'

pass=0 fail=0 current='' current_failed=0
EXTRA_ENV=''
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
cfg_eq() { eq "config.json $1" "$2" "$(jq -r "$1" "$RUN/config.json" 2>/dev/null)"; }
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
    for f in "$RUN"/state.json "$RUN"/config.json "$STATE"/*.json "$OUT"; do
        [ -f "$f" ] || continue
        hasnt "$f" "$TOKEN"
        hasnt "$f" 'Bearer '
        hasnt "$f" 'lgsm-rcon-secret'
        hasnt "$f" 'console-rcon-secret'
        hasnt "$f" 'OLDSECRET'
        hasnt "$f" 'latin1-token-secret'
        hasnt "$f" 'latin1-rcon-secret'
        hasnt "$f" 'inline-raw-secret'
        hasnt "$f" 'MixedCaseSecret'
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

    echo 370 >"$CSGO/addons/counterstrikesharp/VERSION"
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
    echo 380 >"$FAKE/mods_next/addons/counterstrikesharp/VERSION"
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
    printf 'Loading map\n\033[38;5;10mCSSharp: \033[0mCounterStrikeSharp.API Loaded Successfully.\033[0m\n\033[32m[MatchZy 0.9.2 LOADED] MatchZy by WD- (https://github.com/shobhit-pathak/) [build 0000000, CounterStrikeSharp API 370, Ubuntu 24.04.5 LTS]\033[0m\n' \
        >"$GS/log/console/cs2server-console.log"
    : >"$FAKE/calls"

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
        GS_UPDATE_LOCK_WAIT=1 \
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
    st_eq .phase "done"
    st_eq .before.lgsm v26.6.0
    st_eq .before.cs2_build 1000
    st_eq .before.cssharp 370
    st_eq .before.matchzy 0.9.2
    st_eq .before.metamod null
    st_eq .latest.lgsm v26.7.0
    st_eq .latest.cs2_build 2000
    st_eq .latest.metamod mmsource-2.0.0-git1491-linux.tar.gz
    st_eq .latest.cssharp 380
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
    st_eq .phase "done"
    st_eq .restarted true
    st_eq .demo_upload_block_present true
    st_eq .load.matchzy true
    st_eq .before.matchzy 0.9.2
    st_eq .after.matchzy 0.9.3
    st_eq .after.cssharp 380
    eq "installed.json cssharp" 380 "$(jq -r .cssharp "$STATE/installed.json")"
    cfg_eq .demo_upload_wired true
    # Six of the eight lines were refused: what is in the file is not what
    # was asked for, and the report says so instead of "applied".
    cfg_eq .overrides_applied false
    cfg_eq .reason '6 line(s) of the MatchZy overrides file were rejected by the helper (or the file is unreadable)'
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
    # The agent reloads the config over RCON; the helper only says so.
    st_eq .needs_reload true
    eq "files_changed" config.cfg "$(st '.config_report.files_changed | join(",")')"
    eq "calls (LinuxGSM is not involved)" '' "$(calls)"
    cfg_eq .demo_upload_wired true
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

test_repair_config_without_a_snippet_invents_nothing() {
    rm "$T/etc/matchzy_portal.cfg"
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG"
    helper repair-config
    eq "exit status" 0 "$RC"
    st_eq .demo_upload_block_present false
    st_eq .config_changed false
    cfg_eq .demo_upload_wired null
    cfg_eq .reason 'no enrollment snippet on this host'
}

test_verify_config_reports_wiring_from_the_file() {
    helper verify-config
    eq "exit status" 0 "$RC"
    eq "prints nothing" '' "$(cat "$OUT")"
    cfg_eq .demo_upload_wired true
    cfg_eq .overrides_applied null
    cfg_eq .reason null
    [[ $(jq -r .checked_at "$RUN/config.json") =~ ^20[0-9-]+T[0-9:]+Z$ ]] || bad "checked_at is not a timestamp"
    eq "config.json mode" 644 "$(stat -c %a "$RUN/config.json")"
    eq "keys" 'auto_repair,checked_at,demo_upload_wired,overrides_applied,reason' "$(jq -r 'keys | join(",")' "$RUN/config.json")"
    # Not a job: no state, no record, LinuxGSM never called, nothing left behind.
    [ ! -e "$RUN/state.json" ] || bad "verify-config must not write state.json"
    [ ! -e "$STATE/last-job.json" ] || bad "verify-config must not write last-job.json"
    eq "calls" '' "$(calls)"
    eq "work dirs left" 0 "$(find "$STATE" -maxdepth 1 -name '.work.*' | wc -l)"

    # mods-update by hand: the block is gone.
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG"
    helper verify-config
    cfg_eq .demo_upload_wired false
    [[ $(jq -r .reason "$RUN/config.json") == *'missing from the MatchZy config or differs'* ]] || bad "reason: $(jq -r .reason "$RUN/config.json")"
    # repair-config fixes it and refreshes config.json on its way out.
    helper repair-config
    cfg_eq .demo_upload_wired true
    cfg_eq .reason null

    # Right block, wrong token: differs from the snippet.
    sed -i "s/$TOKEN/tok_STALE/" "$CFG"
    helper verify-config
    cfg_eq .demo_upload_wired false
    helper repair-config

    # Block intact, but a later line switches upload off again.
    echo 'matchzy_demo_upload_url ""' >>"$CFG"
    helper verify-config
    cfg_eq .demo_upload_wired false
    [[ $(jq -r .reason "$RUN/config.json") == *'after the demo-upload block'* ]] || bad "reason: $(jq -r .reason "$RUN/config.json")"
    helper repair-config
    cfg_eq .demo_upload_wired true
    # A later line about something else, or a comment, is the operator's business.
    printf '%s\n' '// matchzy_demo_upload_url ""' 'matchzy_autostart_mode 1' >>"$CFG"
    helper verify-config
    cfg_eq .demo_upload_wired true

    # No config file at all.
    rm "$CFG"
    helper verify-config
    eq "exit status (no config)" 0 "$RC"
    cfg_eq .demo_upload_wired false

    # No snippet: nothing to compare against.
    rm "$T/etc/matchzy_portal.cfg"
    helper verify-config
    cfg_eq .demo_upload_wired null
    cfg_eq .reason 'no enrollment snippet on this host'
    no_secrets
    hasnt "$RUN/config.json" "$TOKEN"
    hasnt "$RUN/config.json" tok_STALE
}

test_verify_config_reports_overrides() {
    helper verify-config
    cfg_eq .overrides_applied null
    echo 'matchzy_minimum_ready_required 5' >"$T/etc/matchzy_overrides.cfg"
    helper verify-config
    cfg_eq .overrides_applied false
    cfg_eq .demo_upload_wired true
    [[ $(jq -r .reason "$RUN/config.json") == *'overrides block'* ]] || bad "reason: $(jq -r .reason "$RUN/config.json")"
    helper repair-config
    cfg_eq .overrides_applied true
    cfg_eq .reason null
    # The portal changed a value: the block is out of date.
    echo 'matchzy_minimum_ready_required 4' >"$T/etc/matchzy_overrides.cfg"
    helper verify-config
    cfg_eq .overrides_applied false
    helper repair-config
    cfg_eq .overrides_applied true
    # A later line sets the same command: the override does not hold.
    echo 'matchzy_minimum_ready_required 1' >>"$CFG"
    helper verify-config
    cfg_eq .overrides_applied false
    helper repair-config
    # A longer name is a different command.
    echo 'matchzy_minimum_ready_required_extra 1' >>"$CFG"
    helper verify-config
    cfg_eq .overrides_applied true
    # Overrides cleared in the portal: applied once the block is gone.
    : >"$T/etc/matchzy_overrides.cfg"
    helper verify-config
    cfg_eq .overrides_applied false
    helper repair-config
    cfg_eq .overrides_applied true
}

test_verify_config_steps_aside_for_a_running_job() {
    helper mods
    cp "$RUN/state.json" "$T/state.before"
    cp "$RUN/config.json" "$T/config.before"
    cp "$STATE/last-job.json" "$T/last.before"
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG" # would flip the answer
    flock "$RUN/lock" sleep 1 &
    local holder=$!
    sleep 0.3
    helper verify-config
    eq "exit status" 75 "$RC"
    eq "prints nothing" '' "$(cat "$OUT")"
    cmp -s "$T/config.before" "$RUN/config.json" || bad "config.json must be untouched while a job runs"
    wait "$holder"
    helper verify-config
    eq "exit status (lock free)" 0 "$RC"
    cfg_eq .demo_upload_wired false
    cmp -s "$T/state.before" "$RUN/state.json" || bad "verify-config must not touch state.json"
    cmp -s "$T/last.before" "$STATE/last-job.json" || bad "verify-config must not touch last-job.json"
}

test_every_job_refreshes_config_json() {
    local op
    for op in check game lgsm; do
        rm -f "$RUN/config.json"
        helper "$op"
        cfg_eq .demo_upload_wired true
    done
    # A rolled-back mods run too: the old files, block included, are back.
    touch "$FAKE/mods_next/addons/BROKEN"
    rm -f "$RUN/config.json"
    helper mods
    st_eq .status rolled_back
    cfg_eq .demo_upload_wired true
}

# What the agent relies on while a job runs: state.json says `running`,
# names a live pid whose command line identifies the helper, and already
# carries the output so far.
test_state_is_published_while_the_job_runs() {
    echo 2 >"$FAKE/slow"
    helper game &
    local bg=$! pid
    for _ in $(seq 1 50); do
        grep -q 'SteamCMD downloading' "$RUN/state.json" 2>/dev/null && break
        sleep 0.1
    done
    st_eq .status running
    st_eq .phase game-update
    st_eq .finished_at null
    has "$RUN/state.json" 'SteamCMD downloading'
    pid=$(st .pid)
    tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q 'portal-gs-update game' \
        || bad "state pid $pid is not the helper: $(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)"
    # A second operation is refused while it runs, and says so.
    local first_out=$OUT
    OUT="$T/out2.txt"
    helper check
    eq "exit status of the second run" 75 "$RC"
    OUT=$first_out
    wait "$bg"
    st_eq .status succeeded
    st_eq .phase "done"
    [ ! -e "/proc/$pid" ] || bad "helper pid should be gone"
}

test_lock_contention_fails_fast() {
    helper check
    cp "$RUN/state.json" "$T/state.before"
    flock "$RUN/lock" sleep 3 &
    local holder=$!
    sleep 0.3
    helper mods
    eq "exit status" 75 "$RC"
    has "$OUT" 'lock still held'
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

# --- adversarial review, round 1 ---------------------------------------------

helper_bg() { # like helper, but exec'd in the background: $! is the helper itself
    (exec env -i PATH="$PATH" HOME="$T" \
        GS_UPDATE_CONF="$T/etc/gs-update.conf" GS_UPDATE_RUN_DIR="$RUN" GS_UPDATE_STATE_DIR="$STATE" \
        GS_UPDATE_SNIPPET="$T/etc/matchzy_portal.cfg" GS_UPDATE_OVERRIDES="$T/etc/matchzy_overrides.cfg" \
        GS_UPDATE_LGSM_DIRECT=1 GS_UPDATE_NO_SYSLOG=1 \
        GS_UPDATE_GITHUB_API="file://$T/api" GS_UPDATE_MMS_URL="file://$T/mms-latest" \
        GS_UPDATE_VERIFY_POLL=0.2 GS_UPDATE_PUMP_INTERVAL=0.05 GS_UPDATE_LOCK_WAIT=1 \
        "$HELPER" "$@" >"$OUT" 2>&1) &
}
wait_for_phase() { # wait_for_phase <phase> [log line] — up to 10 s
    local _
    for _ in $(seq 1 100); do
        if [ "$(jq -r .phase "$RUN/state.json" 2>/dev/null)" = "$1" ] \
            && { [ -z "${2:-}" ] || grep -qF -- "$2" "$RUN/state.json"; }; then
            return 0
        fi
        sleep 0.1
    done
    bad "never reached phase $1"
}

# 1. Root must never read a name in the agent-writable run directory.
test_planted_state_link_is_never_read() {
    echo 'root-only-secret-content' >"$T/victim"
    # Writable directory: the link is replaced by a real file, target untouched.
    ln -s "$T/victim" "$RUN/state.json"
    ln -s "$T/victim" "$RUN/config.json"
    helper repair-config
    eq "exit status" 0 "$RC"
    [ -f "$RUN/state.json" ] && [ ! -L "$RUN/state.json" ] || bad "state.json should be a regular file now"
    [ -f "$RUN/config.json" ] && [ ! -L "$RUN/config.json" ] || bad "config.json should be a regular file now"
    eq "link target untouched" 'root-only-secret-content' "$(cat "$T/victim")"
    hasnt "$STATE/last-job.json" 'root-only-secret-content'
    eq "last-job.json is the job" succeeded "$(jq -r .status "$STATE/last-job.json")"

    # The reviewer's case: publishing fails (tmpfs full / not writable), so
    # the planted link survives. last-job.json must still be the job's own
    # record, not a copy of whatever the link points at.
    rm -f "$RUN/state.json" "$RUN/config.json" "$STATE/last-job.json"
    ln -s "$T/victim" "$RUN/state.json"
    chmod 0555 "$RUN"
    helper repair-config
    chmod 0755 "$RUN"
    eq "exit status (publish impossible)" 0 "$RC"
    [ -L "$RUN/state.json" ] || bad "test setup: the link should have survived"
    hasnt "$STATE/last-job.json" 'root-only-secret-content'
    eq "last-job.json status" succeeded "$(jq -r .status "$STATE/last-job.json")"
    eq "last-job.json says publishing failed" true "$(jq -r .state_publish_failed "$STATE/last-job.json")"
    has "$OUT" 'cannot publish state.json'
    eq "link target untouched" 'root-only-secret-content' "$(cat "$T/victim")"
    # A directory or FIFO planted at the name is cleared too.
    rm -f "$RUN/state.json"
    mkdir "$RUN/state.json"
    touch "$RUN/state.json/x"
    mkfifo "$RUN/config.json"
    helper repair-config
    eq "exit status (planted dir/fifo)" 0 "$RC"
    [ -f "$RUN/state.json" ] && [ -f "$RUN/config.json" ] || bad "planted dir/fifo should have been replaced"
}

# 2. Redaction must not depend on the line being valid UTF-8.
test_redaction_survives_invalid_utf8_and_odd_config_lines() {
    touch "$FAKE/latin1"
    # Secrets where a name-anchored rule would miss them.
    sed -i '2i foo 1; matchzy_demo_upload_header_value "inline-raw-secret"\nMatchZy_Remote_Log_Header_Value "MixedCaseSecret"' "$CFG"
    helper mods
    eq "exit status" 0 "$RC"
    has "$OUT" '[redacted line]'
    eq "redacted lines in the output" 4 "$(grep -c 'redacted line' "$OUT")"
    eq "inline secret" '{"name":"foo","before":"[redacted]","after":"[redacted]"}' \
        "$(st -c '.config_report.cvars_changed[], (.config_report.cvars_removed[] | {name: ., before: "[redacted]", after: "[redacted]"}) | select(.name == "foo")')"
    [ "$(st '[.config_report.cvars_removed[] | select(. == "MatchZy_Remote_Log_Header_Value")] | length')" = 1 ] \
        || bad "mixed-case cvar should be listed as removed (by name only)"
    no_secrets
    # And when such a value changes rather than disappears.
    echo 'MatchZy_Remote_Log_Header_Value "x"' >>"$FAKE/mods_next/cfg/MatchZy/config.cfg"
    echo 'foo 2' >>"$FAKE/mods_next/cfg/MatchZy/config.cfg"
    sed -i '2i foo 1; matchzy_demo_upload_header_value "inline-raw-secret"\nMatchZy_Remote_Log_Header_Value "MixedCaseSecret"' "$CFG"
    helper mods
    eq "mixed case redacted" '{"name":"MatchZy_Remote_Log_Header_Value","before":"[redacted]","after":"[redacted]"}' \
        "$(st -c '.config_report.cvars_changed[] | select(.name == "MatchZy_Remote_Log_Header_Value")')"
    eq "inline redacted" '{"name":"foo","before":"[redacted]","after":"2"}' \
        "$(st -c '.config_report.cvars_changed[] | select(.name == "foo")')"
    no_secrets
}

# 3. The snapshot a rollback needs is never pruned, and nothing live is
#    deleted before the archive is known to be good.
test_rollback_snapshot_survives_pruning_under_clock_skew() {
    local i
    mkdir -p "$STATE/snapshots"
    for i in 1 2 3; do
        echo x >"$STATE/snapshots/0000000$i-0000-0000-0000-000000000000.tar.zst"
        touch -d '+2 days' "$STATE/snapshots/0000000$i-0000-0000-0000-000000000000.tar.zst"
    done
    touch "$FAKE/mods_next/addons/BROKEN"
    helper mods
    eq "exit status" 3 "$RC"
    st_eq .status rolled_back
    eq "previous plugin files restored" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
    [ -e "$STATE/snapshots/$(st .job_id).tar.gz" ] || [ -e "$STATE/snapshots/$(st .job_id).tar.zst" ] \
        || bad "this job's snapshot was pruned"
    eq "snapshots kept (SNAPSHOT_KEEP=2)" 2 "$(find "$STATE/snapshots" -type f | wc -l)"
}

test_unreadable_snapshot_does_not_delete_the_live_tree() {
    echo "$STATE/snapshots" >"$FAKE/corrupt"
    touch "$FAKE/mods_next/addons/BROKEN"
    helper mods
    eq "exit status" 1 "$RC"
    st_eq .status failed
    [[ $(st .error) == *'restoring the snapshot'*'also failed'* ]] || bad "error: $(st .error)"
    has "$OUT" 'snapshot is missing or unreadable - leaving the current files in place'
    # The update's files are still there — a broken plugin set, not an empty tree.
    [ -d "$CSGO/addons/metamod" ] || bad "addons/ was deleted"
    eq "updated files left in place" 0.9.3 "$(cat "$CSGO/addons/matchzy/VERSION")"
    [ -s "$CFG" ] || bad "cfg/MatchZy was deleted"
    eq "server started anyway" 'stop,mods-update,start,stop,start,' "$(calls)"
    eq "no staging left" 0 "$(find "$CSGO" -maxdepth 1 -name '.portal-*' | wc -l)"
}

test_snapshot_leaves_out_plugin_logs_and_restores_by_swap() {
    mkdir -p "$CSGO/addons/counterstrikesharp/logs"
    echo 'log line' >"$CSGO/addons/counterstrikesharp/logs/log-today.txt"
    touch "$FAKE/mods_next/addons/BROKEN"
    helper mods
    st_eq .status rolled_back
    local snap
    snap=$(find "$STATE/snapshots" -type f | head -n 1)
    if tar -tf "$snap" | grep -q '/logs'; then bad "plugin logs are in the snapshot"; fi
    tar -tf "$snap" | grep -q '^addons/matchzy/VERSION$' || bad "snapshot lacks the plugin files"
    tar -tf "$snap" | grep -q '^cfg/MatchZy/config.cfg$' || bad "snapshot lacks the MatchZy config"
    eq "no staging left" 0 "$(find "$CSGO" -maxdepth 1 -name '.portal-*' | wc -l)"
    [ ! -e "$CSGO/addons/BROKEN" ] || bad "the failed update's files should be gone"
}

# 6. An interrupted job must not leave the server down.
test_sigterm_while_the_server_is_stopped_starts_it_again() {
    echo 2000 >"$FAKE/remote_build"
    echo 31 >"$FAKE/slow"
    helper_bg all
    local pid=$!
    wait_for_phase game-update 'SteamCMD downloading'
    kill -TERM "$pid"
    wait "$pid"
    eq "exit status" 143 "$?"
    st_eq .status failed
    st_eq .failed_phase game-update
    st_eq .restarted true
    st_eq .load.matchzy true
    [[ $(st .error) == *'interrupted (status 143) during phase game-update'*'plugin files were not touched'*'server started and the plugin stack loaded'* ]] \
        || bad "error: $(st .error)"
    eq "calls" 'update-lgsm,stop,update,start,' "$(calls)"
    eq "server state" running "$(cat "$FAKE/running")"
    eq "last-job.json" failed "$(jq -r .status "$STATE/last-job.json")"
    cfg_eq .demo_upload_wired true
    # The LinuxGSM command it was running is gone with it (no orphan without the lock).
    sleep 0.3
    if pgrep -f "$GS/cs2server update" >/dev/null; then bad "the LinuxGSM child is still running"; fi
    if pgrep -f 'sleep 31' >/dev/null; then bad "a process started by the LinuxGSM command is still running"; fi
    eq "work dirs left" 0 "$(find "$STATE" -maxdepth 1 -name '.work.*' | wc -l)"
}

test_sigterm_during_mods_update_restores_the_snapshot() {
    echo 30 >"$FAKE/slow_mods"
    cp -p "$CFG" "$T/config.before"
    helper_bg mods
    local pid=$!
    wait_for_phase mods-update 'extracting'
    eq "half-updated files are on disk" 0.9.3 "$(cat "$CSGO/addons/matchzy/VERSION")"
    kill -TERM "$pid"
    wait "$pid"
    eq "exit status" 143 "$?"
    st_eq .status failed
    st_eq .failed_phase mods-update
    st_eq .restarted true
    st_eq .config_report null
    [[ $(st .error) == *'during phase mods-update; plugin files restored from'*'server started and the plugin stack loaded'* ]] \
        || bad "error: $(st .error)"
    eq "previous plugin files restored" 0.9.2 "$(cat "$CSGO/addons/matchzy/VERSION")"
    [ ! -e "$CSGO/addons/matchzy/NewFile.dll" ] || bad "the update's new files should be gone"
    cmp -s "$T/config.before" "$CFG" || bad "config.cfg should be back to the pre-update file"
    eq "calls" 'stop,mods-update,stop,start,' "$(calls)"
    eq "server state" running "$(cat "$FAKE/running")"
    st_eq .after.matchzy 0.9.2
}

test_sigterm_outside_a_transaction_just_fails() {
    echo 30 >"$FAKE/slow"
    echo 2000 >"$FAKE/remote_build"
    helper_bg game
    local pid=$!
    wait_for_phase game-update 'SteamCMD downloading'
    kill -TERM "$pid"
    wait "$pid"
    st_eq .status failed
    # `update` may have had the server down: it is started, nothing restored.
    eq "calls" 'update,start,' "$(calls)"
    [[ $(st .error) == *'plugin files were not touched'* ]] || bad "error: $(st .error)"
}

# 7. A job waits for the lock; verify-config never does.
test_job_waits_for_a_lock_held_briefly() {
    helper check
    flock "$RUN/lock" sleep 0.6 &
    local holder=$!
    sleep 0.2
    : >"$FAKE/calls"
    helper repair-config
    eq "exit status" 0 "$RC"
    st_eq .operation repair-config
    wait "$holder"
    # Reviewer's race: verify-config and a job started at the same moment.
    local i busy=0
    for i in $(seq 1 15); do
        helper_bg verify-config
        local v=$!
        OUT="$T/out-job.txt" helper repair-config
        [ "$RC" = 0 ] || busy=$((busy + 1))
        wait "$v"
    done
    eq "jobs that lost to verify-config" 0 "$busy"
}

# 8. Agent-writable inputs: one open, no links, no FIFOs, a real size cap.
test_untrusted_inputs_are_not_followed_or_overread() {
    # Overrides through a symlink: refused, even though the target is fine.
    echo 'matchzy_minimum_ready_required 5' >"$T/etc/real-overrides"
    ln -s "$T/etc/real-overrides" "$T/etc/matchzy_overrides.cfg"
    helper repair-config
    eq "exit status" 0 "$RC"
    hasnt "$CFG" 'matchzy_minimum_ready_required "5"'
    cfg_eq .overrides_applied false
    has "$OUT" 'not a plain file'
    # Past the cap: nothing of it is used (not even the lines before the cap).
    rm "$T/etc/matchzy_overrides.cfg"
    { echo 'matchzy_first 1'; head -c 70000 /dev/zero | tr '\0' '\n'; echo 'matchzy_past_the_cap 1'; } >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    hasnt "$CFG" matchzy_past_the_cap
    hasnt "$CFG" matchzy_first
    cfg_eq .overrides_applied false
    # A FIFO does not hang the helper.
    rm "$T/etc/matchzy_overrides.cfg"
    mkfifo "$T/etc/matchzy_overrides.cfg"
    local t0=$SECONDS
    helper repair-config
    eq "exit status (fifo overrides)" 0 "$RC"
    [ $((SECONDS - t0)) -lt 8 ] || bad "a FIFO stalled the helper for $((SECONDS - t0))s"
    cfg_eq .overrides_applied false
    rm "$T/etc/matchzy_overrides.cfg"

    # The snippet through a symlink: treated as no snippet, never copied.
    echo 'SECRET-OTHER-FILE' >"$T/other"
    rm "$T/etc/matchzy_portal.cfg"
    ln -s "$T/other" "$T/etc/matchzy_portal.cfg"
    helper repair-config
    hasnt "$CFG" SECRET-OTHER-FILE
    cfg_eq .demo_upload_wired null

    # request.json: a link, a FIFO, a directory, or something big → 65, at once.
    ln -s "$T/other" "$RUN/request.json"
    helper repair-config
    eq "exit status (symlink request)" 65 "$RC"
    hasnt "$OUT" SECRET-OTHER-FILE
    [ ! -L "$RUN/request.json" ] || bad "the planted request link should be removed"
    mkfifo "$RUN/request.json"
    t0=$SECONDS
    helper repair-config
    eq "exit status (fifo request)" 65 "$RC"
    [ $((SECONDS - t0)) -lt 8 ] || bad "a FIFO request stalled the helper"
    mkdir "$RUN/request.json"
    helper repair-config
    eq "exit status (directory request)" 65 "$RC"
    { printf '{"operation":"repair-config","job_id":"11111111-2222-4333-8444-555555555555","pad":"'; head -c 5000 /dev/zero | tr '\0' 'x'; printf '"}'; } >"$RUN/request.json"
    helper repair-config
    eq "exit status (oversized request)" 65 "$RC"
    echo '{"job_id":["a"],"operation":"repair-config"}' >"$RUN/request.json"
    helper repair-config
    eq "exit status (job_id not a string)" 65 "$RC"
    eq "work dirs left" 0 "$(find "$STATE" -maxdepth 1 -name '.work.*' | wc -l)"
}

# e. The helper's override rules are the agent's: printable ASCII only.
test_hostile_overrides_never_reach_the_config() {
    {
        printf '%s\n' \
            'matchzy_ok 5' \
            'matchzy_semi 1; rcon_password pwned' \
            'matchzy_quote 1" ; exec evil "' \
            'matchzy_demo_upload_url http://evil.example/' \
            'MATCHZY_DEMO_UPLOAD_URL http://evil.example/' \
            'Matchzy_Remote_log_url http://evil.example/' \
            'matchzy_remote_log_url http://evil.example/' \
            'matchzy_loadmatch_url http://evil.example/' \
            'matchzy_loadbackup_url http://evil.example/' \
            'exec evil.cfg' \
            'alias matchzy_x "quit"' \
            "// END PORTAL MANAGED — matchzy overrides" \
            "// END ANSIBLE MANAGED — portal demo upload" \
            'matchzy_bs 1\' \
            'matchzy_two_spaces  1' \
            ' matchzy_leading 1'
        printf 'matchzy_tab\ta\tb\n'
        printf 'matchzy_tabval a\tb\n'
        printf 'matchzy_cr 1\rquit\n'
        printf 'matchzy_u2028 1\xe2\x80\xa8quit\n'
        printf 'matchzy_nbsp 1\xc2\xa0quit\n'
        printf 'matchzy_latin1 caf\xe9\n'
        printf 'matchzy_del 1\x7f\n'
        printf 'matchzy_fine a b c $(id) `id` ${HOME} // x\n'
    } >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    eq "exit status" 0 "$RC"
    eq "overrides block" "$OVR_BEGIN|matchzy_ok \"5\"|matchzy_two_spaces \" 1\"|matchzy_fine \"a b c \$(id) \`id\` \${HOME} // x\"|$OVR_END|" \
        "$(sed -n "/^\/\/ BEGIN PORTAL/,/^\/\/ END PORTAL/p" "$CFG" | tr '\n' '|')"
    eq "one END marker for each block" '1 1' "$(grep -c "^$OVR_END" "$CFG") $(grep -c "^$DEMO_END" "$CFG")"
    eq "non-ASCII bytes in the block" 0 "$(sed -n "/^\/\/ BEGIN PORTAL/,/^\/\/ END PORTAL/p" "$CFG" | sed '1d;$d' | LC_ALL=C grep -c '[^ -~]')"
    cfg_eq .overrides_applied false
    [[ $(jq -r .reason "$RUN/config.json") == '21 line(s) of the MatchZy overrides file were rejected'* ]] || bad "reason: $(jq -r .reason "$RUN/config.json")"
    cfg_eq .demo_upload_wired true
    # 64 is the limit; the 65th is dropped and reported.
    for i in $(seq 1 65); do echo "matchzy_cvar_$i $i"; done >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    eq "override lines" 64 "$(grep -c '^matchzy_cvar_' "$CFG")"
    cfg_eq .overrides_applied false
    # Limits: 128-byte names and 256-byte values pass, one more does not.
    {
        echo "matchzy_$(head -c 120 /dev/zero | tr '\0' a) 1"
        echo "matchzy_$(head -c 121 /dev/zero | tr '\0' b) 1"
        echo "matchzy_v256 $(head -c 256 /dev/zero | tr '\0' x)"
        echo "matchzy_v257 $(head -c 257 /dev/zero | tr '\0' y)"
    } >"$T/etc/matchzy_overrides.cfg"
    helper repair-config
    eq "accepted at the limits" 2 "$(sed -n "/^\/\/ BEGIN PORTAL/,/^\/\/ END PORTAL/p" "$CFG" | grep -c '^matchzy_')"
    hasnt "$CFG" matchzy_v257
    hasnt "$CFG" bbbbbbbbbb
}

# d. AUTO_REPAIR is the operator's switch; the helper passes it on.
test_auto_repair_switch_is_exposed() {
    helper verify-config
    cfg_eq .auto_repair true
    echo 'AUTO_REPAIR=false' >>"$T/etc/gs-update.conf"
    helper verify-config
    cfg_eq .auto_repair false
    sed -i 's/^AUTO_REPAIR=.*/AUTO_REPAIR=maybe/' "$T/etc/gs-update.conf"
    helper verify-config
    eq "exit status (bad value)" 78 "$RC"
}

# h. Half a block is removed with the stale token line it held; writes are
#    by rename; gameinfo backups are bounded.
test_half_blocks_are_removed_with_their_contents() {
    # BEGIN without END (a truncated write), stale token inside.
    printf '%s\n' 'matchzy_a 1' "$DEMO_BEGIN" '// Written by portal-server-agent enroll.' \
        'matchzy_demo_upload_url "https://old.example/"' \
        'matchzy_demo_upload_header_value "Bearer OLD-STALE-TOKEN"' 'matchzy_b 2' >"$CFG"
    helper repair-config
    eq "exit status" 0 "$RC"
    hasnt "$CFG" OLD-STALE-TOKEN
    hasnt "$CFG" old.example
    has "$CFG" 'matchzy_a 1'
    has "$CFG" 'matchzy_b 2'
    eq "markers" '1 1' "$(grep -c "^$DEMO_BEGIN" "$CFG") $(grep -c "^$DEMO_END" "$CFG")"
    eq "token lines" 1 "$(grep -c header_value "$CFG")"
    cfg_eq .demo_upload_wired true
    blockinfile_converge "$CFG" "$T/etc/matchzy_portal.cfg" >"$T/converged.cfg"
    cmp -s "$T/converged.cfg" "$CFG" || bad "an Ansible blockinfile converge would change the file"

    # END without BEGIN, stale token right before it.
    printf '%s\n' 'matchzy_a 1' 'matchzy_demo_upload_url "https://old.example/"' \
        'matchzy_demo_upload_header_value "Bearer OLD-STALE-TOKEN"' "$DEMO_END" 'matchzy_b 2' >"$CFG"
    helper repair-config
    hasnt "$CFG" OLD-STALE-TOKEN
    hasnt "$CFG" old.example
    has "$CFG" 'matchzy_a 1'
    has "$CFG" 'matchzy_b 2'
    eq "markers" '1 1' "$(grep -c "^$DEMO_BEGIN" "$CFG") $(grep -c "^$DEMO_END" "$CFG")"
    cfg_eq .demo_upload_wired true

    # Two blocks, CRLF file: one block left, operator lines intact, stable.
    printf 'a 1\r\n%s\r\nx 1\r\n%s\r\nb 2\r\n%s\r\nold 2\r\n%s\r\n' "$DEMO_BEGIN" "$DEMO_END" "$DEMO_BEGIN" "$DEMO_END" >"$CFG"
    helper repair-config
    eq "markers" '1 1' "$(grep -c "^$DEMO_BEGIN" "$CFG") $(grep -c "^$DEMO_END" "$CFG")"
    hasnt "$CFG" 'old 2'
    has "$CFG" 'b 2'
    cp "$CFG" "$T/first"
    helper repair-config
    st_eq .config_changed false
    cmp -s "$T/first" "$CFG" || bad "not stable"

    # A stray overrides END must not take the operator's settings with it.
    printf '%s\n' 'matchzy_keep_me "1"' "$OVR_END" 'matchzy_b 2' >"$CFG"
    helper repair-config
    has "$CFG" 'matchzy_keep_me "1"'
    eq "stray marker gone" 0 "$(grep -c "^$OVR_END" "$CFG")"
    hasnt "$RUN/state.json" OLD-STALE-TOKEN
}

test_config_and_gameinfo_are_replaced_by_rename() {
    # config.cfg: new inode (rename), mode 0640 whatever it was, no temp left.
    cp "$FAKE/mods_next/cfg/MatchZy/config.cfg" "$CFG"
    chmod 0664 "$CFG"
    local inode
    inode=$(stat -c %i "$CFG")
    helper repair-config
    st_eq .config_changed true
    eq "config mode" 640 "$(stat -c %a "$CFG")"
    [ "$(stat -c %i "$CFG")" != "$inode" ] || bad "config.cfg was rewritten in place, not renamed into place"
    eq "temp files left" 0 "$(find "$CSGO" -name '*.portal-tmp' | wc -l)"
    # A config that does not exist yet is created 0640.
    rm "$CFG"
    helper repair-config
    eq "created mode" 640 "$(stat -c %a "$CFG")"
    cfg_eq .demo_upload_wired true

    # gameinfo.gi keeps its mode; old backups are pruned to five.
    chmod 0600 "$CSGO/gameinfo.gi"
    local i
    for i in 1 2 3 4 5 6 7; do touch "$CSGO/gameinfo.gi.bak-2020-01-0$i-000000"; done
    echo 2000 >"$FAKE/remote_build"
    touch "$FAKE/strip_gameinfo"
    inode=$(stat -c %i "$CSGO/gameinfo.gi")
    helper game
    eq "exit status" 0 "$RC"
    eq "gameinfo mode kept" 600 "$(stat -c %a "$CSGO/gameinfo.gi")"
    [ "$(stat -c %i "$CSGO/gameinfo.gi")" != "$inode" ] || bad "gameinfo.gi was rewritten in place"
    eq "backups kept" 5 "$(find "$CSGO" -maxdepth 1 -name 'gameinfo.gi.bak-*' | wc -l)"
    [ ! -e "$CSGO/gameinfo.gi.bak-2020-01-01-000000" ] || bad "the oldest backup should be gone"
    [ -e "$CSGO/gameinfo.gi.bak-2020-01-07-000000" ] || bad "a recent backup was pruned"
    eq "temp files left" 0 "$(find "$CSGO" -name '*.portal-tmp' | wc -l)"
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
