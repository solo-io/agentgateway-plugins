# Cedar plugin

Authorizes requests through agentgateway with [Cedar](https://docs.cedarpolicy.com/)
policies, evaluated in-process by the [`cedar-policy`](https://crates.io/crates/cedar-policy)
crate. It maps each request to a Cedar principal, action and resource, reads
identity from JWT claims, and type-checks policies against an optional schema
when the config loads.

| Field | Value |
| --- | --- |
| Language | Rust |
| Description | Authorizes requests with inline Cedar policies, schema validation, and entity hierarchies. |
| Status | Demo |

## How it works

1. At load, the plugin parses the policies, the optional schema and static
   entities. With a schema, policies are validated in strict mode, so a typo
   such as `principal.levle` fails the config.
2. Per request, it builds `principal` from `principal.id` (for example
   `User::"<jwt.sub>"`), with optional attributes (for example all JWT claims)
   and parents (for example `Group` entities from `jwt.groups`). The default
   `action` is `Action::"<method>"` and the default `resource` is
   `Route::"<path>"`. `context` holds `method`, `path`, `query`, `host`, the
   configured headers and properties.
3. Cedar decides. A deny returns `denyStatus` (403); a request that can't form
   a valid Cedar request, such as a context that breaks the schema, returns
   `errorStatus` (503). Both bodies are generic JSON.
4. The decision (`allow`, `deny`, `error` or `skip`) and the deciding policy
   IDs (`@id("...")`) are recorded as CEL metadata `cedar.decision` and
   `cedar.policies`.

## Build and run

```sh
cd plugins/cedar
cargo clippy --all-targets -- -D warnings && cargo test   # lint and unit tests
cargo build --release          # target/release/libagw_plugin_cedar.{so,dylib}
cd ../..
make docker PLUGIN=cedar       # image with only /plugin.so
make e2e PLUGIN=cedar AGW_BIN=<enterprise agentgateway>   # needs ENTERPRISE_AGENTGATEWAY_LICENSE_KEY
```

`make e2e` starts the gateway with `config.yaml` and a test backend, then runs
`e2e.sh`. The demo takes the principal from an `x-user` header for local
testing only; group membership comes from static entities:

```sh
curl -si -X POST -H 'x-user: alice' localhost:3000/admin    # 200: alice is in admins
curl -si -X POST -H 'x-user: bob' localhost:3000/admin      # 403
curl -si localhost:3000/public/readme                       # 200: anonymous GET on /public/*
curl -si -X DELETE -H 'x-user: dan' localhost:3000/thing    # 403: dan is a contractor, forbid wins
```

## Config

A source is `{ header: name }`, `{ property: cel.path }` (such as `jwt.sub` or
`mcp.tool.name`), `{ literal: value }`, `method`, `path` or `host`.

| Field | Default | Description |
| --- | --- | --- |
| `policies` | required | Cedar policy text. Name policies with `@id("...")`. |
| `principal.id` | required | Source of the principal ID, normally `{ property: jwt.sub }`. Without a value the principal is `<type>::""`. |
| `principal.type` | `User` | Principal entity type. |
| `principal.attributes` | none | A map property (such as `{ property: jwt }`) whose entries become principal attributes. Nulls are dropped; doubles and timestamps become strings. |
| `principal.parents` | none | `{ type: Group, from: <source> }`. Lists are used item by item; headers and plain strings are split on commas. |
| `action` | `{ type: Action, id: method }` | `{ type, id: <source>, default }`. Without an ID or default, the request is denied. |
| `resource` | `{ type: Route, id: path }` | Same shape as `action`. |
| `schema` | none | Cedar schema (human-readable format). Principal attributes the schema doesn't declare are dropped. |
| `entities` | `[]` | Static entities in [Cedar entity JSON](https://docs.cedarpolicy.com/auth/entities-syntax.html). They win over request-derived principals with the same UID. |
| `context.headers` | `[]` | Request headers copied into `context.headers`. |
| `context.properties` | `{}` | Context field name to CEL property path. |
| `mcpToolCallsOnly` | `false` | Authorize only MCP `tools/call`; other MCP methods continue with decision `skip`. Non-MCP requests are still authorized. |
| `mode` | `enforce` | `audit` records the decision but never blocks. |
| `denyStatus` | `403` | Status for denies (400-599). |
| `errorStatus` | `503` | Status when the request can't be evaluated (400-599). |
| `decisionHeader` | none | Response header with the decision. |
| `exposeReasons` | `false` | Include the deciding policy IDs in deny bodies. |

## Deploy: standalone

Needs Solo Enterprise for agentgateway v2026.11.0 or later and a license in
`ENTERPRISE_AGENTGATEWAY_LICENSE_KEY`. Without the license the gateway exits at
startup.

### Get the plugin library

The image `ghcr.io/solo-io/agentgateway-plugins/cedar:<version>` holds only
`/plugin.so` (linux amd64 and arm64) and has no command, so give
`docker create` one:

```sh
mkdir -p plugins/cedar
cid=$(docker create --platform linux/amd64 ghcr.io/solo-io/agentgateway-plugins/cedar:<version> /plugin.so)   # or linux/arm64
docker cp "$cid:/plugin.so" plugins/cedar/plugin.so
docker rm "$cid"
```

Images are published from `cedar/v*` tags (for example `cedar/v0.1.0`). While the image is private, run
`docker login ghcr.io` first.

The image holds a Linux library: use it with the Linux binary or the container
image, and match `--platform` to the gateway's architecture. For the macOS
binary, build instead (`cargo build --release` in `plugins/cedar`) and copy
`plugins/cedar/target/release/libagw_plugin_cedar.dylib` to `plugins/cedar/plugin.dylib`.

Layout, one library per directory:

```
plugins/
  cedar/
    plugin.so
```

### config.yaml

Plugins are a policy, not a top-level key. Reference the plugin by bare name;
the gateway finds it under `AGW_PLUGIN_PATH`. Standalone `order` values are
camelCase: `early`, `postAuthentication`, `postAuthorization`, `late`
(default).

```yaml
# yaml-language-server: $schema=https://agentgateway.dev/schema/config
gateways:
  default:
    port: 3000
routes:
- name: main
  policies:
    plugins:
    - name: cedar
      order: postAuthentication   # after JWT authentication, so jwt.* is set
      config:
        decisionHeader: x-cedar-decision
        principal:
          id: { property: jwt.sub }
        # action = Action::"<method>", resource = Route::"<path>" (defaults)
        entities:
        - uid: { type: User, id: alice }
          attrs: {}
          parents: [{ type: Group, id: admins }]
        - uid: { type: Group, id: admins }
          attrs: {}
          parents: []
        policies: |
          @id("admins-can-do-anything")
          permit (principal in Group::"admins", action, resource);

          @id("anyone-can-read-public")
          permit (principal, action == Action::"GET", resource)
          when { context.path like "/public/*" };
  backends:
  - host: 127.0.0.1:8080
```

Add your JWT authentication policy to the same route so `jwt.sub` is set.
Policies and entities are inline config; see [Config](#config) for the other
fields.

The plugin reads no secrets. The gateway shell-expands the whole config file
before parsing it, so `${VAR}` works in plugin config, an unset variable
without a default fails startup, and any literal `$` in policy text is read as
a variable.

### Run

With the enterprise binary:

```sh
export ENTERPRISE_AGENTGATEWAY_LICENSE_KEY=...
export AGW_PLUGIN_PATH=$PWD/plugins
agentgateway -f config.yaml --validate-only   # optional: check the config first
agentgateway -f config.yaml
```

With the enterprise container image:

```sh
docker run -d --name agentgateway \
  -e ENTERPRISE_AGENTGATEWAY_LICENSE_KEY \
  -e AGW_PLUGIN_PATH=/var/lib/agentgateway/plugins \
  -v "$PWD/plugins:/var/lib/agentgateway/plugins:ro" \
  -v "$PWD/config.yaml:/config/config.yaml:ro" \
  -p 3000:3000 \
  us-docker.pkg.dev/solo-public/enterprise-agentgateway/agentgateway-enterprise:<agw-version> \
  -f /config/config.yaml
```

`-e NAME` without a value passes the variable from your shell. With the
read-only config mount the gateway logs a warning that it can't write a
session key and keeps running; set `SESSION_KEY` (64 hex characters) to
silence it.

If the plugin fails to load, standalone startup fails and the error lists
every path it searched.

### Verify

```sh
docker logs agentgateway 2>&1 | grep 'loaded plugin'
# loaded plugin name=cedar version=0.1.0 abi_version=1 path=/var/lib/agentgateway/plugins/cedar/plugin.so
curl -si localhost:3000/public/readme   # 200, x-cedar-decision: allow
```

To update, replace `plugins/cedar/plugin.so` and restart the gateway.

## Deploy: Kubernetes

Prerequisites:

- The Solo Enterprise for agentgateway controller and CRDs, v2026.11.0 or
  later.
- Kubernetes image volume support (the `ImageVolume` feature and a runtime
  that supports it). The controller mounts the plugin image as a volume.
- A Gateway that runs as a Deployment (the default). DaemonSet gateways get
  no plugin mounts.

Images are published from `cedar/v*` tags (for example `cedar/v0.1.0`). While the image is private, the gateway
pod needs an `imagePullSecret` (below).

Put all objects in the Gateway's namespace.

### EnterpriseAgentgatewayParameters

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayParameters
metadata:
  name: agw-plugins
spec:
  plugins:
  - name: cedar
    image: ghcr.io/solo-io/agentgateway-plugins/cedar:<version>
```

The controller mounts the image at `/var/lib/agentgateway/plugins/cedar` and
sets `AGW_PLUGIN_PATH`. The plugin reads no secrets, so `spec.env` isn't
needed.

While the image is private, create a registry secret and add it with the
deployment overlay:

```sh
kubectl create secret docker-registry ghcr-pull -n <gateway-namespace> \
  --docker-server=ghcr.io --docker-username=<user> --docker-password=<token>
```

```yaml
spec:
  plugins:
  - name: cedar
    image: ghcr.io/solo-io/agentgateway-plugins/cedar:<version>
  deployment:
    spec:
      template:
        spec:
          imagePullSecrets:
          - name: ghcr-pull
```

### Gateway

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: my-gateway
spec:
  gatewayClassName: enterprise-agentgateway
  infrastructure:
    parametersRef:
      group: enterpriseagentgateway.solo.io
      kind: EnterpriseAgentgatewayParameters
      name: agw-plugins
  listeners:
  - name: http
    port: 8080
    protocol: HTTP
```

### EnterpriseAgentgatewayPolicy

Kubernetes `order` values are PascalCase: `Early`, `PostAuthentication`,
`PostAuthorization`, `Late` (default). `config` must be a JSON object, and it
is not expanded for `$VAR`, so `$` in policy text is passed as written.

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayPolicy
metadata:
  name: cedar
spec:
  targetRefs:
  - group: gateway.networking.k8s.io
    kind: Gateway
    name: my-gateway
  traffic:
    entPlugin:
      name: cedar
      order: PostAuthentication   # after JWT authentication, so jwt.* is set
      config:
        principal:
          id: { property: jwt.sub }
          parents: { type: Group, from: { property: jwt.groups } }
        resource: { type: Tool, id: { property: mcp.tool.name } }
        action: { type: Action, id: { literal: call_tool } }
        mcpToolCallsOnly: true
        policies: |
          @id("sre-can-restart")
          permit (principal in Group::"sre", action == Action::"call_tool", resource == Tool::"restart_service");
          @id("everyone-can-search")
          permit (principal, action == Action::"call_tool", resource == Tool::"search");
```

### Verify

```sh
kubectl -n <gateway-namespace> get pods   # ContainerCreating: image volume unsupported or pull failed; see describe
kubectl -n <gateway-namespace> logs deploy/my-gateway | grep -E 'loaded plugin|failed to load plugin'
kubectl -n <gateway-namespace> get enterpriseagentgatewaypolicy cedar -o jsonpath='{.status.ancestors[*].conditions}'
```

The `loaded plugin name=cedar ...` line appears when the policy reaches the
gateway, not at pod start. The gateway image has no `sh` or `ls`, so
`kubectl exec` can't list the plugin directory. A load failure doesn't crash
the pod: the gateway logs `failed to load plugin` and fails every request the
policy applies to.

### Update

Push a new `cedar/v*` tag and set the new `image` in `spec.plugins`; the Deployment
rolls. Don't reuse a tag. Config, order and target changes are edits to the
policy and need no rollout.

## Limitations

- Policies and entities are static config; a change is a config change.
- Request bodies aren't parsed. Use `context.properties` with CEL properties
  the gateway already computes, such as `llm.requestModel`.
- With a schema, the context fields above must match the action's context
  type, and kept principal attributes must match their declared types.
- A request-derived principal with attributes or parents is added to a copy of
  the static entity store. Measured cost: about 35 µs per request with 100
  static entities and 330 µs with 1,000.

## Future work

### Plugin

- Adding the request principal copies the static entity store and makes Cedar
  scan every entity, so cost grows linearly with `entities` (see Limitations).
  Precomputing the principal's ancestors at load was measured and saves
  nothing, because the cost is inside `Entities::add_entities`. Suggested
  change: ask Cedar for an API that adds one entity without a full scan, or
  load large directories from a store instead of inline config.
- Schema filtering of principal attributes covers top-level claims only. A
  nested claim object or a claim with the wrong type still makes the request
  return `errorStatus`. Suggested change: filter nested records against the
  schema too, or drop non-conforming values with a log.
- Context values are built directly as Cedar values, so a schema that types a
  context field as an extension (`ipaddr`, `decimal`, `datetime`) won't match
  the string the plugin sends. Suggested change: convert by schema type.
- Cedar's API doesn't expose entity shapes, so the plugin reads declared
  attributes from the schema's JSON form. Suggested change: use a shape API if
  Cedar adds one.
- Unauthenticated requests use `<type>::""` rather than a separate entity type,
  so they work with schemas. Policies can't write `principal is Anonymous`.
  Suggested change: make this an option if users ask for it.
- `context.properties` takes a bare property path while the AuthZEN plugin
  takes a source or a path. Suggested change: use one shape across the
  authorization plugins.
- With `principal.attributes: { property: jwt }` and parents from
  `jwt.groups`, the plugin makes three property calls for data that is in the
  first. Suggested change: read parents from the attributes map when the
  paths share a root.

### SDK and gateway

- `RequestContext::property` takes `&[&str]`, so a plugin that stores owned
  paths allocates on every call. Suggested change: accept an iterator of
  `&str` or a path compiled once.
- The test host can't return an error from a property read, so the fail-closed
  path for host property errors has no unit test. Suggested change: let tests
  inject property errors.
- The test host doesn't compile CEL or run callouts. This plugin makes no
  callouts; plugins that do must test them through their own seams.
- Callouts are sent chunked without `Content-Length` unless the plugin sets it,
  and there is no built-in callout timeout. This plugin makes no callouts.
- The upstream plugin template docs call the Kubernetes orders
  `PostAuth`/`PostAuthz`, but the shipped CRD accepts only `Early`,
  `PostAuthentication`, `PostAuthorization` and `Late` (standalone config uses
  the camelCase `early`, `postAuthentication`, `postAuthorization`, `late`).
  This plugin's docs are corrected. Suggested change: correct the template.

### Verification

- The MCP properties (`mcp.methodName`, `mcp.tool.name`) and
  `llm.requestModel` match the gateway source, but their availability at the
  plugin's order is tested only against the mock host, not a real MCP or LLM
  route.
- The JWT path (`jwt.sub`, `jwt.groups`, attributes) is unit-tested only; the
  real-gateway test uses a header principal.
- The standalone config passes `--validate-only` and loads the plugin. The
  Kubernetes manifests match the CRD schema but were not applied to a
  cluster, and the image `ghcr.io/solo-io/agentgateway-plugins/cedar` is not
  published yet.
- Scale numbers come from a micro-benchmark of the entity store, not a load
  test through the gateway.

## License

Apache-2.0
