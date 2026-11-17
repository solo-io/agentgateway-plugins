#!/usr/bin/env bash
# Runs a plugin end to end in a real agentgateway.
#
# Usage: scripts/e2e.sh <plugin> [agentgateway-binary]
#
# 1. Builds plugins/<plugin> (cargo build --release, or make for C plugins).
# 2. Copies plugins/<plugin>/config.yaml to a temp file, with the plugin path
#    made absolute and `.so` changed to the library suffix of this OS
#    (`.dylib` on macOS).
#    If a port the config uses is busy (the bind port, the 8080 backend, or the
#    15000/15020/15021 admin ports), it moves to a free one in the temp copy.
# 3. Starts a test backend on 127.0.0.1 (8080 unless busy). It answers every
#    request with 200 and a JSON echo of the request (method, path, headers).
# 4. Starts `agentgateway -f <temp config>` and waits for the gateway port.
# 5. Runs plugins/<plugin>/e2e.sh, if present, with GATEWAY_URL set. Without
#    one, it only checks that the gateway starts and loads the plugin.
# 6. Stops everything.
#
# The binary is the second argument, else $AGW_BIN, else `agentgateway` on PATH.
# It must be an enterprise build with plugin support. Standalone enterprise
# agentgateway needs a license in ENTERPRISE_AGENTGATEWAY_LICENSE_KEY.
#
# Environment:
#   AGW_BIN        agentgateway binary
#   BACKEND_PORT   test backend port (default: 8080, or the next free port)
#   GATEWAY_PORT   gateway port (default: the first bind port in config.yaml,
#                  or the next free port)
#   SKIP_BUILD=1   use the library that is already built
#   KEEP_TMP=1     keep the temp directory (config, logs)
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
	echo "usage: $0 <plugin> [agentgateway-binary]" >&2
	exit 2
fi

plugin="$1"
agw_bin="${2:-${AGW_BIN:-agentgateway}}"
root="$(cd "$(dirname "$0")/.." && pwd)"
plugin_dir="${root}/plugins/${plugin}"

die() {
	echo "e2e: $*" >&2
	exit 1
}

[ -d "${plugin_dir}" ] || die "no plugin directory ${plugin_dir}"
[ -f "${plugin_dir}/config.yaml" ] || die "${plugin_dir} has no config.yaml"
command -v "${agw_bin}" >/dev/null 2>&1 || die "agentgateway binary not found: ${agw_bin} (pass it as the second argument or set AGW_BIN)"
command -v python3 >/dev/null 2>&1 || die "python3 is required for the test backend"
command -v curl >/dev/null 2>&1 || die "curl is required"
agw_bin="$(command -v "${agw_bin}")"

if [ -z "${ENTERPRISE_AGENTGATEWAY_LICENSE_KEY:-}" ]; then
	echo "e2e: warning: ENTERPRISE_AGENTGATEWAY_LICENSE_KEY is not set; a standalone enterprise gateway refuses to start without a license" >&2
fi

case "$(uname -s)" in
Darwin) lib_ext="dylib" ;;
*) lib_ext="so" ;;
esac

