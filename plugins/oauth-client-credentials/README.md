# OAuth client credentials

| Language | Description | Status |
| --- | --- | --- |
| Rust | Adds an OAuth client credentials access token to backend requests. | Demo |

## Purpose

This Solo Enterprise for agentgateway native plugin obtains OAuth 2.0 client credentials tokens for downstream services such as MCP servers. Agentgateway's built-in backend authentication covers token exchange and JWT bearer grants; this plugin covers the client credentials grant. Native plugins require an enterprise gateway and an SDK revision matching that gateway release.

## How it works

The plugin reads the client ID and secret from environment variables when it loads. It obtains a token from the configured endpoint and sets the backend request header. Tokens are cached until the refresh window; concurrent requests share one token fetch. During a refresh, other requests may use the existing token until it expires. A backend 401 invalidates the token used by that request so the next request fetches another. Requests are not retried.

## Build and run

From `plugins/oauth-client-credentials`, run `cargo clippy --all-targets -- -D warnings`, `cargo test` and `cargo build --release` with Rust installed. From the repository root, build a container with `make docker PLUGIN=oauth-client-credentials`. For a real gateway test, set `ENTERPRISE_AGENTGATEWAY_LICENSE_KEY` and run `make e2e PLUGIN=oauth-client-credentials AGW_BIN=/path/to/agentgateway`.

## Config

| Field | Default | Description |
| --- | --- | --- |
| `tokenUrl` | required | Absolute HTTP or HTTPS token endpoint URL. HTTPS is recommended. |
| `clientIdEnv` | required | Environment variable containing the client ID. |
| `clientSecretEnv` | required | Environment variable containing the client secret. |
| `clientAuth` | `basic` | `basic` or `post`. |
| `scope` | omitted | OAuth scope string. |
| `audience` | omitted | Provider-specific audience. |
| `resource` | omitted | RFC 8707 resource indicator. |
| `extraParams` | `{}` | Additional form fields; grant type and client credential fields are reserved. |
| `refreshBeforeSeconds` | `60` | Refresh this many seconds before expiry, capped at half the token lifetime. |
| `defaultTtlSeconds` | `300` | Lifetime when `expires_in` is omitted. |
| `timeoutMs` | `2000` | Token request and waiter timeout, greater than zero. |
| `maxResponseBytes` | `65536` | Maximum token response size. |
| `header` | `authorization` | Backend request header. |
| `scheme` | `Bearer` | Authentication scheme before the token. |
| `replaceIncoming` | `true` | Remove incoming Authorization and the configured header before setting the token. |
| `errorStatus` | `503` | Status on token failure, from 400 to 599. |

## Deploy: standalone

Needs Solo Enterprise for agentgateway v2026.11.0 or later and a license in
`ENTERPRISE_AGENTGATEWAY_LICENSE_KEY`.

Images are published from `oauth-client-credentials/v*` tags (for example
`oauth-client-credentials/v0.1.0`).

Get the library from the image (Linux amd64 and arm64; the image has no
command, so give `docker create` one), or build it with `cargo build --release` in `plugins/oauth-client-credentials`. For the
macOS binary, copy `plugins/oauth-client-credentials/target/release/libagw_plugin_oauth_client_credentials.dylib`
to `plugins/oauth-client-credentials/plugin.dylib` instead.

```sh
mkdir -p plugins/oauth-client-credentials
cid=$(docker create --platform linux/amd64 ghcr.io/solo-io/agentgateway-plugins/oauth-client-credentials:<version> /plugin.so)
docker cp "$cid:/plugin.so" plugins/oauth-client-credentials/plugin.so
docker rm "$cid"
```

Reference the plugin by name under the route that sends traffic to the MCP
server. Standalone `order` values are camelCase (`early`,
`postAuthentication`, `postAuthorization`, `late`).

```yaml
binds:
- port: 3000
  listeners:
  - protocol: HTTP
    routes:
    - policies:
        plugins:
        - name: oauth-client-credentials
          order: late
          config:
            tokenUrl: https://idp.example.com/oauth2/token
            clientIdEnv: MCP_CLIENT_ID          # names of variables, not values
            clientSecretEnv: MCP_CLIENT_SECRET
            scope: mcp.read mcp.write
      backends:
      - host: mcp.internal.example.com:443
```

