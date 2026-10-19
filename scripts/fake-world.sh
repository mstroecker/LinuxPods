#!/bin/sh
# Runs LinuxPods against the fake world of the test harness (#72): fakepods
# serves a fake BlueZ and fake AirPods on a private bus that stands in for the
# system bus, and the app runs on a second private bus as its session bus. The
# real bluetoothd, a running LinuxPods and its tray never see any of it.
#
#   scripts/fake-world.sh start [SCENARIO]       in-range (default), connected,
#                                                two-pairs, flood or empty
#   scripts/fake-world.sh stop
#   scripts/fake-world.sh world METHOD [ARG...]  calls FakeWorld1
#   scripts/fake-world.sh app METHOD [ARG...]    calls the app's Automation1
#   scripts/fake-world.sh tree                   prints Automation1.Tree's JSON
#
# HEADLESS=1 puts the app on its own Broadway display, to watch in a browser,
# instead of the desktop. `make fake-world` builds the binaries first; this
# runs them as they are, from ${CARGO_TARGET_DIR:-target}/debug.
#
# Everything lives in $XDG_RUNTIME_DIR/linuxpods-harness/world: the buses'
# sockets, the AAP sockets, the app's data and config directories, the logs,
# and `env`, which holds the bus addresses for gdbus and busctl:
#   . "$XDG_RUNTIME_DIR/linuxpods-harness/world/env"
#   gdbus call --address "$SYSTEM_BUS" ...   # fake BlueZ, FakeWorld1
#   gdbus call --address "$SESSION_BUS" ...  # the app, Automation1
# The paths stay short: a Unix socket path over 108 bytes cannot be bound.
set -eu

cd "$(dirname "$0")/.."
bin=${CARGO_TARGET_DIR:-target}/debug
base=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/linuxpods-harness
run=$base/world
pids=$run/pids

APP_NAME=io.github.mstroecker.LinuxPods
AUTOMATION_PATH=/io/github/mstroecker/LinuxPods/Automation
AUTOMATION=io.github.mstroecker.LinuxPods.Automation1
WORLD_NAME=io.github.mstroecker.LinuxPods.FakeWorld
WORLD_PATH=/io/github/mstroecker/LinuxPods/FakeWorld
WORLD=io.github.mstroecker.LinuxPods.FakeWorld1

die() {
    echo "fake-world: $*" >&2
    exit 1
}

# Starts a private bus: session-type, open to this user, and with no service
# directory, so nothing is ever activated on it - no portal, dconf or
# accessibility bus. dbus-daemon forks into its own session; its PID is kept.
start_bus() {
    mkdir -p "$run/$1"
    cat > "$run/$1.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=$run/$1</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
    dbus-daemon --config-file="$run/$1.conf" --fork --print-address=3 --print-pid=4 \
        3>"$run/$1.address" 4>"$run/$1.pid"
    echo "$(cat "$run/$1.pid") dbus-daemon" >> "$pids"
}

# Runs a command in a session of its own, in the background, and keeps its
# PID. setsid execs it in place, so the PID is the command's.
spawn() {
    name=$1
    log=$2
    shift 2
    setsid "$@" > "$log" 2>&1 < /dev/null &
    echo "$! $name" >> "$pids"
}

# Whether the bus at $1 has a name $2.
has_name() {
    gdbus call --address "$1" --dest org.freedesktop.DBus \
        --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.NameHasOwner "$2" 2>/dev/null | grep -q true
}

# Whether the process we started as $2 is still running at PID $1.
ours() {
    [ "$(cat "/proc/$1/comm" 2>/dev/null)" = "$2" ]
}

# The first Broadway display whose port, 8080 plus its number, is free and
# which no running broadwayd serves.
free_display() {
    n=5
    while [ "$n" -lt 50 ]; do
        if ! ss -Hltn "sport = :$((8080 + n))" | grep -q . &&
            ! pgrep -f "broadwayd.* :$n\$" > /dev/null; then
            echo "$n"
            return
        fi
        n=$((n + 1))
    done
    die "no free Broadway display"
}