port_in_use() {
	(exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# free_port <port>: prints <port>, or the next port after it that is free and
# not already in ${taken} (ports this script has handed out).
# Tests by binding, on IPv4 and IPv6 wildcards, because a connect test misses
# listeners on other addresses that still make the gateway's bind fail.
free_port() {
	python3 - "$1" ${taken} <<'PY'
import socket, sys
port = int(sys.argv[1])
taken = {int(p) for p in sys.argv[2:]}
def free(p):
    if p in taken:
        return False
    for family, addr in ((socket.AF_INET, "0.0.0.0"), (socket.AF_INET6, "::")):
        s = socket.socket(family, socket.SOCK_STREAM)
        try:
            if family == socket.AF_INET6:
                s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            s.bind((addr, p))
        except OSError:
            return False
        finally:
            s.close()
    return True
while not free(port):
    port += 1
print(port)
PY
}

config_port="$(sed -n 's/^[[:space:]-]*port:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "${plugin_dir}/config.yaml" | head -n 1)"
config_port="${config_port:-3000}"
taken=""
backend_port="${BACKEND_PORT:-$(free_port 8080)}"
taken="${backend_port}"
gateway_port="${GATEWAY_PORT:-$(free_port "${config_port}")}"
taken="${taken} ${gateway_port}"
# The admin, stats and readiness servers. Without this, a gateway that is
# already running on this machine makes the test gateway fail to start.
for var in ADMIN_ADDR:15000 STATS_ADDR:15020 READINESS_ADDR:15021; do
	name="${var%%:*}"
	if [ -z "${!name:-}" ]; then
		export "${name}=127.0.0.1:$(free_port "${var##*:}")"
	fi
	taken="${taken} ${!name##*:}"
done
[ "$(echo ${taken} | tr ' ' '\n' | sort -u | wc -l | tr -d ' ')" = "5" ] || die "port clash: ${taken}; set the ports explicitly"

# 1. Build.
if [ "${SKIP_BUILD:-0}" != "1" ]; then
	echo "==> building ${plugin}"
	if [ -f "${plugin_dir}/Cargo.toml" ]; then
		cargo build --release --manifest-path "${plugin_dir}/Cargo.toml"
	elif [ -f "${plugin_dir}/Makefile" ]; then
		make -C "${plugin_dir}"
	else
		die "${plugin_dir} has neither Cargo.toml nor Makefile"
	fi
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/agw-e2e-${plugin}.XXXXXX")"
backend_pid=""
agw_pid=""
status=1

cleanup() {
	local pid
	for pid in ${agw_pid} ${backend_pid}; do
		kill "${pid}" 2>/dev/null || true
		wait "${pid}" 2>/dev/null || true
	done
	if [ "${status}" -ne 0 ] && [ -f "${tmp}/agentgateway.log" ]; then
		echo "==> agentgateway log (last 40 lines)" >&2
		tail -n 40 "${tmp}/agentgateway.log" >&2
	fi
	if [ "${KEEP_TMP:-0}" = "1" ]; then
		echo "==> kept ${tmp}" >&2
	else
		rm -rf "${tmp}"
	fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# 2. Config: absolute plugin path and this OS's library suffix. The rewrite only
# touches `plugin:` lines; bare names (resolved through AGW_PLUGIN_PATH) are
# left alone, since the gateway adds the suffix itself. Ports are moved as
# chosen above.
sed -e "/^[[:space:]-]*plugin:/s#\(plugin:[[:space:]]*\)\./#\1${root}/#" \
	-e "/^[[:space:]-]*plugin:/s#\.so\([[:space:]]*\)\$#.${lib_ext}\1#" \
	-e "s#^\([[:space:]-]*port:[[:space:]]*\)${config_port}\([[:space:]]*\)\$#\1${gateway_port}\2#" \
	-e "s#127\.0\.0\.1:8080\$#127.0.0.1:${backend_port}#" -e "s#127\.0\.0\.1:8080\([^0-9]\)#127.0.0.1:${backend_port}\1#g" \
	-e "s#localhost:8080\$#127.0.0.1:${backend_port}#" -e "s#localhost:8080\([^0-9]\)#127.0.0.1:${backend_port}\1#g" \
	"${plugin_dir}/config.yaml" >"${tmp}/config.yaml"
echo "==> gateway :${gateway_port}, backend :${backend_port}, admin ${ADMIN_ADDR}"
for lib in $(sed -n 's#^[[:space:]-]*plugin:[[:space:]]*\(/[^[:space:]]*\).*#\1#p' "${tmp}/config.yaml"); do
	[ -f "${lib}" ] || die "plugin library not found: ${lib}"
	echo "==> plugin library ${lib}"
done

# 3. Backend.
cat >"${tmp}/backend.py" <<'EOF'
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class Echo(BaseHTTPRequestHandler):
    def handle_one(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length).decode("utf-8", "replace") if length else ""
        out = json.dumps({
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body,
        }, indent=1).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)
    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = handle_one
    def do_HEAD(self):
        self.send_response(200)
        self.end_headers()
    def log_message(self, *args):
        pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Echo).serve_forever()
EOF
python3 "${tmp}/backend.py" "${backend_port}" >"${tmp}/backend.log" 2>&1 &
backend_pid=$!

# 4. Gateway. Run from the repository root so relative paths in configs work.
echo "==> starting ${agw_bin}"
(cd "${root}" && exec "${agw_bin}" -f "${tmp}/config.yaml") >"${tmp}/agentgateway.log" 2>&1 &
agw_pid=$!

ready=0
for _ in $(seq 1 150); do
	if ! kill -0 "${agw_pid}" 2>/dev/null; then
		die "agentgateway exited during startup"
	fi
	if port_in_use "${gateway_port}" && port_in_use "${backend_port}"; then
		ready=1
		break
	fi
	sleep 0.2
done
[ "${ready}" = "1" ] || die "gateway did not listen on ${gateway_port} within 30s"

if grep -qi "failed to load plugin\|plugin.*error" "${tmp}/agentgateway.log"; then
	echo "==> warning: the gateway log mentions a plugin error" >&2
fi

# 5. Tests.
export GATEWAY_URL="http://127.0.0.1:${gateway_port}"
export BACKEND_URL="http://127.0.0.1:${backend_port}"
if [ -f "${plugin_dir}/e2e.sh" ]; then
	echo "==> running plugins/${plugin}/e2e.sh"
	set +e
	bash "${plugin_dir}/e2e.sh"
	status=$?
	set -e
else
	echo "==> plugins/${plugin}/e2e.sh not found; checking only that the gateway answers"
	code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "${GATEWAY_URL}/" || true)"
	echo "GET / -> ${code}"
	case "${code}" in
	000 | 5??) status=1 ;;
	*) status=0 ;;
	esac
fi

if [ "${status}" -eq 0 ]; then
	echo "==> PASS ${plugin}"
else
	echo "==> FAIL ${plugin}" >&2
fi
exit "${status}"
