# SDK Tour Plugin

A single plugin that exercises most of the agentgateway plugin SDK. Read
[`src/lib.rs`](./src/lib.rs) top to bottom as a guided example.

| Field | Value |
| --- | --- |
| Language | Rust |
| Description | A tour of the plugin SDK, covering headers, metadata, CEL, sleep, logging, callouts, direct and streamed responses, and response-phase state. |
| Status | Demo |

## What it shows

On every request, the plugin:

- records the longest request header as plugin **metadata**, which is visible to
  the rest of the gateway through CEL (e.g. in access logs)
- evaluates the configured **CEL** expression, if any

Then it dispatches on the request path:

| Path | SDK features | Result |
| --- | --- | --- |
| `/direct` | Direct response | `200` from the plugin. The upstream isn't called. |
| `/echo` | Streaming request body, streamed direct response | The request body is echoed back chunk by chunk. |
| `/sleep` | Host `sleep` | Waits 100ms, then continues upstream. |
| `/callout` | Host `http_call`, `log` | Calls `calloutUrl`, then continues upstream with `x-tour-callout-status`. |
| anything else | Request header mutation | Continues upstream with `x-tour-action: continue`. |

Requests that continue upstream also run the **response phase**. State from the
request phase is passed to `on_response`, which adds these headers:

```
x-tour-action: sleep
x-tour-upstream-status: 200
x-tour-elapsed-ms: 104
x-tour-cel: "curl/8.11.0"
```

## Config

```yaml
plugins:
- name: sdk-tour
  plugin: ./plugins/sdk-tour/target/release/libagw_plugin_sdk_tour.so
  config:
    calloutUrl: http://127.0.0.1:8080/   # required: absolute URL called by /callout
    cel: request.headers["user-agent"]  # optional
```

## Build and run

From the repository root:

```sh
cargo build --release --manifest-path plugins/sdk-tour/Cargo.toml
agentgateway oneshot -f plugins/sdk-tour/config.yaml -- curl -s http://127.0.0.1:3000/direct
agentgateway oneshot -f plugins/sdk-tour/config.yaml -- curl -s --data-binary hello http://127.0.0.1:3000/echo
agentgateway oneshot -f plugins/sdk-tour/config.yaml -- curl -si http://127.0.0.1:3000/sleep
```

`/sleep`, `/callout`, and the fallback route need a backend on `127.0.0.1:8080`.

## Tests

The unit tests run the plugin against the SDK's mock host
(`agentgateway_sdk::testing`, enabled with the `testing` feature), without a
gateway:

```sh
cargo test --manifest-path plugins/sdk-tour/Cargo.toml
```

The tests live in [`src/tests.rs`](./src/tests.rs). The mock host doesn't
implement CEL, `sleep`, or HTTP callouts, so the tests
cover direct responses, header mutation, metadata, and the response phase.

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
