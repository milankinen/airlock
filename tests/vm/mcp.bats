#!/usr/bin/env bats

load helpers

# Host stdio MCP servers, served in the sandbox as HTTP endpoints.

setup_file() {
    vm_setup_file

    # Tiny MCP server. It reads one JSON-RPC message per line. It answers
    # initialize and tools/list and echoes the request id. It ignores
    # notifications (messages without an id).
    cat > echo-mcp.sh <<'SCRIPT'
#!/bin/sh
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    [ -z "$id" ] && continue
    case "$line" in
    *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"echo-mcp","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo-tool","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no method"}}\n' "$id" ;;
    esac
done
SCRIPT
    chmod +x echo-mcp.sh

    cat > airlock.toml <<EOF
[vm]
# The default image has no curl.
image = "curlimages/curl:latest"

[mcp.echo]
command = "$FILE_TEMP_DIR/project/echo-mcp.sh"
EOF
}

# Test that the sandbox talks to a host stdio MCP server over HTTP.
#   1. Send initialize to http://mcp.airlock/echo and keep the session id
#   2. Send the initialized notification and check for HTTP 202
#   3. Send tools/list with the session id
#   4. Check the response ids, the server name and the tool name
#   5. End the session with DELETE
@test "host stdio mcp server answers initialize and tools list over http" {
    run_vm sh -c '
        url=http://mcp.airlock/echo
        h="Accept: application/json, text/event-stream"
        c="Content-Type: application/json"
        curl -sS -D /tmp/headers.txt -H "$c" -H "$h" -d \
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}" \
            $url
        echo
        sid=$(tr -d "\r" < /tmp/headers.txt | sed -n "s/^[Mm]cp-[Ss]ession-[Ii]d: //p")
        echo "session-id-len=${#sid}"
        curl -sS -o /dev/null -w "notify=%{http_code}\n" -H "$c" -H "$h" \
            -H "Mcp-Session-Id: $sid" -d \
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}" $url
        curl -sS -H "$c" -H "$h" -H "Mcp-Session-Id: $sid" -d \
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}" $url
        echo
        curl -sS -o /dev/null -w "delete=%{http_code}\n" -X DELETE \
            -H "Mcp-Session-Id: $sid" $url
    '
    assert_success
    assert_output_contains '"id":1'
    assert_output_contains 'echo-mcp'
    assert_output_contains 'notify=202'
    assert_output_contains '"id":2'
    assert_output_contains 'echo-tool'
    assert_output_contains 'delete=200'
    assert_output_not_contains 'session-id-len=0'
}

# Test that the index page lists the names and an unknown name is refused.
#   1. Get http://mcp.airlock/ and send initialize to an unknown name
#   2. Check that the index has the line "echo" and the unknown name is not HTTP 200
@test "mcp index lists server names and unknown name is refused" {
    run_vm sh -c '
        curl -sS http://mcp.airlock/ | grep -x echo && echo index-has-echo
        curl -sS -o /dev/null -w "unknown=%{http_code}\n" \
            -H "Content-Type: application/json" \
            -H "Accept: application/json, text/event-stream" \
            -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}" \
            http://mcp.airlock/nope
    '
    assert_success
    assert_output_contains 'index-has-echo'
    assert_output_not_contains 'unknown=200'
}
