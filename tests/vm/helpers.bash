# Helpers for the VM tests. These tests boot real sandboxes.
source "$(dirname "${BASH_SOURCE[0]}")/../helpers.bash"

# -- Prerequisites --

# Skip the test (or the file) if the host cannot run a VM: no KVM on Linux,
# or no docker daemon.
require_vm_support() {
    if [[ "$(uname)" == "Linux" && ! -r /dev/kvm ]]; then
        skip "no KVM access (Linux)"
    fi
    if ! command -v docker &>/dev/null; then
        skip "docker not available"
    fi
    if ! docker info &>/dev/null 2>&1; then
        skip "docker daemon not running"
    fi
}

# -- File-level setup and teardown for VM tests --

# Check the binary and VM support. Then make one temp directory for all
# tests of the file, so that the sandbox (disk, image cache link, overlay)
# stays between tests. Each test boots a new VM but uses the same sandbox
# state again. The temp directory has the home directory and the project
# side by side, because airlock refuses a project that contains the home.
# Also make one data home directory for all tests of the file, and set it
# as the data directory in the user settings. macOS ignores XDG_DATA_HOME,
# and it limits Unix socket paths to 104 bytes. The data home has a short
# path, and the sockets of the sandbox go below it.
# Call this from setup_file, then write the config.

vm_setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    mkdir -p "$TEST_TEMP_ROOT"
    FILE_TEMP_DIR="$(mktemp -d "$TEST_TEMP_ROOT/XXXXXXXX")"
    TEST_HOME="$FILE_TEMP_DIR/home"
    FILE_DATA_HOME="$(make_data_home)"
    export FILE_TEMP_DIR TEST_HOME FILE_DATA_HOME
    mkdir -p "$TEST_HOME/.airlock" "$FILE_TEMP_DIR/project"
    printf 'data_dir = "%s/airlock"\n' "$FILE_DATA_HOME" >"$TEST_HOME/.airlock/settings.toml"
    cd "$FILE_TEMP_DIR/project" || return 1
}

# Stop the host HTTP server and remove the shared temp and data home
# directories, unless AIRLOCK_TEST_KEEP=1.
teardown_file() {
    stop_host_http_server
    cd "$REPO_ROOT" || true
    if [[ -n "${FILE_DATA_HOME:-}" && -d "${FILE_DATA_HOME:-}" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$FILE_DATA_HOME"
        fi
    fi
    if [[ -n "${FILE_TEMP_DIR:-}" && -d "${FILE_TEMP_DIR:-}" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$FILE_TEMP_DIR"
        fi
    fi
}

# Start each test in the shared project directory of the file.
setup() {
    BG_PIDS=()
    cd "$FILE_TEMP_DIR/project" || return 1
}

# Serve http_root/index.html ("hello-from-host") on host port $1 until
# teardown_file. Return when the server is ready. Call from setup_file in
# $FILE_TEMP_DIR.
start_host_http_server() {
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    python3 -m http.server "$1" --directory http_root \
        >"$FILE_TEMP_DIR/http_server.log" 2>&1 &
    HTTP_PID=$!
    export HTTP_PID
    # Wait until the server accepts connections, for at most 30 seconds.
    local i
    for i in $(seq 300); do
        if (: >"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    echo "host HTTP server on port $1 did not start" >&2
    return 1
}

# Stop the server of start_host_http_server, if it runs.
stop_host_http_server() {
    if [[ -n "${HTTP_PID:-}" ]]; then
        kill "$HTTP_PID" 2>/dev/null || true
        wait "$HTTP_PID" 2>/dev/null || true
    fi
}

# -- Run a command in a VM --

# Run a command in a new VM. --quiet removes the setup logs, so $output
# contains only the output of the command. Sets $status, $output and
# $lines, as run_airlock does.
run_vm() {
    run_airlock --quiet start -- "$@"
}

# -- Sandbox sessions --

# Start airlock in the background with the environment of run_airlock.
# Write its output to the file $1, and give the other arguments to
# airlock. Set $BG_PID and add it to $BG_PIDS. Close fd 3, so that bats
# does not wait for the process.
airlock_bg() {
    local out="$1"
    shift
    local _home="${TEST_HOME:-${TEST_TEMP_DIR:-$FILE_TEMP_DIR}}"
    env XDG_DATA_HOME="${TEST_DATA_HOME:-$FILE_DATA_HOME}" \
        NO_COLOR=1 \
        HOME="$_home" \
        RUST_BACKTRACE=0 \
        "$AIRLOCK" "$@" </dev/null >"$out" 2>&1 3>&- &
    BG_PID=$!
    BG_PIDS+=("$BG_PID")
}

# Kill all background processes of airlock_bg and wait for them. Call it
# from teardown.
kill_bg() {
    local pid
    for pid in "${BG_PIDS[@]:-}"; do
        [[ -n "$pid" ]] || continue
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    BG_PIDS=()
}

# Set $BOX_DIR to the sandbox directory of the current project. The
# sandbox must exist.
find_box_dir() {
    run_airlock sandboxes info --json
    assert_success || return 1
    BOX_DIR="$(sed -n 's/^ *"dir": "\(.*\)",$/\1/p' <<<"$output")"
    [[ -d "$BOX_DIR" ]]
}

# Return 0 if a sandbox process holds run.lock in $BOX_DIR: the sandbox
# boots, runs or stops.
sandbox_running() {
    python3 -I -c '
import fcntl, sys
with open(sys.argv[1]) as f:
    try:
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        sys.exit(0)
sys.exit(1)
' "$BOX_DIR/run.lock"
}

# Return 0 if no process holds run.lock in $BOX_DIR. A test can call it
# as a check. A plain `! sandbox_running` does not fail a bats test.
sandbox_stopped() {
    ! sandbox_running
}

# Wait until the shell command $1 succeeds, for at most $2 seconds
# (default 60).
wait_until() {
    local i
    for i in $(seq $((${2:-60} * 10))); do
        if eval "$1"; then
            return 0
        fi
        sleep 0.1
    done
    echo "timeout after ${2:-60} s: $1" >&2
    return 1
}

# Print a free TCP port on the host loopback.
free_port() {
    python3 -I -c '
import socket
with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    print(s.getsockname()[1])
'
}
