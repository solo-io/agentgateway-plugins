# Hello C Plugin

The smallest useful plugin: about 40 lines of C, written directly against the
plugin ABI ([`agentgateway_abi.h`](../../agentgateway_abi.h)) with no SDK. It
adds `x-hello: world` to every request before it's sent upstream.

| Field | Value |
| --- | --- |
| Language | C |
| Description | Minimal C plugin, written directly against the ABI, that adds a request header. |
| Status | Demo |

## How it works

[`hello.c`](./hello.c) implements the four pieces every plugin needs:

1. `agentgateway_plugin_entrypoint` returns a static descriptor with the ABI
   version, name, and `create` function.
2. `create` runs once at load time. It saves the host's `set_header` callback
   and returns an instance with an `apply` function.
3. `apply` runs per request and returns a future. Here the future's state is
   just the request context.
4. `poll` sets the header and reports `AGW_APPLY_CONTINUE`, so the request
   carries on to the upstream.

## Build and run

From the repository root:

```sh
make -C plugins/hello-c
agentgateway oneshot -f plugins/hello-c/config.yaml -- curl -s localhost:3000/
```

The config sends requests to a backend on `127.0.0.1:8080`. Use one that echoes
request headers to see `x-hello: world`.

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
