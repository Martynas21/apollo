#!/usr/bin/env bash
# Runs worker, apollo and tailscale funnel as windows in the current tmux
# session, so they live as long as the session does. A window whose process
# exits stays open (remain-on-exit) so its last output can be read.
#
#   ./run-native.sh [start]   build, then open the three windows
#   ./run-native.sh stop      Ctrl-C each process, then close its window
#   ./run-native.sh restart   build, then stop and start
set -euo pipefail

cd "$(dirname "$0")"

if [ -z "${TMUX:-}" ]; then
    echo "run this from inside a tmux session" >&2
    exit 1
fi

# Both binaries load .env themselves; the script only needs the addresses to
# wait for the worker and to point the funnel at the dashboard.
env_value() {
    [ -f .env ] || return 0
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" .env | tail -n 1 |
        sed -e 's/[[:space:]]*$//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\(.*\)'$/\1/"
}

require_env_value() {
    local value
    value="$(env_value "$1")"
    if [ -z "$value" ]; then
        echo "set $1 (host:port) in .env" >&2
        exit 1
    fi
    printf '%s\n' "$value"
}

worker_socket="$(require_env_value AUDIO_WORKER_SOCKET)"
worker_host="${worker_socket%:*}"
worker_port="${worker_socket##*:}"
dashboard_addr="$(require_env_value DASHBOARD_BIND_ADDR)"
dashboard_port="${dashboard_addr##*:}"

window_exists() {
    tmux list-windows -F '#{window_name}' | grep -qx "$1"
}

open_window() {
    local name="$1" cmd="$2"
    tmux new-window -d -a -t ':{end}' -n "$name" -c "$PWD" "$cmd" \; \
        set-window-option -t ":=$name" remain-on-exit on >/dev/null
}

close_window() {
    local name="$1"
    window_exists "$name" || return 0
    tmux send-keys -t ":=$name" C-c
    for _ in $(seq 1 50); do
        [ "$(tmux display-message -p -t ":=$name" '#{pane_dead}')" = 1 ] && break
        sleep 0.1
    done
    tmux kill-window -t ":=$name"
}

start() {
    cargo build --release --workspace
    for name in worker apollo funnel; do
        close_window "$name"
    done

    open_window worker ./target/release/apollo-audio-worker
    for _ in $(seq 1 50); do
        (exec 3<>"/dev/tcp/$worker_host/$worker_port") 2>/dev/null && break
        sleep 0.1
    done

    open_window apollo ./target/release/apollo
    open_window funnel "tailscale funnel $dashboard_port"
    echo "started windows: worker, apollo, funnel"
}

stop() {
    for name in funnel apollo worker; do
        close_window "$name"
    done
    echo "stopped"
}

case "${1:-start}" in
    start) start ;;
    stop) stop ;;
    restart) cargo build --release --workspace && stop && start ;;
    *)
        echo "usage: $0 [start|stop|restart]" >&2
        exit 1
        ;;
esac
