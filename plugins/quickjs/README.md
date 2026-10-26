# QuickJS Plugin

Run TypeScript or JavaScript from config instead of compiling a plugin. The
code gets async access to the plugin host API: headers, bodies, metadata, CEL,
`sleep`, HTTP callouts, and logging.

| Field | Value |
| --- | --- |
| Language | Rust (runs user TypeScript/JavaScript) |
| Description | Run TypeScript or JavaScript from config, with async access to the plugin host API. |
| Status | Demo |

## Example

```yaml
plugins:
- name: quickjs
  plugin: ./plugins/quickjs/target/release/libagw_plugin_quickjs.so
  config:
    code: |
      async function onRequest(req: GatewayRequest): Promise<RequestResult> {
        if (req.header("x-api-key") !== "secret") {
          return { status: 401, body: "unauthorized\n" };
        }
        const user = await httpCall("http://users.internal/me", {
          headers: { authorization: req.header("authorization") ?? "" },
        });
        req.setHeader("x-user", JSON.parse(user.body).id);
        return { state: { start: Date.now() } };
      }

      const onResponse: OnResponse = (res, state) => {
        res.setHeader("x-elapsed-ms", String(Date.now() - state.start));
      };
```

## Writing code

Define `onRequest` and/or `onResponse` at the top level. Both may be async.
[`agentgateway.d.ts`](./agentgateway.d.ts) has the full API with doc comments;
point your editor at it for completion and type checking.

| `onRequest` returns | Effect |
| --- | --- |
| nothing | Continue to the upstream |
| `{ status, headers?, body? }` | Respond directly; the upstream isn't called |
| `{ state }` | Continue, and pass `state` (any JSON) to `onResponse` |

`onResponse(res, state)` can modify the response, or return
`{ status, headers?, body? }` to replace it.

| API | |
| --- | --- |
| `req.method`, `req.path`, `req.scheme`, `req.authority`, `res.status` | Request and response line |
| `header(name)`, `headers()`, `setHeader`, `addHeader`, `removeHeader` | Headers |
| `await text(limit?)`, `setBody(body)` | Body |
| `property(path)`, `setMetadata(key, value)`, `cel(expression)` | Gateway properties, metadata, and CEL |
| `res.setStatus(status)` | Response status |
| `await sleep(ms)` | Wait without blocking the gateway |
| `await httpCall(url, { method, headers, body })` | HTTP callout through the gateway |
| `log(level, message)`, `console.log/info/warn/error/debug` | Gateway logging |

## Config

| Field | Default | |
| --- | --- | --- |
| `code` | required | TypeScript or JavaScript source |
| `timeoutMs` | `1000` | Maximum execution time per phase, including awaited host calls |
| `memoryLimitBytes` | `16777216` | Maximum JavaScript heap per phase |

## How it works

- TypeScript types are stripped once, when the plugin loads, with
  [`swc_ts_fast_strip`](https://crates.io/crates/swc_ts_fast_strip) (the same
  approach as Node.js `--experimental-strip-types`). Types aren't checked, and
  TypeScript-only runtime features such as `enum` aren't supported. Plain
  JavaScript works unchanged.
- The code is also evaluated once at load time, so syntax errors and missing
  handlers fail plugin creation instead of every request.
- Each phase runs in a fresh [QuickJS](https://github.com/DelSkayn/rquickjs)
  runtime with the memory and time limits above. No JavaScript state is shared
  between requests, or between `onRequest` and `onResponse`; use `{ state }`.
- Host calls such as `sleep` and `httpCall` return promises backed by the
  gateway's own async runtime, so awaiting them doesn't block a thread.

## Performance

This plugin favors simplicity and isolation over speed, and isn't meant for
high-throughput paths. Every phase creates a new runtime and re-evaluates the
code, which costs about 50 µs before your handler runs (so about 100 µs per
request when `onResponse` is defined). For hot paths, write a native plugin
against the SDK instead.

A pool of reusable runtimes (as nginx does with `js_context_reuse`) would cut
that overhead to under 1 µs, but globals would then persist between requests
unpredictably, so it isn't implemented.

## Build and run

From the repository root:

```sh
cargo build --release --manifest-path plugins/quickjs/Cargo.toml
agentgateway oneshot -f plugins/quickjs/config.yaml -- curl -s localhost:3000/hello
```

The full sample in `config.yaml` also uses `sleep`, `httpCall`, and CEL, and
needs a backend on `127.0.0.1:8080`.

## Tests

```sh
cargo test --manifest-path plugins/quickjs/Cargo.toml
```

The tests run against the SDK's mock host, which doesn't implement CEL, `sleep`,
or HTTP callouts.

See the [repository README](../../README.md) for status definitions, prebuilt images, and build prerequisites.
