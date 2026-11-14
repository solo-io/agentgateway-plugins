#!/usr/bin/env bash
# End-to-end checks for config.yaml. Run with: make e2e PLUGIN=regorus
. "$(dirname "$0")/../../scripts/e2e-lib.sh"

# The admin rule allows the request and writes its result into the response.
req -H 'x-user: admin' /
expect_status 200
expect_header x-rego-result admin
expect_body_contains '"path": "/"'

# /tenant without x-tenant matches no rule, so the default (false) denies it.
req /tenant
expect_status 403
expect_header x-rego-result deny

# /tenant with x-tenant is allowed by the tenant rule.
req -H 'x-tenant: acme' /tenant
expect_status 200
expect_header x-rego-result tenant-route

# Any other path is allowed and tagged with method and path.
req -X POST /orders
expect_status 200
expect_header x-rego-result POST:/orders

finish
