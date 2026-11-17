#!/usr/bin/env bash
# End-to-end checks for config.yaml. Run with: make e2e PLUGIN=cedar
. "$(dirname "$0")/../../scripts/e2e-lib.sh"

# alice is a static member of Group::"admins": anything is allowed.
req -X POST -H 'x-user: alice' /admin
expect_status 200
expect_header x-cedar-decision allow

# bob matches no permit: default deny with a JSON body.
req -X POST -H 'x-user: bob' /admin
expect_status 403
expect_header x-cedar-decision deny
expect_body_contains '"authorization_denied"'

# A client can't join a group by sending a header.
req -X DELETE -H 'x-user: bob' -H 'x-groups: admins' /thing
expect_status 403

# Anonymous GET under /public/* is allowed.
req /public/readme
expect_status 200
expect_header x-cedar-decision allow

# /tenant needs x-tenant: acme in the Cedar context.
req /tenant
expect_status 403
req -H 'x-tenant: globex' /tenant
expect_status 403
req -H 'x-tenant: acme' /tenant
expect_status 200

# dan is in admins and contractors: POST is allowed, DELETE is forbidden.
req -X POST -H 'x-user: dan' /thing
expect_status 200
req -X DELETE -H 'x-user: dan' /thing
expect_status 403
expect_body_contains 'no-deletes-for-contractors'

finish
