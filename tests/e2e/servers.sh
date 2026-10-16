# Everything's network features end to end: the ETP/FTP, HTTP and index
# servers, used by clients that share no code with them (curl, Python's
# ftplib and an ETP client written from the protocol), and remote indexes
# copied from them. Nothing can be changed through any of them; the
# read-only suite afterwards checks that downloads did not even touch
# access times. Sourced by run.sh; uses the local daemon and volumes.

ETP_PORT=2121
HTTP_PORT=8080
INDEX_PORT=14630

# run_py_checks SCRIPT ARGS...: runs a Python check script and counts its
# PASS/FAIL lines; a script that dies or checks nothing fails.
run_py_checks() {
    local out line checks=0 status before=$FAIL
    out=$(python3 "$@" 2>&1)
    status=$?
    while IFS= read -r line; do
        echo "$line"
        case $line in
        "PASS "*) PASS=$((PASS + 1)) checks=$((checks + 1)) ;;
        "FAIL "*) FAIL=$((FAIL + 1)) checks=$((checks + 1)) ;;
        esac
    done <<<"$out"
    if [ "$checks" -eq 0 ] || { [ "$status" -ne 0 ] && [ "$FAIL" -eq "$before" ]; }; then
        fail "$1 exited with status $status after $checks checks"
    fi
}

# urlpath PATH: a path percent-encoded for a URL.
urlpath() {
    python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$1"
}

# The read-only suite's strictatime volume, if it runs too.
have_ro() { [ -f "$RO/dir/note.txt" ]; }

port_open() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

tree_state() {
    (cd "$DISK" && find . -path ./bulk -prune -o -printf '%p %s %T@ %m\n' | sort)
}

