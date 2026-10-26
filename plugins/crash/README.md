# Crash Plugin

Shows what happens when a plugin fails. Plugins run inside the gateway process,
so how they fail matters:

| Path | Plugin does | Result |
| --- | --- | --- |
| `/panic` | Rust `panic!` | The SDK catches it. That request gets a 503; the gateway keeps serving. |
| `/segfault` | Writes through a null pointer | The whole gateway process dies (SIGSEGV), dropping every connection. |
| anything else | Nothing | Continues upstream. |

| Field | Value |
| --- | --- |
| Language | Rust |
| Description | Shows what happens when a plugin fails. A Rust panic fails only the request; a segfault takes down the whole gateway. |
| Status | Demo |

**Don't run this in a shared environment.** `/segfault` kills the gateway.

## What it looks like

```console
$ curl -s -w ' %{http_code}\n' localhost:3000/panic
internal error 503
$ curl -s -o /dev/null -w '%{http_code}\n' localhost:3000/
200
$ curl -s localhost:3000/segfault
curl: (52) Empty reply from server
$ curl -s localhost:3000/
curl: (7) Failed to connect to localhost port 3000
```

The gateway logs the panic and the failed request, then carries on:

```
thread '<unnamed>' panicked at src/lib.rs:36:31:
crash plugin: /panic requested
error request ... http.path=/panic http.status=503 error="processing failed: plugin failed: plugin apply future panicked"
```

After `/segfault`, the gateway exits with status 139 (128 + SIGSEGV) and logs
nothing; in Kubernetes the pod restarts.

## Why

The Rust SDK wraps every call across the ABI in `catch_unwind`, so a panic
becomes an ordinary error for that request.

That's a safeguard, not error handling. Don't panic on purpose: return an
`Err` (or a direct response) instead. A caught panic still prints a backtrace
to stderr instead of a structured log, gives the client a generic 503, and can
leave shared state such as a `Mutex` poisoned for later requests.

Nothing can do the same for a
segfault, `abort()`, stack overflow, or running out of memory: they end the
process. That includes:

- `unsafe` Rust and C code with memory bugs.
- Plugins written against the C ABI directly, where there's no SDK to catch
  anything (a C++ exception escaping into the gateway is also fatal).
- Rust built with `panic = "abort"`, which turns every panic into a crash.

Treat plugins as trusted code, like the gateway itself. See
[SECURITY.md](../../SECURITY.md).

## Build and run

From the repository root:

```sh
cargo build --release --manifest-path plugins/crash/Cargo.toml
agentgateway -f plugins/crash/config.yaml
```

The config sends other requests to a backend on `127.0.0.1:8080`.

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
