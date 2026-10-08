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
# tests of the file, so that the .airlock/ sandbox (disk, image cache link,
# overlay) stays between tests. Each test boots a new VM but uses the same
# sandbox state again. Call this from setup_file, then write the config.

vm_setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
    require_vm_support
    mkdir -p "$TEST_TEMP_ROOT"
    FILE_TEMP_DIR="$(mktemp -d "$TEST_TEMP_ROOT/XXXXXXXX")"
    export FILE_TEMP_DIR
    cd "$FILE_TEMP_DIR" || return 1
}

# Stop the host HTTP server and remove the shared temp directory, unless
# AIRLOCK_TEST_KEEP=1.
teardown_file() {
    stop_host_http_server
    cd "$REPO_ROOT" || true
    if [[ -n "${FILE_TEMP_DIR:-}" && -d "${FILE_TEMP_DIR:-}" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$FILE_TEMP_DIR"
        fi
    fi
}

# Start each test in the shared temp directory of the file.
setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

# Serve http_root/index.html ("hello-from-host") on host port $1 until
# teardown_file. Call from setup_file in $FILE_TEMP_DIR.
start_host_http_server() {
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    python3 -m http.server "$1" --directory http_root \
        >"$FILE_TEMP_DIR/http_server.log" 2>&1 &
    HTTP_PID=$!
    export HTTP_PID
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
