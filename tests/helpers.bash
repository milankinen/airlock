# Helpers that all airlock bats tests share.
# The helpers.bash of each suite loads this file.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Find the airlock binary. Order: AIRLOCK_BIN, then CARGO_TARGET_DIR, then target/.
if [[ -n "${AIRLOCK_BIN:-}" ]]; then
    AIRLOCK="$AIRLOCK_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/airlock" ]]; then
    AIRLOCK="$CARGO_TARGET_DIR/release/airlock"
else
    AIRLOCK="$REPO_ROOT/target/release/airlock"
fi

# -- File-level setup (runs one time for each .bats file) --

# Stop the file if the release binary does not exist.
setup_file() {
    if [[ ! -x "$AIRLOCK" ]]; then
        echo "airlock binary not found at $AIRLOCK" >&2
        echo "run: mise run build:release" >&2
        return 1
    fi
}

# -- Per-test setup/teardown --

# Root of the test temp directories. It is in .tmp/tests/ to make
# debugging easy. Set AIRLOCK_TEST_KEEP=1 to keep the temp directories.
TEST_TEMP_ROOT="$REPO_ROOT/.tmp/tests"

# Make a new $TEST_TEMP_DIR and go into it. run_airlock uses it as HOME.
setup_temp_dir() {
    mkdir -p "$TEST_TEMP_ROOT"
    TEST_TEMP_DIR="$(mktemp -d "$TEST_TEMP_ROOT/XXXXXXXX")"
    cd "$TEST_TEMP_DIR" || return 1
}

# Give each test its own temp directory.
setup() {
    setup_temp_dir
}

# Remove the temp directory of the test, unless AIRLOCK_TEST_KEEP=1.
teardown() {
    if [[ -n "$TEST_TEMP_DIR" && -d "$TEST_TEMP_DIR" ]]; then
        if [[ "${AIRLOCK_TEST_KEEP:-}" != "1" ]]; then
            rm -rf "$TEST_TEMP_DIR"
        fi
    fi
}

# -- Run airlock in an isolated environment --

# Run airlock with HOME set to the temp directory, so that it does not read
# ~/.airlock or ~/.cache/airlock/config of the real home. Color output and
# backtraces are off. Stdin is /dev/null, so there is no terminal.
# Sets $status, $output and $lines, as the bats "run" command does.

run_airlock() {
    local _home="${TEST_TEMP_DIR:-$FILE_TEMP_DIR}"
    local _output
    _output="$(env \
        NO_COLOR=1 \
        HOME="$_home" \
        RUST_BACKTRACE=0 \
        "$AIRLOCK" "$@" </dev/null 2>&1)" && status=0 || status=$?
    # Remove carriage returns. airlock writes \r\n for raw terminal mode.
    output="${_output//$'\r'/}"
    IFS=$'\n' read -r -d '' -a lines <<< "$output" || true
}

# -- Assertions --

# Fail if the last command did not exit with 0.
assert_success() {
    if [[ "$status" -ne 0 ]]; then
        echo "expected success (exit 0), got exit $status" >&2
        echo "output: $output" >&2
        return 1
    fi
}

# Fail if the last command exited with 0. With an argument, fail if the
# exit code is not that value.
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

# Fail if $output does not contain the text.
assert_output_contains() {
    local substr="$1"
    if [[ "$output" != *"$substr"* ]]; then
        echo "expected output to contain: $substr" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

# Fail if $output contains the text.
assert_output_not_contains() {
    local substr="$1"
    if [[ "$output" == *"$substr"* ]]; then
        echo "expected output NOT to contain: $substr" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

# Fail if no line of $output matches the extended regex.
assert_output_matches() {
    local pattern="$1"
    if ! echo "$output" | grep -qE "$pattern"; then
        echo "expected output to match: $pattern" >&2
        echo "actual output: $output" >&2
        return 1
    fi
}

# -- Config file helpers --

# Write the argument to airlock.toml in the current directory.
write_config() {
    printf '%s\n' "$1" > airlock.toml
}

# Write the argument to airlock.local.toml in the current directory.
write_local_config() {
    printf '%s\n' "$1" > airlock.local.toml
}

# -- Sandbox state helpers --

# Make a sandbox disk in the current project, with the disk id [1, 2].
make_sandbox_disk() {
    mkdir -p .airlock/sandbox
    truncate -s 1M .airlock/sandbox/disk.img
    echo 00000000000000010000000000000002 >.airlock/sandbox/disk.id
}

# Write .airlock/sandbox/installs.json for the disk [1, 2], with one install
# record for each "pack=status" argument. A "kept" record is also confirmed.
write_installs() {
    local fp packs="" sep="" arg record
    fp="$(printf 'a%.0s' {1..64})"
    for arg in "$@"; do
        record="\"status\": \"${arg#*=}\""
        if [[ "${arg#*=}" == kept ]]; then
            record+=", \"confirmed\": true"
        fi
        packs+="$sep\"${arg%%=*}\": { $record, \"fingerprint\": \"$fp\", \"at\": 1 }"
        sep=", "
    done
    mkdir -p .airlock/sandbox
    printf '{ "version": 1, "disk": [1, 2], "image_id": "sha256:1", "packs": { %s } }\n' "$packs" \
        >.airlock/sandbox/installs.json
}
