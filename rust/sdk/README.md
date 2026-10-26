# Agentgateway Plugin SDK

Agentgateway plugins are native plugins that run inside the agentgateway process.

They are intended for policy and request/response plugin points that need more control than CEL and lower latency than an
out-of-process service

Plugins are loaded from shared libraries, such as `libexample.so`, and are
called by agentgateway through a stable C ABI.
This SDK provides higher-level APIs over that ABI; it is strongly recommended to use the SDK rather than the raw ABI.

![Plugin architecture][plugin-architecture]

[plugin-architecture]: diagram.png

_* Go SDK is just an example; only a Rust SDK is currently available._

Plugins can:

- inspect and mutate request and response headers
- read, replace, or stream request and response bodies
- return a direct response
- evaluate CEL expressions and mutate CEL metadata
- make host HTTP callouts
- access host functionality such as `sleep`, `log`, etc

The plugin system is native and trusted. It is not a sandbox. A plugin should
be treated like code linked into agentgateway.

## Getting started

A plugin is a `cdylib` crate that depends on `agentgateway-sdk` and exports itself with `export_plugin!`.

### Anatomy of a plugin

A plugin has a few high level parts: instantiation, request handling, response handling, etc.

**Instantiation** builds a new plugin instance. This happens once when the plugin is loaded (_not_ per request).

```rust,ignore
pub trait HttpPluginFactory: HttpPlugin + Sized {
	fn new(config: serde_json::Value) -> Result<Self>;
}
```

For simple plugins, the plugin can simply implement `serde::Deserialize`.

Plugins implement the **HttpPlugin** trait which exposes methods call on each request/response.

```rust,ignore
pub trait HttpPlugin: Send + Sync + 'static {
	type ResponseState: Send + 'static;

	async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction>;

	async fn on_response( &self, state: Self::ResponseState, response: ResponseContext) -> Result<HttpResponseAction>;
}
```

The `on_request` runs on each request and gives the ability to access or mutate the request.
If the plugin needs to retain state between the request and response, it can pass the information in `ResponseState`;
if no state is needed `()` can be used.

`on_response` runs on the response side on each response, and gives the ability to access or mutate the response.

**Exporting** the plugin is the final piece to register our plugin as a plugin.
This takes the plugin type and name and a small macro to handle registration:

```rust,ignore
export_plugin!(HelloWorldPlugin, name = "hello-world");
```

Putting this all together, a full end to end example:

```rust,ignore
#[derive(Debug, Deserialize)]
struct HelloWorldPlugin {
    /// The message we will return, or a default
    message: Option<String>,
    /// Counter incremented per-request
    #[serde(skip)]
    counter: Arc<AtomicUsize>,
}

struct ResponseState {
    path: String,
}

#[async_trait]
impl HttpPlugin for HelloWorldPlugin {
    type ResponseState = ResponseState;

    async fn on_request(
        &self,
        request: RequestContext,
    ) -> Result<HttpRequestAction<Self::ResponseState>> {
        agentgateway_sdk::sleep(Duration::from_secs(1)).await?;
        // Set a header based on a stateful counter. Each request will increment.
        request.set_header(
            HeaderName::from_static("x-agentgateway-request-count"),
            HeaderValue::from_str(&format!(
                "{}: {}",
                self.counter.fetch_add(1, Ordering::Relaxed),
                self.message.as_deref().unwrap_or("")
            ))?,
        )?;
        Ok(HttpRequestAction::Continue {
            response: ResponsePhase::Run(ResponseState {
                // Pass the original path into the response handling for referencing later.
                // Note: this can
                path: request
                  .path()
                  .map(|p| p.to_string())
                  .unwrap_or_else(|| "/".to_string()),
            }),
        })
    }

    async fn on_response(
        &self,
        state: Self::ResponseState,
        response: ResponseContext,
    ) -> Result<ResponseAction> {
        // note: response.property(&["request", "path"]) could also get this information
        response.set_header(
            HeaderName::from_static("x-agentgateway-plugin-original-path"),
            HeaderValue::from_str(&state.path)?,
        )?;
        Ok(ResponseAction::Continue)
    }
}

export_plugin!(HelloWorldPlugin, name = "hello-world");
```

## Safety and Responsibilities

Plugins run in the same process as agentgateway. This gives them low overhead
and direct access to host-provided request operations, but it also means plugin
bugs can affect the gateway process.

Plugin authors are responsible for:

- memory safety inside unsafe code
- avoiding panics in production paths
- avoiding blocking work on hot request paths
- bounding CPU, memory, and body buffering
- handling malformed input explicitly
- returning errors instead of assuming inputs are valid
- not leaking host-owned ABI objects
- dropping futures, streams, and response sessions correctly
- keeping plugin dependencies patched

**The plugin ABI is not automatically memory safe**.
Writing correct code in a memory-unsafe language (C) is extremely challenging.
We strongly recommend building plugins with the provider SDKs which provide safe wrappers on top of the C ABI.

