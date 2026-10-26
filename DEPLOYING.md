# Building and deploying a plugin

This guide takes a plugin from source to a running gateway:

1. [Create a plugin](#1-create-a-plugin) from the `sdk-tour` example
2. [Test it](#2-test-it) with unit tests and a local gateway
3. [Build and push an image](#3-build-and-push-an-image)
4. [Mount it into the gateway](#4-mount-it-into-the-gateway) with `EnterpriseAgentgatewayParameters`
5. [Enable it](#5-enable-it-with-a-policy) with an `EnterpriseAgentgatewayPolicy`
6. [Verify](#6-verify) and [update](#7-update) it

The examples use a plugin named `my-plugin` and a registry at
`registry.example.com`. Replace both with your own.

## Prerequisites

- Rust (stable) and Docker with `buildx`
- For local testing: an enterprise `agentgateway` binary and a license key in
  `ENTERPRISE_AGENTGATEWAY_LICENSE_KEY`
- For deploying: a Kubernetes cluster running Enterprise Agentgateway, with
  [image volume](https://kubernetes.io/docs/tasks/configure-pod-container/image-volumes/)
  support (the `ImageVolume` feature and a container runtime that supports it)

## 1. Create a plugin

Copy `sdk-tour` and rename it:

```sh
cp -r plugins/sdk-tour plugins/my-plugin
cd plugins/my-plugin
rm -rf target Cargo.lock

sed -i 's/agw-plugin-sdk-tour/agw-plugin-my-plugin/' Cargo.toml
sed -i 's/export_plugin_with_factory!(SdkTour, "sdk-tour")/export_plugin_with_factory!(SdkTour, "my-plugin")/' src/lib.rs
sed -i 's#sdk-tour#my-plugin#g; s#libagw_plugin_sdk_tour#libagw_plugin_my_plugin#' config.yaml
```

Then edit:

- `src/lib.rs`: your plugin logic. `on_request` and `on_response` are the entry
  points; the SDK docs are in [`rust/sdk`](./rust/sdk/).
- `src/tests.rs`: unit tests.
- `plugin.yaml`: the description and status. CI builds, tests, and publishes
  every plugin that has one.
- `README.md`: what the plugin does and its config options.

## 2. Test it

Run the unit tests. They use the SDK's mock host, so no gateway is needed:

```sh
cargo test
```

To try the plugin in a real gateway, build it and run the sample config from the
repository root:

```sh
cargo build --release --manifest-path plugins/my-plugin/Cargo.toml
agentgateway oneshot -f plugins/my-plugin/config.yaml -- curl -si localhost:3000/direct
```

> [!NOTE]
> agentgateway may write a generated session key into the config file it was
> started with. Don't commit it.

The local config references the library by path:

```yaml
plugins:
- name: my-plugin
  plugin: ./plugins/my-plugin/target/release/libagw_plugin_my_plugin.so
  config:
    calloutUrl: http://127.0.0.1:8080/
```

## 3. Build and push an image

The image contains only `/plugin.so`. Build it for both architectures and push:

```sh
docker buildx build --platform linux/amd64,linux/arm64 \
  --build-arg PLUGIN=my-plugin \
  -t registry.example.com/my-plugin:0.1.0 --push .
```

Use a new tag for every change. Kubernetes caches image volumes by tag, so
pushing a changed image under an existing tag may not reach running gateways.

## 4. Mount it into the gateway

List the image under `spec.plugins` in an `EnterpriseAgentgatewayParameters`.
The controller mounts each entry into the gateway pod at
`/var/lib/agentgateway/plugins/<name>` and sets `AGW_PLUGIN_PATH` to match.

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayParameters
metadata:
  name: plugins
  namespace: default
spec:
  plugins:
  - name: my-plugin
    image: registry.example.com/my-plugin:0.1.0
    pullPolicy: IfNotPresent
```

Point the gateway at the parameters:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: my-gateway
  namespace: default
spec:
  gatewayClassName: enterprise-agentgateway
  infrastructure:
    parametersRef:
      group: enterpriseagentgateway.solo.io
      kind: EnterpriseAgentgatewayParameters
      name: plugins
  listeners:
  - name: http
    port: 8080
    protocol: HTTP
```

If the registry is private, add a pull secret to the gateway pod in the same
parameters:

```yaml
spec:
  plugins:
  - name: my-plugin
    image: registry.example.com/my-plugin:0.1.0
  deployment:
    spec:
      template:
        spec:
          imagePullSecrets:
          - name: my-registry-credentials
```

## 5. Enable it with a policy

Mounting a plugin doesn't run it. Attach it to traffic with an
`EnterpriseAgentgatewayPolicy`. The target can be a `Gateway` or an `HTTPRoute`.

```yaml
apiVersion: enterpriseagentgateway.solo.io/v1alpha1
kind: EnterpriseAgentgatewayPolicy
metadata:
  name: my-plugin
  namespace: default
spec:
  targetRefs:
  - group: gateway.networking.k8s.io
    kind: Gateway
    name: my-gateway
  traffic:
    entPlugin:
      name: my-plugin          # matches spec.plugins[].name in the parameters
      order: Late              # Early | PostAuth | PostAuthz | Late
      config:                  # passed to the plugin factory as JSON
        calloutUrl: http://my-service.default.svc.cluster.local/
```

`name` identifies this use of the plugin. To run the same library twice with
different config, give each policy a different `name` and set `plugin` to the
name from the parameters:

```yaml
    entPlugin:
      name: my-plugin-strict
      plugin: my-plugin
      config: { ... }
```

## 6. Verify

Check that the library is mounted and found:

```sh
kubectl get deploy my-gateway -o yaml | grep -A1 AGW_PLUGIN_PATH
kubectl exec deploy/my-gateway -- ls /var/lib/agentgateway/plugins/my-plugin
kubectl logs deploy/my-gateway | grep -i plugin
```

Then send a request through the gateway:

```sh
kubectl port-forward deploy/my-gateway 8080:8080 &
curl -si localhost:8080/direct
```

The policy's status also reports whether it was accepted and attached:

```sh
kubectl get enterpriseagentgatewaypolicy my-plugin -o yaml
```

## 7. Update

| To change | Do this | Effect |
| --- | --- | --- |
| Plugin config | Edit `entPlugin.config` in the policy | Applied by the running gateway |
| Plugin code | Push a new tag, then update `image` in the parameters | The gateway pods roll out with the new image |
| Where it runs | Edit the policy's `targetRefs` or `order` | Applied by the running gateway |
| Remove it | Delete the policy, then the `spec.plugins` entry | Removing the entry rolls out the pods |

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `failed to initialize: missing field ...` | `entPlugin.config` doesn't match what the plugin's factory expects. |
| Library not found | `entPlugin.name` (or `plugin`) doesn't match a `spec.plugins[].name`, or the image doesn't contain exactly one `.so`. |
| Pod stuck in `ContainerCreating` | Image volumes aren't supported by the cluster, or the image can't be pulled. Check `kubectl describe pod`. |
| Undefined symbol or ABI errors | The plugin was built against a different SDK version than the gateway. Rebuild it against the SDK that matches your gateway release. |
