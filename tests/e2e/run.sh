#!/usr/bin/env bash
# End-to-end tests.
#
#   docker compose --profile test run --rm e2e             every suite
#   docker compose --profile test run --rm e2e bash tests/e2e/run.sh network
#   docker compose run --rm dev bash tests/e2e/run.sh features readonly ui
#
# Suites:
#   features  the search syntax, other scripts, content, properties, run
#             history, exports, live updates and per-user filtering
#   readonly  indexing and searching change nothing, not even access times
#   servers   the ETP/FTP, HTTP and index servers and remote indexes, used
#             by independent clients
#   ui        the desktop client, driven and read through accessibility
#   network   real SMB and NFS servers (needs the test profile's services)
#
# All but network need only a privileged container; they index two tmpfs
# volumes with their own daemon.
set -uo pipefail
cd "$(dirname "$0")/../.."
. tests/e2e/lib.sh
. tests/e2e/features.sh
. tests/e2e/readonly.sh
. tests/e2e/servers.sh
. tests/e2e/ui.sh
. tests/e2e/network.sh

SUITES=" ${*:-features readonly servers ui network} "
want() { [[ $SUITES == *" $1 "* ]]; }

section "building"
packages=(-p evosd -p evos-cli)
want ui && packages+=(-p evos-gtk)
cargo build -q --release "${packages[@]}" || exit 1
export PATH=/target/release:$PATH

if want features || want readonly || want servers || want ui; then
    LOCAL=/tmp/e2e-local
    umount "$LOCAL/disk" "$LOCAL/ro" 2>/dev/null
    rm -rf "$LOCAL"
    mkdir -p "$LOCAL/disk" /run/evos-e2e-local
    DISK=$LOCAL/disk
    RO=$LOCAL/ro
    mount -t tmpfs e2e-disk "$DISK" || { echo "cannot mount tmpfs (run privileged)"; exit 1; }
    make_tree "$DISK"
    want readonly && setup_readonly
    export EVOS_SOCKET=/run/evos-e2e-local/evosd.sock
    cat >"$LOCAL/evosd.toml" <<EOF
state_dir = "$LOCAL/state"
config_dir = "$LOCAL/etc"
run_dir = "/run/evos-e2e-local"
socket = "$EVOS_SOCKET"
[local]
index_tmpfs = true
skip_mounts = [ $(skip_all_mounts_but "$DISK" "$RO") ]
[network]
index_unmanaged = false
EOF
    evosd --config "$LOCAL/evosd.toml" >"$LOCAL/evosd.log" 2>&1 &
    LOCAL_DAEMON=$!
    export E2E_EVOSD_CONFIG="$LOCAL/evosd.toml" E2E_EVOSD_LOG="$LOCAL/evosd.log"
    wait_for 10 evos status || { echo "daemon did not start"; cat "$LOCAL/evosd.log"; exit 1; }

    want features && run_features
    want servers && run_servers
    if want ui; then
        section "desktop client"
        if start_ui; then
            run_ui_test local "$DISK" "$RO"
        else
            fail "the desktop client did not start"
        fi
        stop_ui
    fi
    want readonly && check_readonly

    # The UI suite restarts the daemon, so stop whichever instance runs.
    pkill -x evosd
    wait_for 20 sh -c "! pgrep -x evosd"
    umount "$DISK" "$RO" 2>/dev/null
    if [ "$FAIL" -gt 0 ]; then
        echo "--- local daemon log (tail)"
        tail -20 "$LOCAL/evosd.log"
        want ui && { echo "--- desktop client log (tail)"; tail -20 "$UI_DIR/gtk.log"; }
    fi
fi

want network && run_network

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