start() {
    scenario=${1:-in-range}
    for b in fakepods linuxpods; do
        [ -x "$bin/$b" ] || die "$bin/$b is missing: build with cargo build --features harness, or run make fake-world"
    done
    [ ! -e "$pids" ] || die "a fake world is running already, or was not stopped: $0 stop"
    rm -rf "$run"
    mkdir -p -m 700 "$run"
    mkdir -p "$run/aap" "$run/log" "$run/home/data" "$run/home/config"
    : > "$pids"
    # A start that fails part way stops what it started.
    trap 'stop > /dev/null 2>&1 || true' EXIT

    start_bus system
    start_bus session
    {
        echo "SYSTEM_BUS='$(cat "$run/system.address")'"
        echo "SESSION_BUS='$(cat "$run/session.address")'"
        echo "AAP_SOCKET_DIR='$run/aap'"
        echo "SCENARIO='$scenario'"
    } > "$run/env"
    load_env

    # Every process here gets the private buses, so none can reach the real
    # ones by default - and plain logs, as they go to files.
    common="DBUS_SYSTEM_BUS_ADDRESS=$SYSTEM_BUS DBUS_SESSION_BUS_ADDRESS=$SESSION_BUS NO_COLOR=1"

    # shellcheck disable=SC2086 # $common is a list of assignments
    spawn fakepods "$run/log/fakepods.log" env $common \
        RUST_LOG="${RUST_LOG:-linuxpods=debug,fakepods=debug}" \
        "$bin/fakepods" --address "$SYSTEM_BUS" --socket-dir "$run/aap" \
        --scenario "$scenario" --keystore "$run/home/data/linuxpods"
    fakepods=$(tail -n 1 "$pids" | cut -d' ' -f1)
    i=0
    until has_name "$SYSTEM_BUS" "$WORLD_NAME"; do
        ours "$fakepods" fakepods || die "fakepods exited: $(tail -n 5 "$run/log/fakepods.log")"
        i=$((i + 1))
        [ "$i" -lt 100 ] || die "fakepods did not come up: $run/log/fakepods.log"
        sleep 0.1
    done

    display=
    if [ "${HEADLESS:-}" = 1 ]; then
        n=$(free_display)
        port=$((8080 + n))
        spawn gtk4-broadwayd "$run/log/broadwayd.log" \
            gtk4-broadwayd --address 127.0.0.1 --port "$port" ":$n"
        # broadwayd :n listens at broadway<n+1>.socket.
        i=0
        until [ -S "$XDG_RUNTIME_DIR/broadway$((n + 1)).socket" ]; do
            i=$((i + 1))
            [ "$i" -lt 50 ] || die "gtk4-broadwayd did not come up: $run/log/broadwayd.log"
            sleep 0.1
        done
        display="-u WAYLAND_DISPLAY -u DISPLAY GDK_BACKEND=broadway BROADWAY_DISPLAY=:$n"
        {
            echo "BROADWAY_DISPLAY=$n"
            echo "BROADWAY_URL='http://127.0.0.1:$port'"
        } >> "$run/env"
    fi

    # Automation1 is asked for here and nowhere else: on this private session
    # bus, nothing of the user's can press the window.
    # shellcheck disable=SC2086 # $display and $common are lists of arguments
    spawn linuxpods "$run/log/linuxpods.log" env $display $common \
        XDG_DATA_HOME="$run/home/data" XDG_CONFIG_HOME="$run/home/config" \
        GSETTINGS_BACKEND=memory GTK_A11Y=none NO_AT_BRIDGE=1 GTK_USE_PORTAL=0 \
        LINUXPODS_AAP_SOCKET_DIR="$run/aap" LINUXPODS_HARNESS_AUTOMATION=1 \
        RUST_LOG="${RUST_LOG:-linuxpods=debug}" \
        "$bin/linuxpods"
    app=$(tail -n 1 "$pids" | cut -d' ' -f1)

    # GApplication takes its name before Automation1 is registered, so the
    # first call is retried until it is.
    i=0
    until call_app Settle 5000 > /dev/null 2>&1; do
        ours "$app" linuxpods || die "the app exited: $(tail -n 5 "$run/log/linuxpods.log")"
        i=$((i + 1))
        [ "$i" -lt 150 ] || die "the app did not come up: $run/log/linuxpods.log"
        sleep 0.2
    done
    trap - EXIT

    echo "Fake world '$scenario' is running."
    echo "  system bus:  $SYSTEM_BUS"
    echo "               fake BlueZ, and $WORLD_NAME at $WORLD_PATH"
    echo "  session bus: $SESSION_BUS"
    echo "               the app, and Automation1 at $AUTOMATION_PATH"
    [ -z "$display" ] || echo "  watch:       http://127.0.0.1:$port"
    echo "  logs:        $run/log/"
    echo "  addresses:   . $run/env"
    echo "  change it:   $0 world SetBattery 00:00:5E:00:53:01 left 42"
    echo "  drive it:    $0 app Tree"
    echo "  stop it:     make fake-world-stop"
}