Rust plugins built with the normal SDK are safer than raw C plugins, but they
are still native code. The SDK catches Rust unwinds at exported ABI boundaries
for the standard SDK path, converting panics into plugin errors where possible.

## Async and Tokio

Plugin callbacks are `async`, but they don't run on a Tokio runtime. The host
polls your futures directly, and your plugin links its own copy of Tokio, which
can't see the host's runtime anyway.

Anything that only needs a waker works fine: `tokio::sync` channels and locks,
`join!`/`select!`, and `futures` combinators. Anything that needs a Tokio
reactor or timer doesn't: `tokio::time::sleep`, `tokio::spawn`, `tokio::net`,
and `spawn_blocking` all panic with "there is no reactor running", which fails
the request.

For the common cases, use the host instead: `agentgateway_sdk::sleep` for
timers and `http_call` for HTTP. They need no extra threads, and request
callouts show up in the request's traces.

If you really need Tokio (say, a client library built on it), build one runtime
per process and spawn onto it. You can await the `JoinHandle` from
`on_request`:

```rust,ignore
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
});

let request2 = request.clone();
RUNTIME
    .spawn(async move {
        let stream = tokio::net::TcpStream::connect("10.0.0.1:9000").await?;
        // ...
        request2.set_header(name, value)
    })
    .await??;
```

`RequestContext` and `ResponseContext` can be cloned and moved to other threads
or tasks. They work until that request (or response) phase ends, and return an
error after that.

Work that isn't tied to a request, like a background thread that batches and
flushes data, can use the free `agentgateway_sdk::http_call`. It doesn't need a
context or a runtime; the host runs the call for you.

## Picking a Language

### Rust

Rust is the best option for plugins.
It has the best SDK, strong type safety, the best async integration, and high performance.
Selecting another option is almost always a bad option.

### C

C is the smallest and lowest-level option, but it has the highest safety cost.
Use it for very small plugins, ABI experiments, or when wrapping an existing C
library directly.

Agentgateway does not provide a high level C SDK, only the low level ABI.
This places full ownership to the plugin author, including manual implementation of futures in C.

### Other Languages

Other languages can target the ABI if they can build a native shared library
and obey the C calling convention. The same safety and lifecycle rules apply.
Before adding another SDK, make sure the language runtime can handle in-process
loading, callback-driven polling, and clean shutdown without surprising the
host.

## Picking a Plugin Mechanism

Agentgateway has several plugin mechanisms. Pick the smallest one that fits
the problem.

### CEL

Use CEL when the policy can be expressed as data and pure expressions. CEL is
best for selection, authorization predicates, header/value matching, small
computed metadata values, and configuration-driven logic.

CEL is usually the best first choice when:

- the logic is simple and deterministic
- there is no need to call arbitrary libraries
- there is no need to own streaming body transformation
- the policy should be easy to inspect and manage as configuration
- operational safety matters more than arbitrary programmability

In general, CEL performance should be equivalent to plugins for most functionality.

### External Services

Use an external service when isolation, independent deployment, or operational
decoupling matters more than per-request overhead. External services can be
implemented with normal server frameworks, can have their own runtime, and can
be scaled, rolled out, and monitored independently.

External services are usually the best choice when:

- the plugin is not fully trusted
- a crash must not affect agentgateway
- the plugin has heavy CPU or memory use
- the plugin needs its own async runtime, thread pools, database clients, or background tasks
- the plugin is shared across many gateways
- independent deployment and rollback are important

The tradeoff is that external services add network hops, serialization,
timeouts, backpressure concerns, and another operational component. For simple
policy checks, CEL is often cheaper. For hot-path custom logic, a plugin can be
cheaper.

### Native Plugins

Use a plugin when the logic should run in-process, needs a real programming
language, and is trusted enough to share fate with agentgateway.

Plugins are usually the right choice when:

- the plugin needs a native library, such as a parser, WAF, or classifier
- the plugin must transform streaming bodies
- the plugin is latency-sensitive

Plugins are not a substitute for sandboxing. If the code is untrusted or
failure isolation is mandatory, use an external service or a sandboxed runtime
instead.

## Testing

Enable the `testing` feature in `[dev-dependencies]` to unit test a plugin
against an in-process mock host, without building or loading a shared library.
See the `agentgateway_sdk::testing` module: `TestEnvironment` runs
`on_request`/`on_response` with a `TestRequest`/`TestResponse`, then exposes the
resulting headers, body, metadata, and logs for assertions.

## Versioning

The plugin ABI exposes an ABI version.
Breaking changes to the ABI will result in the version being incremented.
New features may be added without an ABI version bump if they are backwards compatible; however, plugins
attempting to use these features on a host that is too old will fail.

Below shows the ABI versions:

| Version | First Agentgateway Version | Description |
|---------|----------------------------|-------------|
| `1`     | `v1.7`                     | Initial ABI |

An ABI version will only be bumped during a minor release (i.e. `v1.3` to `v1.4`), not a patch release.
