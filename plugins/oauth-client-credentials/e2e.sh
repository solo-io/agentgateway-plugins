#!/usr/bin/env bash
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"

if [ -z "${GATEWAY_URL:-}" ]; then
    export OAUTH_CLIENT_ID='test-client' OAUTH_CLIENT_SECRET='test-secret'
    export OAUTH_SERVER_SECRET="${OAUTH_CLIENT_SECRET}" OAUTH_TTL=5
    OAUTH_SERVER_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    export OAUTH_SERVER_PORT OAUTH_TOKEN_URL="http://127.0.0.1:${OAUTH_SERVER_PORT}/token"
    python3 "${dir}/testdata/oauth_server.py" "${OAUTH_SERVER_PORT}" &
    server=$!
    export OAUTH_SERVER_PID="${server}"
    trap 'kill "${server}" 2>/dev/null || true; wait "${server}" 2>/dev/null || true' EXIT
    for _ in $(seq 1 100); do
        curl -fsS "http://127.0.0.1:${OAUTH_SERVER_PORT}/count" >/dev/null 2>&1 && break
        sleep 0.1
    done
    "${dir}/../../scripts/e2e.sh" oauth-client-credentials "$@"
    exit
fi

. "${dir}/../../scripts/e2e-lib.sh"
req -H 'Authorization: Bearer client-token' /
expect_status 200
expect_upstream_header authorization 'Bearer tok-1'
if grep -qF 'client-token' "${_e2e_dir}/body"; then _e2e_fail 'client authorization removed'; else _e2e_pass 'client authorization removed'; fi
req /
expect_status 200
expect_upstream_header authorization 'Bearer tok-1'
count() { curl -fsS "http://127.0.0.1:${OAUTH_SERVER_PORT}/count" | python3 -c 'import json,sys; print(json.load(sys.stdin)["count"])'; }
if [ "$(count)" = 1 ]; then _e2e_pass 'one token fetch'; else _e2e_fail 'one token fetch'; fi
sleep 6
before="$(count)"
pids=()
for index in $(seq 1 10); do
    curl -fsS --max-time 10 "${GATEWAY_URL}/" >"${_e2e_dir}/parallel-${index}" &
    pids+=("$!")
done
parallel_ok=1
for pid in "${pids[@]}"; do wait "${pid}" || parallel_ok=0; done
if [ "${parallel_ok}" = 1 ] && [ "$(count)" -eq "$((before + 1))" ]; then _e2e_pass 'parallel requests share one token fetch'; else _e2e_fail 'parallel requests share one token fetch'; fi
req /
expect_status 200
expect_upstream_header authorization 'Bearer tok-2'
kill "${OAUTH_SERVER_PID}"
export OAUTH_SERVER_SECRET=wrong
python3 "${dir}/testdata/oauth_server.py" "${OAUTH_SERVER_PORT}" &
server=$!
trap 'kill "${server}" 2>/dev/null || true; wait "${server}" 2>/dev/null || true' EXIT
sleep 6
req /
expect_status 503
expect_body_contains '{"error":{"type":"backend_auth_unavailable","message":"could not obtain backend credentials"}}'
if grep -qF -e "${OAUTH_CLIENT_ID}" -e "${OAUTH_CLIENT_SECRET}" "${_e2e_dir}/body" "${_e2e_dir}/headers"; then _e2e_fail 'response contains no credentials'; else _e2e_pass 'response contains no credentials'; fi
kill "${server}"
wait "${server}" 2>/dev/null || true
req /
expect_status 503
finish
