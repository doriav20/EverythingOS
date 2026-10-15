# Network shares end to end against real SMB and NFS servers (the samba
# and nfs services of the compose test profile).
#
# The share contents live in a volume that is also mounted here at
# /server, so "server-side" changes (made directly on the server's disk)
# can be distinguished from changes made by another client through SMB.

FILES=${E2E_FILES:-100000}
DIRS=${E2E_DIRS:-5000}
RECHECK=${E2E_RECHECK:-5}

# populate ROOT: FILES files in DIRS folders, three levels deep, plus
# names in several scripts and a few files with known content.
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
    mkdir -p "$root/unicode/תיקייה"
    echo "תוכן בעברית על השיתוף" >"$root/unicode/שלום עולם.txt"
    echo "remote content marker" >"$root/unicode/תיקייה/Résumé.txt"
    : >"$root/unicode/Ελληνικά.txt"
    : >"$root/unicode/日本語ファイル.txt"
}

# unicode_checks ROOT LABEL: names in other scripts and content on a share.
unicode_checks() {
    local root=$1 label=$2
    local n
    n=$(count "under:$root" 'שלום עולם')
    [ "$n" -eq 1 ] && pass "$label: Hebrew name found" || fail "$label: Hebrew name ($n)"
    n=$(count "under:$root" 'שָׁלוֹם')
    [ "$n" -eq 1 ] && pass "$label: Hebrew search with niqqud" || fail "$label: Hebrew with niqqud ($n)"
    n=$(count "under:$root" 'תיקייה/resume')
    [ "$n" -eq 1 ] && pass "$label: Hebrew folder and diacritics in a path search" ||
        fail "$label: Hebrew path search ($n)"
    n=$(count "under:$root" 'ελληνικα')
    [ "$n" -eq 1 ] && pass "$label: Greek name, case and accents ignored" || fail "$label: Greek ($n)"
    n=$(count "under:$root" '日本語')
    [ "$n" -eq 1 ] && pass "$label: Japanese name" || fail "$label: Japanese ($n)"
    n=$(count "under:$root" 'content:"בעברית"')
    [ "$n" -eq 1 ] && pass "$label: Hebrew content read over the network" ||
        fail "$label: Hebrew content ($n)"
    n=$(count "under:$root" 'content:"remote content marker"')
    [ "$n" -eq 1 ] && pass "$label: content search over the network" || fail "$label: content ($n)"
}

run_network() {
    export EVOS_SOCKET=/run/evos-e2e/evosd.sock
    section "generating $FILES files in $DIRS folders per share"
    local t
    t=$(now)
    populate /server/smb
    populate /server/nfs
    info "generated in $(since "$t")s"

    # Index only what this suite mounts, whatever else the container has.
    mkdir -p /tmp/e2e /run/evos-e2e
    local skip
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
    start_daemon || { fail "daemon did not start"; cat /tmp/e2e/evosd.log; return; }

    SMB=/mnt/evos/smb
    NFS=/mnt/evos/nfs
    local DEEP=l07/m107/d00107

    section "SMB share"
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
    local n
    n=$(count "under:$SMB" file: !under:$SMB/unicode)
    [ "$n" -eq "$FILES" ] && pass "all $FILES SMB files indexed" || fail "SMB file count $n != $FILES"
    info "status: $(evos status | grep "$SMB")"
    unicode_checks "$SMB" SMB

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
    echo x >"/server/smb/l13/m213/d01213/server-made/נוצר בשרת.txt"
    if wait_for 180 has_hits 'נוצר בשרת'; then
        pass "Hebrew-named file created on the server's disk found after $(since "$t")s"
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
    fi

    if want ui; then
        section "a share in the desktop client"
        stop_ui
        if start_ui; then
            run_ui_test share "$SMB" "שלום עולם.txt"
            run_ui_test share-mount //samba/share evos evos "שלום עולם.txt"
        else
            fail "UI did not start"
        fi
        stop_ui
    fi

    section "NFS export"
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
    n=$(count "under:$NFS" file: !under:$NFS/unicode)
    [ "$n" -eq "$FILES" ] && pass "all $FILES NFS files indexed" || fail "NFS file count $n != $FILES"
    info "status: $(evos status | grep "$NFS")"
    unicode_checks "$NFS" NFS

    t=$(now)
    touch /server/nfs/l21/m321/d00321/nfs-server-side.txt
    if wait_for 180 has_hits nfs-server-side.txt; then
        pass "NFS server-side change found after $(since "$t")s (scheduled sweeps)"
    else
        fail "NFS server-side change not found"
    fi

    section "adaptive concurrency under 5 ms latency"
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

    section "offline share"
    local ip
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

    section "restart from snapshots"
    kill -TERM "$DAEMON"
    wait "$DAEMON"
    t=$(now)
    start_daemon
    if wait_for 60 has_hits file-0000042.dat; then
        pass "searchable $(since "$t")s after restart"
        n=$(count "under:$NFS" file:)
        [ "$n" -ge "$FILES" ] && pass "NFS index restored ($n files)" || fail "NFS index after restart: $n"
        has_hits 'שלום עולם' && pass "Hebrew names survive the snapshot" || fail "Hebrew names lost on restart"
    else
        fail "not searchable after restart"
    fi

    section "cleanup"
    evos mount rm smb && evos mount rm nfs && pass "mounts removed"
    kill -TERM "$DAEMON"
    wait "$DAEMON"
    if [ "$FAIL" -gt 0 ]; then
        echo "--- network daemon log (tail)"
        tail -40 /tmp/e2e/evosd.log
    fi
}
