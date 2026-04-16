# Shared helpers for airlock bats tests.
# Source this from every .bats file: load helpers

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Find the airlock binary: explicit override > CARGO_TARGET_DIR > default target/
if [[ -n "${AIRLOCK_BIN:-}" ]]; then
    AIRLOCK="$AIRLOCK_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/airlock" ]]; then
    AIRLOCK="$CARGO_TARGET_DIR/release/airlock"
else
    AIRLOCK="$REPO_ROOT/target/release/airlock"
fi

# -- File-level setup (runs once per .bats file) --

setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
}

# -- Per-test setup/teardown --

# Test temp directory root — under .tmp/tests/ for easy debugging.
# Set AIRLOCK_TEST_KEEP=1 to preserve temp dirs after tests.
TEST_TEMP_ROOT="$REPO_ROOT/.tmp/tests"

setup() {
    mkdir -p "$TEST_TEMP_ROOT"
    TEST_TEMP_DIR="$(mktemp -d "$TEST_TEMP_ROOT/XXXXXXXX")"
    cd "$TEST_TEMP_DIR" || return 1
}

teardown() {
    if [[ -n "$TEST_TEMP_DIR" && -d "$TEST_TEMP_DIR" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$TEST_TEMP_DIR"
        fi
    fi
}

# -- Run airlock in a hermetic environment --
#
# Sets HOME to the temp dir so ~/.airlock and ~/.cache/airlock/config
# are not read from the real home. Disables color output and backtraces.
# Stdin is /dev/null (non-interactive).
#
# After calling, $status, $output, and $lines are set (bats convention).

run_airlock() {
    local _home="${TEST_TEMP_DIR:-$FILE_TEMP_DIR}"
    local _output
    _output="$(env \
        NO_COLOR=1 \
        HOME="$_home" \
        RUST_BACKTRACE=0 \
        "$AIRLOCK" "$@" </dev/null 2>&1)" && status=0 || status=$?
    # Strip carriage returns (airlock uses \r\n for raw terminal compat)
    output="${_output//$'\r'/}"
    # Rebuild lines array
    IFS=$'\n' read -r -d '' -a lines <<< "$output" || true
}

# -- Assertions --

assert_success() {
    if [[ "$status" -ne 0 ]]; then
        echo "expected success (exit 0), got exit $status" >&2
        echo "output: $output" >&2
        return 1
    fi
}

assert_failure() {
    local expected="${1:-}"
    if [[ -n "$expected" ]]; then
        if [[ "$status" -ne "$expected" ]]; then
            echo "expected exit $expected, got exit $status" >&2
            echo "output: $output" >&2
            return 1
        fi
    else
        if [[ "$status" -eq 0 ]]; then
            echo "expected failure (non-zero exit), got exit 0" >&2
            echo "output: $output" >&2
            return 1
        fi
    fi
}

assert_output_contains() {
    local substr="$1"
    if [[ "$output" != *"$substr"* ]]; then
        echo "expected output to contain: $substr" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

assert_output_not_contains() {
    local substr="$1"
    if [[ "$output" == *"$substr"* ]]; then
        echo "expected output NOT to contain: $substr" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

assert_output_matches() {
    local pattern="$1"
    if ! echo "$output" | grep -qE "$pattern"; then
        echo "expected output to match: $pattern" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

# -- Config file helpers --

write_config() {
    printf '%s\n' "$1" > airlock.toml
}

write_local_config() {
    printf '%s\n' "$1" > airlock.local.toml
}
