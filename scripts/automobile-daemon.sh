# shellcheck shell=bash
# One private AutoMobile daemon, shared by every harness run that points at it. Source this file.
#
#   am_daemon_up <auto-mobile checkout>   join the daemon named by AM_DAEMON_ENV, or start one
#   am <tool> [args...]                   one-shot CLI call against that daemon
#   am_release <session uuid>             free one device's session
#   am_daemon_down                        stop the daemon, only if this shell started it
#
# The daemon has its own socket, PID, lock and state paths, so no other AutoMobile daemon on the
# host is touched. It runs with session-scoped plan locks, so sessions on different devices run
# concurrently. AUTOMOBILE_DAEMON_LAUNCHER_PID names the starting shell, so if that shell dies
# without stopping it, the daemon's orphan watchdog stops it after AM_ORPHAN_IDLE_MS idle.
# Android targets that AutoMobile boots itself (@avd) need ANDROID_HOME set before am_daemon_up:
# the daemon keeps the environment it started with.

am_daemon_up() {
  am_dir=$1
  am_entry="$am_dir/src/index.ts"
  [ -f "$am_entry" ] || { echo "no $am_entry: not an auto-mobile checkout" >&2; return 2; }
  if [ -n "${AM_DAEMON_ENV:-}" ]; then
    [ -f "$AM_DAEMON_ENV" ] || { echo "AM_DAEMON_ENV=$AM_DAEMON_ENV does not exist" >&2; return 2; }
    # shellcheck disable=SC1090
    . "$AM_DAEMON_ENV"
    am_owns_daemon=0
    return 0
  fi
  am_root=$(mktemp -d "${TMPDIR:-/tmp}/simee-am.XXXXXX")
  am_sock=$(mktemp -d /tmp/srs-am.XXXX) # short: a socket path must fit sockaddr_un
  export AUTOMOBILE_DATA_DIR="$am_root/data" AUTOMOBILE_LOG_DIR="$am_root/logs" \
    AUTOMOBILE_DB_DIR="$am_root/db" AUTOMOBILE_COORDINATION_DIR="$am_root/coordination" \
    AUTOMOBILE_ADB_SERVER_COORDINATION_DIR="$am_root/adb" AUTOMOBILE_AUX_SOCKET_DIR="$am_sock" \
    AUTOMOBILE_DAEMON_SOCKET_PATH="$am_sock/d.sock" AUTOMOBILE_DAEMON_PID_FILE_PATH="$am_root/daemon.pid" \
    AUTOMOBILE_DAEMON_LOCK_FILE_PATH="$am_root/daemon.lock" \
    AUTOMOBILE_DAEMON_TOKEN_FILE_PATH="$am_root/daemon.token" \
    AUTOMOBILE_DAEMON_LAUNCH_LOG_PATH="$am_root/launch.log" \
    AUTOMOBILE_DAEMON_LAUNCHER_PID=$$ \
    AUTOMOBILE_PRIVATE_DAEMON_ORPHAN_IDLE_MS="${AM_ORPHAN_IDLE_MS:-120000}" \
    AUTOMOBILE_APPEARANCE_SYNC=off AUTOMOBILE_PROJECT_ROOT="$am_dir"
  mkdir -p "$AUTOMOBILE_DATA_DIR" "$AUTOMOBILE_LOG_DIR" "$AUTOMOBILE_DB_DIR" \
    "$AUTOMOBILE_COORDINATION_DIR" "$AUTOMOBILE_ADB_SERVER_COORDINATION_DIR"
  # Other harness runs join this daemon by sourcing the file AM_DAEMON_ENV names.
  {
    for v in AUTOMOBILE_DATA_DIR AUTOMOBILE_LOG_DIR AUTOMOBILE_DB_DIR AUTOMOBILE_COORDINATION_DIR \
      AUTOMOBILE_ADB_SERVER_COORDINATION_DIR AUTOMOBILE_AUX_SOCKET_DIR AUTOMOBILE_DAEMON_SOCKET_PATH \
      AUTOMOBILE_DAEMON_PID_FILE_PATH AUTOMOBILE_DAEMON_LOCK_FILE_PATH AUTOMOBILE_DAEMON_TOKEN_FILE_PATH \
      AUTOMOBILE_DAEMON_LAUNCH_LOG_PATH AUTOMOBILE_APPEARANCE_SYNC AUTOMOBILE_PROJECT_ROOT; do
      printf 'export %s=%q\n' "$v" "${!v}"
    done
    printf 'am_root=%q am_sock=%q\n' "$am_root" "$am_sock"
  } > "$am_root/env.sh"
  am_owns_daemon=1
  if ! (cd "$am_root" && bun "$am_entry" --daemon start --plan-execution-lock-scope session) \
    > "$am_root/start.log" 2>&1; then
    tail -20 "$am_root/start.log" >&2
    return 1
  fi
}

am() { (cd "$am_root" && bun "$am_entry" --cli "$@"); }

am_release() {
  [ -n "${1:-}" ] || return 0
  (cd "$am_root" && bun "$am_entry" --daemon release-session "$1") > /dev/null 2>&1 || true
}

# The daemon's recorded PID: the PID file is JSON ({"pid": N, ...}), or a bare number from older
# daemons.
am_daemon_pid() {
  local f=$AUTOMOBILE_DAEMON_PID_FILE_PATH pid
  [ -f "$f" ] || return 0
  pid=$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$f" | head -1)
  if [ -z "$pid" ] && grep -Eqx '[[:space:]]*[0-9]+[[:space:]]*' "$f"; then
    pid=$(tr -dc 0-9 < "$f")
  fi
  echo "$pid"
}

am_daemon_down() {
  [ "${am_owns_daemon:-0}" = 1 ] || return 0
  am_owns_daemon=0
  local pid
  pid=$(am_daemon_pid)
  # Stop through its own socket first (same environment), then by its recorded PID.
  (cd "$am_root" && bun "$am_entry" --daemon stop) < /dev/null >> "$am_root/daemon-stop.log" 2>&1 || true
  if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then
    kill "$pid" 2> /dev/null || true
    echo "daemon $pid didn't stop on request; sent SIGTERM" >&2
  fi
  rm -rf "$am_sock"
}
