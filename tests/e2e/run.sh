#!/usr/bin/env bash
# End-to-end tests against real SMB and NFS servers.
#
#   docker compose --profile test run --rm e2e
#
# The share contents live in a volume that is also mounted here at /server,
# so "server-side" changes (made directly on the server's disk) can be
# distinguished from changes made by another client through SMB.
set -uo pipefail

FILES=${E2E_FILES:-100000}
DIRS=${E2E_DIRS:-5000}
RECHECK=${E2E_RECHECK:-5}
export EVOS_SOCKET=/run/evos-e2e/evosd.sock

PASS=0
FAIL=0
pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }
info() { echo "      $*"; }
now() { date +%s.%N; }
since() { awk "BEGIN { printf \"%.1f\", $(now) - $1 }"; }

# wait_for SECONDS COMMAND...: retry until the command succeeds.
wait_for() {
    local end=$(($(date +%s) + $1))
    shift
    while (($(date +%s) < end)); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.25
    done
    return 1
}

# evos exits 1 when nothing matched (like grep); only its output matters.
count() {
    local n
    n=$(evos --count "$@" 2>/dev/null)
    echo "${n:-0}"
}
status_field() { evos status 2>/dev/null | awk -v root="$1" -v col="$2" '$1 == root { print $col }'; }
phase_is() { [ "$(status_field "$1" 3)" = "$2" ]; }
has_hits() { [ "$(count "$@")" -ge 1 ]; }
no_hits() { [ "$(count "$@")" -eq 0 ]; }