run_servers() {
    section "network servers: settings"
    local before
    before=$(tree_state)
    if printf 'e2e-pass\n' | evos server enable etp --port $ETP_PORT --bind 127.0.0.1 \
        --user e2e --password-stdin --run-as nobody >/dev/null; then
        pass "evos server enable etp"
    else
        fail "evos server enable etp"
    fi
    printf 'web-pass\n' | evos server enable http --port $HTTP_PORT --bind 127.0.0.1 \
        --user web --password-stdin >/dev/null || fail "evos server enable http"
    printf 'idx-pass\n' | evos server add-user laptop --password-stdin >/dev/null ||
        fail "evos server add-user"
    evos server enable index --port $INDEX_PORT --bind 127.0.0.1 >/dev/null ||
        fail "evos server enable index"
    evos server status | grep -Eq '^etp +on +127\.0\.0\.1:2121 +e2e, password +nobody' &&
        pass "evos server status shows the ETP server" ||
        fail "evos server status: $(evos server status)"
    local file="$LOCAL/etc/servers.toml"
    [ "$(stat -c %a "$file")" = 600 ] && pass "server settings are readable by root only" ||
        fail "servers.toml mode $(stat -c %a "$file")"
    ! grep -q 'e2e-pass\|web-pass\|idx-pass' "$file" && grep -q 'sha256\$' "$file" &&
        pass "passwords are stored as salted hashes" || fail "plain passwords in servers.toml"
    wait_for 5 port_open $ETP_PORT && wait_for 5 port_open $HTTP_PORT &&
        wait_for 5 port_open $INDEX_PORT && pass "the three servers listen" ||
        fail "a server port is closed"
    # Unprivileged users may look but not change.
    if command -v runuser >/dev/null; then
        runuser -u nobody -- evos server status >/dev/null 2>&1 &&
            pass "anyone may see the settings" || fail "server status as nobody"
        runuser -u nobody -- evos server disable etp >/dev/null 2>&1 &&
            fail "nobody turned the ETP server off" ||
            pass "changing servers needs administrator rights"
    fi

    section "ETP/FTP server"
    local etp="e2e:e2e-pass@127.0.0.1:$ETP_PORT"
    expect_names "q1 report.pdf|q2 report.pdf" --etp "$etp" report
    expect_names "שָׁלוֹם.txt" --etp "$etp" שלום
    expect 0 --etp "$etp" plans
    expect 0 --etp "$etp" "under:/net"
    evos --etp "e2e:wrong@127.0.0.1:$ETP_PORT" report >/dev/null 2>&1 &&
        fail "a wrong ETP password was accepted" || pass "a wrong ETP password is refused"
    run_py_checks tests/e2e/etp_client.py 127.0.0.1 $ETP_PORT e2e e2e-pass "$DISK"
    local got
    got=$(curl -sS --user e2e:e2e-pass "ftp://127.0.0.1:$ETP_PORT$(urlpath "$DISK/docs/notes.md")")
    [ "$got" = "$(cat "$DISK/docs/notes.md")" ] && pass "curl downloads over FTP" ||
        fail "curl FTP download: $got"
    if have_ro; then
        got=$(curl -sS --user e2e:e2e-pass "ftp://127.0.0.1:$ETP_PORT$(urlpath "$RO/dir/note.txt")")
        [ "$got" = "needle in a haystack" ] && pass "FTP download from the strictatime volume" ||
            fail "FTP download from $RO: $got"
    fi
    curl -sS --user e2e:e2e-pass --list-only "ftp://127.0.0.1:$ETP_PORT$DISK/docs/" |
        grep -qx "q1 report.pdf" && pass "curl lists a folder" || fail "curl FTP listing"
    echo upload >"$LOCAL/upload.txt"
    curl -sS --user e2e:e2e-pass -T "$LOCAL/upload.txt" \
        "ftp://127.0.0.1:$ETP_PORT$DISK/docs/" >/dev/null 2>&1 &&
        fail "curl uploaded a file" || pass "uploads are refused"
    curl -sS --user e2e:e2e-pass -Q "DELE $DISK/docs/notes.md" \
        "ftp://127.0.0.1:$ETP_PORT/" >/dev/null 2>&1 &&
        fail "curl deleted a file" || pass "deleting is refused"
    curl -sS --user e2e:e2e-pass "ftp://127.0.0.1:$ETP_PORT$DISK/private/plans.txt" \
        >/dev/null 2>&1 && fail "a private file was sent" ||
        pass "files the server's account may not read are not sent"

    section "HTTP server"
    local http="http://127.0.0.1:$HTTP_PORT"
    [ "$(curl -s -o /dev/null -w '%{http_code}' "$http/?search=report")" = 401 ] &&
        pass "the HTTP server asks for its login" || fail "HTTP without a login"
    got=$(curl -sS -u web:web-pass "$http/?search=report&json=1&path_column=1&size_column=1")
    echo "$got" | grep -q '"totalResults":2' && echo "$got" | grep -q '"name":"q1 report.pdf"' &&
        pass "Everything's JSON interface" || fail "HTTP JSON: $got"
    got=$(curl -sS -u web:web-pass "$http/?search=$(urlpath שלום)&json=1")
    echo "$got" | grep -q 'שָׁלוֹם.txt' && pass "a Hebrew search over HTTP" || fail "HTTP Hebrew: $got"
    curl -sS -u web:web-pass "$http/?search=report" | grep -q 'q1 <b>report</b>.pdf' &&
        pass "the search page highlights the terms" || fail "HTTP search page"
    got=$(curl -sS -u web:web-pass "$http$(urlpath "$DISK/עברית/שָׁלוֹם.txt")")
    [ "$got" = "שלום עולם" ] && pass "an HTTP download" || fail "HTTP download: $got"
    if have_ro; then
        got=$(curl -sS -u web:web-pass -r 0-5 "$http$(urlpath "$RO/dir/note.txt")")
        [ "$got" = "needle" ] && pass "an HTTP range from the strictatime volume" ||
            fail "HTTP range: $got"
    fi
    for method in PUT DELETE POST; do
        code=$(curl -s -o /dev/null -w '%{http_code}' -u web:web-pass -X $method \
            --data x "$http$(urlpath "$DISK/docs/notes.md")")
        case $code in 2*) fail "HTTP $method answered $code" ;; *) pass "HTTP $method is refused ($code)" ;; esac
    done
    [ "$(tree_state)" = "$before" ] && pass "nothing changed on disk" ||
        fail "the servers changed files: $(diff <(echo "$before") <(tree_state) | head -5)"

    section "remote indexes"
    printf 'idx-pass\n' | evos remote add office 127.0.0.1 --port $INDEX_PORT --user laptop \
        --password-stdin --refresh 1 >/dev/null && pass "evos remote add" || fail "evos remote add"
    wait_for 30 has_hits "under:/net/office$DISK" '"q1 report.pdf"' &&
        pass "the index server's index is copied" || fail "no copy under /net/office"
    expect 1 "under:/net/office$DISK" "שָׁלוֹם.txt"
    expect 0 "under:/net/office" plans.txt
    have_ro && expect 1 "under:/net/office$RO" note.txt
    touch "$DISK/docs/remote-live.txt"
    wait_for 20 has_hits "under:/net/office" remote-live.txt &&
        pass "a new file reaches the copy" || fail "the copy did not get the new file"
    rm "$DISK/docs/remote-live.txt"
    wait_for 20 no_hits "under:/net/office" remote-live.txt &&
        pass "a deleted file leaves the copy" || fail "the copy kept a deleted file"
    evos remote list | grep -Eq '^office +evos +127\.0\.0\.1:14630 +/net/office +live' &&
        pass "evos remote list" || fail "evos remote list: $(evos remote list)"
    printf 'e2e-pass\n' | evos remote add winpc 127.0.0.1 --etp --port $ETP_PORT --user e2e \
        --password-stdin --refresh 2 >/dev/null || fail "evos remote add --etp"
    wait_for 60 has_hits "under:/net/winpc$DISK" '"q2 report.pdf"' &&
        pass "an ETP server's index is copied" || fail "no copy under /net/winpc"
    printf 'nope\n' | evos remote test bad 127.0.0.1 --port $INDEX_PORT --user laptop \
        --password-stdin >/dev/null 2>&1 && fail "remote test with a wrong password" ||
        pass "evos remote test reports a wrong password"
    evos remote rm office && evos remote rm winpc
    wait_for 20 no_hits "under:/net" && pass "removed remote indexes leave the index" ||
        fail "copies left under /net"

    evos server disable etp >/dev/null && evos server disable http >/dev/null &&
        evos server disable index >/dev/null
    wait_for 5 sh -c "! (exec 3<>/dev/tcp/127.0.0.1/$ETP_PORT)" 2>/dev/null &&
        pass "a disabled server stops listening" || fail "port $ETP_PORT still open"
}
