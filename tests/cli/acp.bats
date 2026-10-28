#!/usr/bin/env bats

load helpers

# The acp command when no sandbox runs.

# Test that the ACP proxy replies to a session request with a JSON-RPC
# error when no sandbox runs. Stdout is the protocol channel, so it must
# have only protocol lines. The proxy log must have the requests.
#   1. Send initialize and session/new for the test dir to airlock acp
#   2. Check the exit code and that each stdout line is a JSON-RPC message
#   3. Check the initialize result and the error for session/new
#   4. Check that the proxy log has the requests with the process id
@test "acp without running sandbox replies error to session/new" {
    local input stdout
    input='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"'"$TEST_TEMP_DIR"'","mcpServers":[]}}'
    local out="$TEST_TEMP_DIR/stdout"
    : >"$out"
    # The proxy drops a pending agent start at the client end. Thus keep
    # the input open until the second reply comes, at most 10 s.
    {
        printf '%s\n' "$input"
        for _ in $(seq 100); do
            [[ "$(wc -l <"$out")" -ge 2 ]] && break
            sleep 0.1
        done
    } | env XDG_DATA_HOME="$TEST_DATA_HOME" \
        NO_COLOR=1 HOME="$TEST_TEMP_DIR" RUST_BACKTRACE=0 \
        "$AIRLOCK" acp -- agent >"$out" 2>"$TEST_TEMP_DIR/stderr" && status=0 || status=$?
    assert_success
    stdout="$(cat "$out")"

    local line count=0
    while IFS= read -r line; do
        [[ "$line" == '{'*'"jsonrpc":"2.0"'*'}' ]] || {
            echo "stdout line is not JSON-RPC: $line" >&2
            return 1
        }
        count=$((count + 1))
    done <<< "$stdout"
    [[ "$count" -eq 2 ]]

    output="$stdout"
    assert_output_contains '"protocolVersion":1'
    assert_output_contains '"id":2'
    assert_output_contains "no running sandbox"

    local log
    log="$(find "$TEST_DATA_HOME" -name acp.log)"
    [[ -n "$log" ]]
    output="$(cat "$log")"
    assert_output_contains "initialize"
    assert_output_contains "agent start failed"
    while IFS= read -r line; do
        [[ "$line" =~ ^\[[0-9]+\]\  ]] || {
            echo "log line has no process id: $line" >&2
            return 1
        }
    done <<< "$output"
}
