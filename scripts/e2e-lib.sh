# Helpers for plugins/<name>/e2e.sh. Source this file, send requests with
# `req`, check them with the `expect_*` functions, and end with `finish`.
#
#   . "$(dirname "$0")/../../scripts/e2e-lib.sh"
#   req -H 'x-user: admin' /
#   expect_status 200
#   expect_header x-rego-result admin
#   finish
#
# GATEWAY_URL defaults to http://127.0.0.1:3000. scripts/e2e.sh sets it, and
# also starts the gateway and the backend. To run a test file against a gateway
# that is already running, run the file directly.

GATEWAY_URL="${GATEWAY_URL:-http://127.0.0.1:3000}"
_e2e_dir="$(mktemp -d "${TMPDIR:-/tmp}/agw-e2e-req.XXXXXX")"
_e2e_failures=0
_e2e_checks=0
_e2e_desc=""
STATUS=""

# req [curl args...] <path>
# Sends a request to GATEWAY_URL<path>. The last argument is the path. Sets
# STATUS, and keeps the response headers and body for the expect_* functions.
req() {
	local path="${!#}"
	local args=("${@:1:$#-1}")
	_e2e_desc="$* "
	STATUS="$(curl -s --max-time 10 -D "${_e2e_dir}/headers" -o "${_e2e_dir}/body" \
		-w '%{http_code}' ${args[@]+"${args[@]}"} "${GATEWAY_URL}${path}")" || STATUS="000"
}

_e2e_pass() {
	_e2e_checks=$((_e2e_checks + 1))
	printf 'ok   %s%s\n' "${_e2e_desc}" "$1"
}

_e2e_fail() {
	_e2e_checks=$((_e2e_checks + 1))
	_e2e_failures=$((_e2e_failures + 1))
	printf 'FAIL %s%s\n' "${_e2e_desc}" "$1"
	printf '     response headers:\n'
	sed 's/^/       /' "${_e2e_dir}/headers" 2>/dev/null
	printf '     response body:\n'
	head -c 2000 "${_e2e_dir}/body" 2>/dev/null | sed 's/^/       /'
	printf '\n'
}

# expect_status <code>
expect_status() {
	if [ "${STATUS}" = "$1" ]; then
		_e2e_pass "status ${1}"
	else
		_e2e_fail "status: want ${1}, got ${STATUS}"
	fi
}

# header_value <name>: prints the value of a response header (case-insensitive).
header_value() {
	local name
	name="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
	tr -d '\r' <"${_e2e_dir}/headers" | awk -v n="${name}" '
		{ i = index($0, ":"); if (i > 0 && tolower(substr($0, 1, i - 1)) == n) {
			v = substr($0, i + 1); sub(/^[ \t]+/, "", v); print v; exit } }'
}

# expect_header <name> [value]: the header is present, and equals value if given.
expect_header() {
	local got
	got="$(header_value "$1")"
	if ! tr -d '\r' <"${_e2e_dir}/headers" | grep -qi "^$1:"; then
		_e2e_fail "header ${1}: missing"
	elif [ $# -ge 2 ] && [ "${got}" != "$2" ]; then
		_e2e_fail "header ${1}: want '${2}', got '${got}'"
	else
		_e2e_pass "header ${1}${2:+=${2}}"
	fi
}

# expect_no_header <name>
expect_no_header() {
	if tr -d '\r' <"${_e2e_dir}/headers" | grep -qi "^$1:"; then
		_e2e_fail "header ${1}: want absent, got '$(header_value "$1")'"
	else
		_e2e_pass "no header ${1}"
	fi
}

# expect_body_contains <text>
expect_body_contains() {
	if grep -qF -- "$1" "${_e2e_dir}/body"; then
		_e2e_pass "body contains '${1}'"
	else
		_e2e_fail "body: want to contain '${1}'"
	fi
}

# The test backend (scripts/e2e.sh) answers 200 with a JSON echo of the request
# it got, so a test can check what the plugin changed on the way upstream.
# expect_upstream_header <name> [value]
expect_upstream_header() {
	local name want
	name="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
	if [ $# -ge 2 ]; then want="\"${name}\": \"$2\""; else want="\"${name}\":"; fi
	if grep -qF -- "${want}" "${_e2e_dir}/body"; then
		_e2e_pass "upstream header ${name}${2:+=${2}}"
	else
		_e2e_fail "upstream header: want ${want}"
	fi
}

# finish: prints a summary and exits non-zero if any check failed.
finish() {
	rm -rf "${_e2e_dir}"
	printf '%d checks, %d failed\n' "${_e2e_checks}" "${_e2e_failures}"
	[ "${_e2e_failures}" -eq 0 ]
	exit $?
}
