# Load shared helpers from parent directory, then add VM test helpers.
source "$(dirname "${BASH_SOURCE[0]}")/../helpers.bash"

# -- Prerequisites --

# Skip the entire test if VM support is not available.
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

# -- File-level setup/teardown for VM tests --
#
# Checks the binary and VM support, then creates a shared temp dir so that
# the .airlock/ sandbox (disk, image cache link, overlay) persists across
# all tests in a file. Each test runs "airlock --quiet start -- <cmd>",
# which boots a fresh VM but reuses the cached sandbox state. Call from
# setup_file, then write the config there.

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

teardown_file() {
    stop_host_http_server
    cd "$REPO_ROOT" || true
    if [[ -n "${FILE_TEMP_DIR:-}" && -d "${FILE_TEMP_DIR:-}" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$FILE_TEMP_DIR"
        fi
    fi
}

setup() {
    cd "$FILE_TEMP_DIR" || return 1
}

# Serve $FILE_TEMP_DIR/http_root/index.html ("hello-from-host") on host
# port $1 until teardown_file.
start_host_http_server() {
    mkdir -p http_root
    echo "hello-from-host" > http_root/index.html
    python3 -m http.server "$1" --directory http_root \
        >"$FILE_TEMP_DIR/http_server.log" 2>&1 &
    HTTP_PID=$!
    export HTTP_PID
}

stop_host_http_server() {
    if [[ -n "${HTTP_PID:-}" ]]; then
        kill "$HTTP_PID" 2>/dev/null || true
        wait "$HTTP_PID" 2>/dev/null || true
    fi
}

# -- Convenience wrapper --

# Run a command inside a fresh VM. Uses --quiet to suppress setup logs
# so $output contains only the command's stdout/stderr.
# Sets $status, $output, $lines (same convention as run_airlock).
run_vm() {
    run_airlock --quiet start -- "$@"
}
