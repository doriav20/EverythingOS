# Helpers shared by the end-to-end suites (sourced, not run).

PASS=${PASS:-0}
FAIL=${FAIL:-0}
pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }
info() { echo "      $*"; }
section() { echo; echo "== $*"; }
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
has_hits() { [ "$(count "$@")" -ge 1 ]; }
no_hits() { [ "$(count "$@")" -eq 0 ]; }
status_field() { evos status 2>/dev/null | awk -v root="$1" -v col="$2" '$1 == root { print $col }'; }
phase_is() { [ "$(status_field "$1" 3)" = "$2" ]; }

# expect N QUERY...: the search finds exactly N results.
expect() {
    local want=$1
    shift
    local got
    got=$(count "$@")
    if [ "$got" -eq "$want" ]; then
        pass "$* → $want"
    else
        fail "$* → $got, expected $want"
        info "$(evos "$@" 2>&1 | head -5)"
    fi
}

# expect_names "NAME..." QUERY...: the search finds exactly these names
# (in this order, as sorted by the query's options).
expect_names() {
    local want=$1
    shift
    local got
    got=$(evos --name "$@" 2>/dev/null | tr '\n' '|' | sed 's/|$//')
    if [ "$got" = "$want" ]; then
        pass "$* → $want"
    else
        fail "$* → [$got], expected [$want]"
    fi
}

# skip_all_mounts_but PATH...: a skip_mounts list for evosd.toml that
# leaves only the given mount points to be indexed.
skip_all_mounts_but() {
    awk -v keep="$*" 'BEGIN { n = split(keep, k, " "); for (i = 1; i <= n; i++) want[k[i]] = 1 }
        !($5 in want) { printf "\"%s\", ", $5 }' /proc/self/mountinfo
}

# stat_snapshot LIST: access, modification, change and birth times (with
# nanoseconds) and size of every path in the NUL-separated file LIST.
# Only stat(2) is used: listing directories would itself update their
# access times.
stat_snapshot() {
    xargs -0 stat -c '%n|%x|%y|%z|%w|%s' <"$1"
}
