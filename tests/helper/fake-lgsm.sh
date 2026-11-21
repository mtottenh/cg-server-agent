#!/usr/bin/env bash
# A stand-in for a LinuxGSM instance script (`cs2server`), driven by control
# files in ./fake beside it. tests/helper/run.sh copies it into a temp tree.
#
#   fake/remote_build     build id `check-update` reports and `update` installs
#   fake/strip_gameinfo   `update` drops the Metamod line from gameinfo.gi
#   fake/mods_next/       tree `mods-update` copies over game/csgo (cp -Rf,
#                         like LinuxGSM's OVERWRITE policy)
#   fake/mods_rc          exit status of `mods-update`
#   fake/next_lgsm        version `update-lgsm` moves to
#   fake/send_rc          exit status of `send`
#   fake/rotate           `start` begins a new console log (LinuxGSM does)
#   fake/slow             `update` prints a line, then takes this many seconds
#
# Every call is appended to fake/calls.
# shellcheck disable=SC2034  # read by the helper, rewritten by update-lgsm
version="v26.6.0"

set -u
root=$(cd "$(dirname "$0")" && pwd)
fake="$root/fake"
csgo="$root/serverfiles/game/csgo"
manifest="$root/serverfiles/steamapps/appmanifest_730.acf"
console="$root/log/console/cs2server-console.log"

echo "$*" >>"$fake/calls"

local_build() { sed -n -E 's/^[[:space:]]*"buildid"[[:space:]]+"([0-9]+)".*/\1/p' "$manifest"; }

boot() { # what CS2 prints when it starts with the files as they are now
    if [ -e "$fake/rotate" ]; then
        mv "$console" "$console.old"
    fi
    {
        echo "Loading map"
        # Never to be seen outside this file: the helper must redact it.
        echo 'rcon_password "console-rcon-secret"'
        if ! grep -q 'csgo/addons/metamod' "$csgo/gameinfo.gi"; then
            : # no search path: Metamod never loads, and says nothing
        elif [ -e "$csgo/addons/BROKEN" ]; then
            echo "MMS: Fatal error: plugin ABI mismatch"
        elif [ -e "$csgo/addons/SILENT" ]; then
            : # loads nothing and reports nothing
        else
            echo "CounterStrikeSharp API v$(cat "$csgo/addons/counterstrikesharp/VERSION")"
            echo "CounterStrikeSharp.API Loaded Successfully."
            echo "[MatchZy $(cat "$csgo/addons/matchzy/VERSION") LOADED]"
        fi
    } >>"$console"
}

case ${1:-} in
    check-update)
        printf '\033[32mChecking for update\033[0m\n'
        echo "* Local build: $(local_build)"
        echo "* Remote build: $(cat "$fake/remote_build")"
        ;;
    update | force-update)
        echo 'Details: rcon_password "lgsm-rcon-secret"'
        if [ -e "$fake/slow" ]; then
            echo "SteamCMD downloading"
            sleep "$(cat "$fake/slow")"
        fi
        remote=$(cat "$fake/remote_build")
        if [ "$(local_build)" != "$remote" ]; then
            sed -i -E "s/(\"buildid\"[[:space:]]+)\"[0-9]+\"/\1\"$remote\"/" "$manifest"
            if [ -e "$fake/strip_gameinfo" ]; then
                sed -i '/csgo\/addons\/metamod/d' "$csgo/gameinfo.gi"
            fi
            echo "Update complete"
        else
            echo "No update available"
        fi
        ;;
    update-lgsm)
        if [ -e "$fake/next_lgsm" ]; then
            sed -i -E "s/^version=.*/version=\"$(cat "$fake/next_lgsm")\"/" "$0"
        fi
        ;;
    mods-update)
        # A chatty installer that echoes config lines, token included.
        grep -h 'matchzy_demo_upload' "$csgo/cfg/MatchZy/config.cfg"
        if [ -d "$fake/mods_next" ]; then
            cp -Rf "$fake/mods_next/." "$csgo/"
        fi
        exit "$(cat "$fake/mods_rc" 2>/dev/null || echo 0)"
        ;;
    stop)
        echo stopped >"$fake/running"
        ;;
    start | restart)
        echo running >"$fake/running"
        boot
        ;;
    send)
        echo "${2:-}" >>"$fake/sent"
        exit "$(cat "$fake/send_rc" 2>/dev/null || echo 0)"
        ;;
    *)
        echo "unknown command: ${1:-}" >&2
        exit 2
        ;;
esac
