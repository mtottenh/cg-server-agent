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
#   fake/rotate           `start` begins a new console log (LinuxGSM does)
#   fake/slow             `update` prints a line, then takes this many seconds
#   fake/slow_mods        `mods-update` copies its files, then takes this long
#   fake/latin1           `mods-update` prints secrets after a non-UTF-8 byte
#   fake/corrupt          `mods-update` truncates every file in this directory
#   fake/lock_cfg         `mods-update` makes csgo/cfg read-only
#   fake/slow_check       `check-update` takes this many seconds
#   fake/update_rc        exit status of `update`
#   fake/orphan           `update` leaves a detached process behind (as
#                         SteamCMD under `runuser -l` would) and one that
#                         looks like the tmux server, then hangs
#
# Like LinuxGSM: `start` on a running server does nothing, and `update`
# stops the server only when there is an update.
#                         (the helper's snapshots: a disk that lost them)
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
            # As the real console prints them, colour codes included.
            printf '\033[38;5;10mCSSharp: \033[0mCounterStrikeSharp.API Loaded Successfully.\033[0m\n'
            printf '\033[32m[MatchZy %s LOADED] MatchZy by WD- (https://github.com/shobhit-pathak/) [build 5fd0271, CounterStrikeSharp API %s, Ubuntu 24.04.5 LTS]\033[0m\n' \
                "$(cat "$csgo/addons/matchzy/VERSION")" "$(cat "$csgo/addons/counterstrikesharp/VERSION")"
        fi
    } >>"$console"
}

case ${1:-} in
    check-update)
        printf '\033[32mChecking for update\033[0m\n'
        echo "* Local build: $(local_build)"
        echo "* Remote build: $(cat "$fake/remote_build")"
        if [ -e "$fake/slow_check" ]; then
            sleep "$(cat "$fake/slow_check")"
        fi
        ;;
    update | force-update)
        echo 'Details: rcon_password "lgsm-rcon-secret"'
        remote=$(cat "$fake/remote_build")
        if [ -e "$fake/orphan" ]; then
            # Own sessions: out of reach of `timeout` and of a group kill.
            setsid sleep 37 &
            setsid "$fake/tmux" 38 &
            echo "SteamCMD downloading"
            sleep 300
        fi
        if [ -e "$fake/slow" ]; then
            if [ "$(local_build)" != "$remote" ]; then
                echo stopped >"$fake/running"
            fi
            echo "SteamCMD downloading"
            sleep "$(cat "$fake/slow")"
        fi
        if [ -e "$fake/update_rc" ]; then
            echo "Error: SteamCMD failed"
            exit "$(cat "$fake/update_rc")"
        fi
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
        if [ -e "$fake/latin1" ]; then
            # A Latin-1 byte (a player name, a progress glyph) before the secret.
            printf 'Installing MatchZy \xe9 : matchzy_demo_upload_header_value "Bearer latin1-token-secret"\n'
            printf 'caf\xe9 \xff rcon_password "latin1-rcon-secret"\n'
        fi
        if [ -d "$fake/mods_next" ]; then
            cp -Rf "$fake/mods_next/." "$csgo/"
        fi
        if [ -e "$fake/lock_cfg" ]; then
            chmod 0555 "$csgo/cfg"
        fi
        if [ -e "$fake/corrupt" ]; then
            for f in "$(cat "$fake/corrupt")"/*; do : >"$f"; done
        fi
        if [ -e "$fake/slow_mods" ]; then
            echo "extracting"
            sleep "$(cat "$fake/slow_mods")"
        fi
        exit "$(cat "$fake/mods_rc" 2>/dev/null || echo 0)"
        ;;
    stop)
        echo stopped >"$fake/running"
        ;;
    start)
        if [ "$(cat "$fake/running" 2>/dev/null)" = running ]; then
            echo "cs2server is already running"
        else
            echo running >"$fake/running"
            boot
        fi
        ;;
    restart)
        echo running >"$fake/running"
        boot
        ;;
    *)
        echo "unknown command: ${1:-}" >&2
        exit 2
        ;;
esac
