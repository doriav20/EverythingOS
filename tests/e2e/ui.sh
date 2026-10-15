# The desktop client end to end: the real evos-gtk window runs on Xvfb and
# is driven and read through the accessibility bus (AT-SPI), its D-Bus
# actions and its command line, the way assistive technology and scripts
# use it. Sourced by run.sh.

UI_DIR=/tmp/e2e-local/ui

start_ui() {
    mkdir -p "$UI_DIR/config"
    export DISPLAY=:99 GDK_BACKEND=x11 XDG_CONFIG_HOME="$UI_DIR/config" NO_AT_BRIDGE=0
    Xvfb :99 -screen 0 1400x900x24 -nolisten tcp >"$UI_DIR/xvfb.log" 2>&1 &
    XVFB_PID=$!
    wait_for 10 xdotool getdisplaygeometry || return 1
    eval "$(dbus-launch --sh-syntax)"
    /usr/libexec/at-spi-bus-launcher --launch-immediately >"$UI_DIR/atspi.log" 2>&1 &
    sleep 1
    evos-gtk >"$UI_DIR/gtk.log" 2>&1 &
    GTK_PID=$!
}

stop_ui() {
    evos-gtk -exit >/dev/null 2>&1
    sleep 1
    kill "$GTK_PID" "$XVFB_PID" 2>/dev/null
    [ -n "${DBUS_SESSION_BUS_PID:-}" ] && kill "$DBUS_SESSION_BUS_PID" 2>/dev/null
    wait "$GTK_PID" "$XVFB_PID" 2>/dev/null
}

# run_ui_test ARGS...: runs the AT-SPI test and counts its results. A test
# that dies or checks nothing is a failure, not a quiet success.
run_ui_test() {
    local line checks=0 failed_before=$FAIL status
    python3 tests/e2e/ui_test.py "$@" >"$UI_DIR/test.out" 2>&1
    status=$?
    while IFS= read -r line; do
        echo "$line"
        case $line in
        "PASS "*) PASS=$((PASS + 1)) checks=$((checks + 1)) ;;
        "FAIL "*) FAIL=$((FAIL + 1)) checks=$((checks + 1)) ;;
        esac
    done <"$UI_DIR/test.out"
    if [ "$checks" -eq 0 ] || { [ "$status" -ne 0 ] && [ "$FAIL" -eq "$failed_before" ]; }; then
        fail "ui_test.py $1 exited with status $status after $checks checks"
    fi
}