The config file is shell-expanded before parsing, comments included, so a
literal dollar sign anywhere in it is read as a variable. `clientIdEnv` and
`clientSecretEnv` name variables the plugin reads itself, so the secret never
appears in the file.

Run with the binary or the enterprise image:

```sh
export ENTERPRISE_AGENTGATEWAY_LICENSE_KEY=... MCP_CLIENT_ID=... MCP_CLIENT_SECRET=...
AGW_PLUGIN_PATH=$PWD/plugins agentgateway -f config.yaml

docker run -d --name agentgateway \
  -e ENTERPRISE_AGENTGATEWAY_LICENSE_KEY -e MCP_CLIENT_ID -e MCP_CLIENT_SECRET \
  -e AGW_PLUGIN_PATH=/var/lib/agentgateway/plugins \
  -v "$PWD/plugins:/var/lib/agentgateway/plugins:ro" \
  -v "$PWD/config.yaml:/config/config.yaml:ro" \
  -p 3000:3000 \
  us-docker.pkg.dev/solo-public/enterprise-agentgateway/agentgateway-enterprise:<agw-version> \
  -f /config/config.yaml
```

Verify that the plugin loaded (`loaded plugin name=oauth-client-credentials`
in the logs). A missing client ID or secret variable fails startup with an
error naming the variable.

## Deploy: Kubernetes

Create a secret in the gateway namespace:

```sh
kubectl create secret generic mcp-oauth --from-literal=client-id=... --from-literal=client-secret=...
```

Mount the plugin image and expose the secret to the gateway container. Reference the parameters from the Gateway and attach the policy to the HTTPRoute that routes to the MCP backend:

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayParameters
metadata:
  name: gateway-params
spec:
  plugins:
  - name: oauth-client-credentials
    image: ghcr.io/solo-io/agentgateway-plugins/oauth-client-credentials:<version>
  env:
  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef: {name: mcp-oauth, key: client-id}
  - name: OAUTH_CLIENT_SECRET
    valueFrom:
      secretKeyRef: {name: mcp-oauth, key: client-secret}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: mcp-gateway
spec:
  gatewayClassName: agentgateway
  infrastructure:
    parametersRef:
      group: enterpriseagentgateway.solo.io
      kind: EnterpriseAgentgatewayParameters
      name: gateway-params
  listeners:
  - name: http
    protocol: HTTP
    port: 80
---
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayPolicy
metadata:
  name: mcp-oauth
spec:
  targetRefs:
  - group: gateway.networking.k8s.io
    kind: HTTPRoute
    name: mcp-route
  traffic:
    entPlugin:
      name: oauth-client-credentials
      order: Late
      config:
        tokenUrl: https://identity.example.com/oauth/token
        clientIdEnv: OAUTH_CLIENT_ID
        clientSecretEnv: OAUTH_CLIENT_SECRET
        resource: https://mcp.example.com/
```

The policy config is a JSON object; Kubernetes does not expand environment references in it. The plugin reads the named variables itself. Kubernetes requires `Late`, while standalone uses `late`.

## Limitations

Each gateway replica caches and fetches its own token. mTLS client authentication and `private_key_jwt` are not supported. If the token endpoint is unavailable at startup, requests fail with `errorStatus` until it recovers. Plain HTTP to a non-loopback endpoint exposes credentials and produces a load-time warning.

## Future work

- Client authentication with mTLS (RFC 8705) and `private_key_jwt` (RFC 7523), for identity providers that don't issue client secrets.
- Refresh tokens in the background instead of on the request path, so no request waits for a token fetch after the first. The SDK background-data helper under review would provide this.
- Per-route or per-MCP-tool `scope` and `resource`, so one gateway can hold narrower tokens per backend instead of one broad token.
- A shared token across gateway replicas. Today each replica fetches its own, which multiplies token requests by the replica count.
- Built-in support: agentgateway's backend auth covers token exchange and JWT bearer grants but not client credentials. If this becomes a common need, it belongs in core backend auth rather than a plugin.
- The plugin sets `content-length` on the token request itself because the gateway otherwise sends it chunked. Once the SDK does this, remove it here.

## License

Apache-2.0. See [LICENSE](../../LICENSE).
