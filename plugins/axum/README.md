# Axum Plugin

This example shows how to adapt an agentgateway plugin request into a
standard `http::Request` and pass it through an Axum `Router`.

| Field | Value |
| --- | --- |
| Language | Rust |
| Description | Adapts an agentgateway plugin request into an Axum router and streams router responses back to the gateway. |
| Status | Demo |

## Purpose

The SDK helper materializes request headers and uses the plugin body
stream ABI as the request body. That makes the conversion relatively expensive,
but it gives plugin authors a normal Rust HTTP integration point.

The Axum response body is returned as a streamed plugin direct response,
so the plugin does not need to buffer router responses before sending them back
to the gateway.

This is a reference for plugin authors who want to reuse familiar Rust HTTP
routing code inside a plugin.

## Build And Run

From the repository root:

```sh
cargo build --release --manifest-path plugins/axum/Cargo.toml
agentgateway oneshot -f plugins/axum/config.yaml -- curl -s http://127.0.0.1:3000/
agentgateway oneshot -f plugins/axum/config.yaml -- curl -s -X POST --data-binary hello http://127.0.0.1:3000/echo
```

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