# Stops what start started, by the PIDs it kept, and nothing else.
stop() {
    if [ ! -f "$pids" ]; then
        echo "No fake world is running."
        return 0
    fi
    # The app first and the buses last, the reverse of how they started.
    tac "$pids" | while read -r pid name; do
        if ours "$pid" "$name"; then kill "$pid" 2> /dev/null || true; fi
    done
    i=0
    while [ "$i" -lt 50 ]; do
        left=0
        while read -r pid name; do
            if ours "$pid" "$name"; then left=1; fi
        done < "$pids"
        [ "$left" = 1 ] || break
        i=$((i + 1))
        sleep 0.1
    done
    while read -r pid name; do
        if ours "$pid" "$name"; then kill -9 "$pid" 2> /dev/null || true; fi
    done < "$pids"

    # broadwayd leaves its socket behind.
    n=$(sed -n 's/^BROADWAY_DISPLAY=//p' "$run/env" 2> /dev/null || true)
    [ -z "$n" ] || rm -f "$XDG_RUNTIME_DIR/broadway$((n + 1)).socket"
    # The logs stay until the next start.
    find "$run" -mindepth 1 -maxdepth 1 ! -name log -exec rm -rf {} +
    echo "Fake world stopped. Logs: $run/log/"
}

load_env() {
    [ -f "$run/env" ] || die "no fake world is running: $0 start"
    # shellcheck disable=SC1091 # written by start
    . "$run/env"
}

call_world() {
    method=$1
    shift
    gdbus call --address "$SYSTEM_BUS" --dest "$WORLD_NAME" --object-path "$WORLD_PATH" \
        --method "$WORLD.$method" "$@"
}

call_app() {
    method=$1
    shift
    gdbus call --address "$SESSION_BUS" --dest "$APP_NAME" --object-path "$AUTOMATION_PATH" \
        --method "$AUTOMATION.$method" "$@"
}

case "${1:-}" in
    start)
        shift
        start "$@"
        ;;
    stop) stop ;;
    world)
        [ $# -ge 2 ] || die "usage: $0 world METHOD [ARG...]"
        shift
        load_env
        call_world "$@"
        ;;
    app)
        [ $# -ge 2 ] || die "usage: $0 app METHOD [ARG...]"
        shift
        load_env
        call_app "$@"
        ;;
    tree)
        # gdbus prints the JSON escaped inside GVariant text; busctl's JSON
        # output carries it as a plain string.
        load_env
        busctl --address="$SESSION_BUS" --json=short call \
            "$APP_NAME" "$AUTOMATION_PATH" "$AUTOMATION" Tree |
            python3 -I -c 'import json, sys; print(json.load(sys.stdin)["data"][0])'
        ;;
    *) die "usage: $0 start [SCENARIO] | stop | world METHOD [ARG...] | app METHOD [ARG...] | tree" ;;
esac
