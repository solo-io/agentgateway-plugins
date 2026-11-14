# agentgateway Plugins

Native plugins for
[Solo Enterprise for agentgateway](https://docs.solo.io/agentgateway/). A plugin
is a shared library (`.so`) that the gateway loads at runtime to process requests
and responses.

> [!IMPORTANT]
> Plugins are an enterprise feature. They need Solo Enterprise for agentgateway
> and a license; open source agentgateway can't load them.

> [!WARNING]
> The plugin ABI and SDK are still changing. A plugin is only guaranteed to work
> with the agentgateway version it was built against.

## SDKs

Ways to write a plugin:

| SDK | Description | Status |
| --- | --- | --- |
| [Rust](./rust/sdk/) | Async Rust SDK with a mock host for tests. Recommended. | Solo.io maintained |
| [C ABI](./agentgateway_abi.h) | The raw ABI header. Usable from any language with a C FFI. See [hello-c](./plugins/hello-c/). | Solo.io maintained |
| [TypeScript](./plugins/quickjs/) | TypeScript or JavaScript loaded from config by the quickjs plugin. Nothing to compile. | Demo |

## Plugins

| Plugin | Description | Status |
| --- | --- | --- |
| [sdk-tour](./plugins/sdk-tour/) | Tour of the SDK. Start here. | Demo |
| [quickjs](./plugins/quickjs/) | Run TypeScript or JavaScript from config, with async access to the host API. | Demo |
| [regorus](./plugins/regorus/) | Allow or deny requests with inline [Rego](https://github.com/microsoft/regorus) policy. | Demo |
| [axum](./plugins/axum/) | Serve requests from an [Axum](https://github.com/tokio-rs/axum) router. | Demo |
| [hello-c](./plugins/hello-c/) | Minimal C plugin, written against the raw ABI. Adds a request header. | Demo |
| [crash](./plugins/crash/) | What happens when a plugin fails: a panic fails one request, a segfault kills the gateway. | Demo |

Status levels (for both SDKs and plugins):

- **Solo.io maintained**: supported by Solo.io, with versioned releases.
- **Community maintained**: maintained by contributors, best-effort support.
- **Demo**: an example to learn from, not for production.

## Using a plugin

For a full walkthrough, from writing a plugin to running it in Kubernetes, see
[DEPLOYING.md](./DEPLOYING.md).

Each plugin is published as an image containing only `/plugin.so`, for amd64 and
arm64:

```
ghcr.io/solo-io/agentgateway-plugins/<plugin>:<version | latest | main>
```

### Kubernetes

List the plugin image in `EnterpriseAgentgatewayParameters`. The controller
mounts it into the gateway pod as an
[image volume](https://kubernetes.io/docs/tasks/configure-pod-container/image-volumes/)
and sets `AGW_PLUGIN_PATH` for you. Then enable it with a policy that refers to
it by name:

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayParameters
metadata:
  name: gateway-params
spec:
  plugins:
  - name: regorus
    image: ghcr.io/solo-io/agentgateway-plugins/regorus:0.1.0
---
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayPolicy
metadata:
  name: regorus
spec:
  targetRefs:
  - group: gateway.networking.k8s.io
    kind: Gateway
    name: my-gateway
  traffic:
    entPlugin:
      name: regorus      # matches spec.plugins[].name above
      order: PostAuthz   # Early | PostAuth | PostAuthz | Late (default)
      config:            # passed to the plugin as JSON
        resultHeader: x-rego-result
        policy: |
          package agentgateway
          default result := false
          result := "admin" if {
            input.headers["x-user"] == "admin"
          }
```

The gateway uses the parameters through its `spec.infrastructure.parametersRef`.

### Standalone

Set `plugin` to the library file, or put each plugin in its own directory under
`AGW_PLUGIN_PATH` and refer to it by directory name:

```yaml
plugins:
- name: regorus
  plugin: ./libagw_plugin_regorus.so   # optional: defaults to `name`, looked up in
  config: {}                           # AGW_PLUGIN_PATH (e.g. /plugins/regorus/)
```

## Development

Each plugin is a standalone Cargo project under `plugins/`, built against the
SDK copied into `rust/`.

```sh
make test                        # test every plugin
make lint                        # clippy every plugin
make build                       # build every plugin
make docker PLUGIN=regorus       # build an image, tagged regorus:dev
make e2e PLUGIN=regorus          # run it in a real enterprise gateway (AGW_BIN, license key)

# try a plugin with its sample config
agentgateway oneshot -f plugins/regorus/config.yaml -- curl -s -H 'x-user: admin' localhost:3000/
```

To update the SDK, run `./sync-sdk.sh [path-to-agentgateway]`.

### Adding a plugin

Copy [`plugins/sdk-tour`](./plugins/sdk-tour/), then update its `plugin.yaml`.
CI builds, tests, and publishes every plugin that has one.

### Releasing

Plugins are versioned individually. Push a `<plugin>/v<version>` tag:

```sh
git tag regorus/v0.1.0 && git push origin regorus/v0.1.0
```

This publishes `regorus:0.1.0`, `regorus:0.1`, and `regorus:latest`. Every commit
to `main` also publishes `:main`.

## License

[Apache-2.0](./LICENSE), including the SDK in `rust/` and `agentgateway_abi.h`.