# populate ROOT: FILES files in DIRS folders, three levels deep.
populate() {
    local root=$1
    rm -rf "${root:?}"/*
    mkdir -p "$root"
    seq 0 $((DIRS - 1)) |
        awk -v r="$root" '{ printf "%s/l%02d/m%03d/d%05d\n", r, $1 % 50, $1 % 500, $1 }' |
        xargs mkdir -p
    seq 0 $((FILES - 1)) |
        awk -v r="$root" -v d="$DIRS" '{ n = $1 % d; printf "%s/l%02d/m%03d/d%05d/file-%07d.dat\n", r, n % 50, n % 500, n, $1 }' |
        xargs touch
}

echo "== building"
cargo build -q --release -p evosd -p evos-cli || exit 1
export PATH=/target/release:$PATH

echo "== generating $FILES files in $DIRS folders per share"
t=$(now)
populate /server/smb
populate /server/nfs
info "generated in $(since "$t")s"

# Index only what this test mounts, whatever else the container has.
mkdir -p /tmp/e2e /run/evos-e2e
skip=$(awk '{ printf "\"%s\", ", $5 }' /proc/self/mountinfo)
cat >/tmp/e2e/evosd.toml <<EOF
state_dir = "/tmp/e2e/state"
config_dir = "/tmp/e2e/etc"
run_dir = "/run/evos-e2e"
socket = "$EVOS_SOCKET"
[local]
skip_mounts = [ $skip ]
[network]
index_unmanaged = false
offline_retry_secs = 5
EOF

start_daemon() {
    evosd --config /tmp/e2e/evosd.toml >>/tmp/e2e/evosd.log 2>&1 &
    DAEMON=$!
    wait_for 10 evos status
}
start_daemon || { echo "daemon did not start"; cat /tmp/e2e/evosd.log; exit 1; }

SMB=/mnt/evos/smb
NFS=/mnt/evos/nfs
DEEP=l07/m107/d00107

echo "== SMB share"
t=$(now)
if echo evos | evos mount add smb --type smb --source //samba/share --user evos --password-stdin --recheck "$RECHECK"; then
    pass "mounted //samba/share with credentials"
else
    fail "mount //samba/share"
fi
if wait_for 900 phase_is "$SMB" live; then
    pass "initial SMB scan finished in $(since "$t")s"
else
    fail "SMB scan did not finish: $(evos status)"
fi
n=$(count "under:$SMB" file:)
[ "$n" -eq "$FILES" ] && pass "all $FILES SMB files indexed" || fail "SMB file count $n != $FILES"
info "status: $(evos status | grep "$SMB")"

t=$(now)
smbclient //samba/share -U evos%evos -c "put /etc/hostname $DEEP/via-smb-client.txt" >/dev/null 2>&1
if wait_for 120 has_hits via-smb-client.txt; then
    pass "file created by another SMB client found after $(since "$t")s"
else
    fail "file created through SMB not found"
fi
info "watch mode: $(status_field "$SMB" 4)"

t=$(now)
mkdir -p /server/smb/l13/m213/d01213/server-made
touch /server/smb/l13/m213/d01213/server-made/on-server-disk.txt
if wait_for 180 has_hits on-server-disk.txt; then
    pass "file created on the server's disk found after $(since "$t")s"
else
    fail "server-side SMB change not found"
fi

t=$(now)
smbclient //samba/share -U evos%evos -c "del $DEEP/via-smb-client.txt" >/dev/null 2>&1
if wait_for 120 no_hits via-smb-client.txt; then
    pass "deletion through SMB noticed after $(since "$t")s"
else
    fail "deletion through SMB not noticed"
    info "status: $(evos status | grep "$SMB")"
    info "client sees: $(ls $SMB/$DEEP | grep -c via-smb) server sees: $(ls /server/smb/$DEEP | grep -c via-smb)"
fi

echo "== NFS export"
t=$(now)
if evos mount add nfs --type nfs --source nfs:/export --recheck "$RECHECK"; then
    pass "mounted nfs:/export"
else
    fail "mount nfs:/export"
fi
if wait_for 900 phase_is "$NFS" live; then
    pass "initial NFS scan finished in $(since "$t")s"
else
    fail "NFS scan did not finish: $(evos status)"
fi
n=$(count "under:$NFS" file:)
[ "$n" -eq "$FILES" ] && pass "all $FILES NFS files indexed" || fail "NFS file count $n != $FILES"
info "status: $(evos status | grep "$NFS")"

t=$(now)
touch /server/nfs/l21/m321/d00321/nfs-server-side.txt
if wait_for 180 has_hits nfs-server-side.txt; then
    pass "NFS server-side change found after $(since "$t")s (scheduled sweeps)"
else
    fail "NFS server-side change not found"
fi

echo "== adaptive concurrency under 5 ms latency"
if tc qdisc add dev eth0 root netem delay 5ms 2>/dev/null; then
    t=$(now)
    evos mount add slow --type nfs --source nfs:/export --mountpoint /mnt/evos/slow >/dev/null
    if wait_for 900 phase_is /mnt/evos/slow live; then
        pass "scan over a 5 ms link finished in $(since "$t")s"
    else
        fail "scan over a 5 ms link did not finish"
    fi
    info "status: $(evos status | grep /mnt/evos/slow)"
    tc qdisc del dev eth0 root 2>/dev/null
    evos mount rm slow >/dev/null
else
    info "skipped: netem is not available in this kernel"
fi

echo "== offline share"
ip=$(getent hosts samba | awk '{ print $1 }')
ip route add blackhole "$ip/32"
t=$(now)
if wait_for 240 phase_is "$SMB" offline; then
    pass "unreachable SMB server detected after $(since "$t")s"
    n=$(count "under:$SMB" file:)
    [ "$n" -ge "$FILES" ] && pass "offline share stays searchable ($n files)" || fail "offline share lost entries ($n)"
else
    fail "unreachable SMB server not detected: $(evos status | grep "$SMB")"
fi
ip route del blackhole "$ip/32"
t=$(now)
if wait_for 240 phase_is "$SMB" live; then
    pass "share back online after $(since "$t")s"
else
    fail "share did not come back: $(evos status | grep "$SMB")"
fi

echo "== restart from snapshots"
kill -TERM "$DAEMON"
wait "$DAEMON"
t=$(now)
start_daemon
if wait_for 60 has_hits file-0000042.dat; then
    pass "searchable $(since "$t")s after restart"
    n=$(count "under:$NFS" file:)
    [ "$n" -ge "$FILES" ] && pass "NFS index restored ($n files)" || fail "NFS index after restart: $n"
else
    fail "not searchable after restart"
fi

echo "== cleanup"
evos mount rm smb && evos mount rm nfs && pass "mounts removed"
kill -TERM "$DAEMON"
wait "$DAEMON"

echo
echo "$PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    echo "--- daemon log (tail)"
    tail -40 /tmp/e2e/evosd.log
    exit 1
fi
