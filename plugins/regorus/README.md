# Regorus Plugin

This example evaluates inline Rego policy with
[Regorus](https://github.com/microsoft/regorus) from a Rust plugin.

| Field | Value |
| --- | --- |
| Language | Rust |
| Description | Evaluates inline Rego policy with Regorus and maps policy results to gateway actions. |
| Status | Demo |

## Purpose

The plugin compiles a Rego policy from plugin config and evaluates it for
each request. It builds policy input from the request method, path, query string,
and selected headers.

A policy result of `false` returns a direct `403` response. Any other result
continues the request and writes the result into the configured response header.

This is a reference for policy-engine integration and config-driven plugin setup.
It is not a complete production authorization plugin.

## Build And Run

From the repository root:

```sh
cargo build --release --manifest-path plugins/regorus/Cargo.toml
agentgateway oneshot -f plugins/regorus/config.yaml -- curl -s -H 'x-user: admin' http://127.0.0.1:3000/
agentgateway oneshot -f plugins/regorus/config.yaml -- curl -s http://127.0.0.1:3000/tenant
```

## Config

```yaml
plugins:
- name: regorus
  plugin: ./plugins/regorus/target/release/libagw_plugin_regorus.so
  config:
    resultHeader: x-rego-result
    rule: data.agentgateway.result
    policy: |
      package agentgateway

      default result := false

      result := "admin" if {
        input.headers["x-user"] == "admin"
      }
```

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
