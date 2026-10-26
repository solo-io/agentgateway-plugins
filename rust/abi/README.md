# agentgateway Plugin ABI

This crate is the low-level C boundary for native plugins.

See [plugins](../../plugins) for user-facing documentation on writing plugins.

Plugins are trusted code loaded into the agentgateway process. The ABI is a
compatibility boundary, not a security boundary.

## Versioning

`AGW_PLUGIN_ABI_VERSION` is the compatibility line. If a change can break an
already-built plugin, bump it and regenerate the checked-in header.

Structs that may grow start with a `size` field (`AgwSized`): the host
vtable, plugin descriptor, plugin instance, apply result, body chunk, and HTTP
callout request/response. New fields are appended, so they can be added without a
version bump:

- Reading a struct the other side produced (`read_sized`): copy only the bytes
  both sides know about; fields past `size` keep their defaults, so a missing
  callback reads as unsupported.
- Writing into an output the other side allocated (`write_sized`): the consumer
  sets `size` to its capacity, and the producer writes only that many bytes and
  sets `size` to what it wrote.

A `size` below the type's first-version layout (`AgwSized::MIN_SIZE`) is
rejected, so every original field, including ownership fields, is always
present.

When adding a field:

- append it, starting at or after the previous struct size, never in the old
  layout's tail padding;
- don't let it own resources on its own, since an older consumer never sees
  it. Whatever it points to must be released by an owner that already existed,
  such as `private_data`;
- structs embedded by value in another struct (`AgwDirectResponse`,
  `AgwPluginResponseSession`, `AgwError`, `AgwBodyStream`, ...) are frozen.
  Extend the outer sized struct instead.

The plugin entrypoint returns `const AgwPluginDescriptor *` pointing at static
data, so the descriptor can grow as well.

## Ownership Model

The ABI has two kinds of data:

- Borrowed views: valid only for the documented call or callback scope.
- Owned handles: opaque state plus a matching drop callback.

Do not infer ownership from pointer shape. If a plugin needs data after a call
returns, it must copy it. If the ABI returns a handle with a drop callback, that
callback owns cleanup and must be called at most once.

`AgwDropOwned` is always paired with an opaque owner pointer. It is not a
general-purpose finalizer; call it only with the exact pointer in the same ABI
value, and only while the library that produced it is still loaded.

This rule is more important than the individual structs. New ABI types should
fit one of these two models.

## Strings

`AgwString` is a borrowed UTF-8 slice, not a C string. It is not
NUL-terminated, and `as_str()` creates a borrow whose lifetime is only as long
as the ABI value that supplied it.

Host code should either consume that borrow immediately or copy it before
storing it, moving it into an async future, or attaching it to request state.
The same rule applies to plugin SDKs reading host-owned strings.

## Status Values

Hostcalls use `AGW_STATUS_OK` or `AGW_STATUS_ERROR`. Missing optional data is
not a third status; getters return `OK` with an empty/null output handle. If an
operation fails, return `ERROR` and populate `AgwError` when the call has an
error output.

`AgwError.code` is zero for no error and `AGW_ERROR_UNKNOWN` otherwise. More
specific codes may be added later; treat any unknown nonzero code as a generic
error.

Output parameters (`AgwOwnedBytes`, `AgwError`, and the like) follow three
rules, so a caller can always clean up the same way:

- The caller initializes every output before the call to its default (in
  Rust, its `Default`): all zero, except that a sized output (one starting with
  a `size` field) has `size` set to the struct size the caller allocated. The
  callee reads nothing from an output except that `size`.
- The callee leaves an output either untouched or fully written. It may write
  an output on any status, including writing an empty error on success.
- After the call returns, whatever the status (including `PENDING` from a
  poll), the caller frees each output that has a `drop` callback set. Freeing
  a default output does nothing.

## CEL Values

`eval_cel` and `get_property` return an `AgwCelResult`: an `AgwCelValue` tree
plus the owner that frees it. `set_metadata` takes a borrowed `AgwCelValue`,
valid only for the call; the host copies it, and it reads back with the same
types. Each CEL type has its own kind (`int`, `uint` and
`double` are distinct, and `type(1)` is `AGW_CEL_TYPE`, not the string
`"int"`). Map keys can be `bool`, `int`, `uint` or `string`. `kind` says which
fields are set:

```c
// {"user": "alice", 7: 1.5}
AgwCelValue entries[4] = {
    { .kind = AGW_CEL_STRING, .value.bytes = { "user", 4 } },
    { .kind = AGW_CEL_STRING, .value.bytes = { "alice", 5 } },
    { .kind = AGW_CEL_INT, .value.int_value = 7 },
    { .kind = AGW_CEL_DOUBLE, .value.double_value = 1.5 },
};
AgwCelValue map = { .kind = AGW_CEL_MAP, .value.items = { entries, 2 } }; // len counts entries
```

A zeroed value is `AGW_CEL_UNSET`, which means "no value" and is distinct from
CEL `null`. It appears only as the root of an output: `get_property` leaves it
for a property that does not exist. It is never nested inside a list or map.

Maps have no duplicate keys; an `int` and a `uint` with the same numeric value
are the same key, as in CEL. Entry order is not meaningful.

An `AgwCelResult` owns one tree:

- The tree is immutable and stays valid until the result is freed, which may
  happen on any thread.
- Nothing in the tree points into the `AgwCelResult` struct itself, so the
  struct may be moved. It may not be duplicated: copying it does not copy the
  ownership, and only one copy may be freed.
- Values borrowed from a result (for example, a pointer to a string) are valid
  until that result is freed.

Some CEL values have no ABI form, such as `ip(...)` or `optional` values; a
hostcall returning one fails with an error instead.

`AgwCelValue` is 24 bytes and stored in arrays, so its layout is frozen: future
kinds use the payload union (or point to a separate struct), and a receiver
rejects kinds or `flags` it does not know.

## Execution Context

Request and response hostcalls take an explicit `const AgwContext *`, the
opaque handle the host passed to `apply` (or to a response session's `apply`).
A context:

- is valid from that call until the returned future completes or is dropped;
- may be used from any thread while valid, including while the future is not
  being polled (for example, from a goroutine), but never from two threads at
  once;
- must not be used after the future completes or is dropped. A plugin's future
  `drop` must stop any other work that still holds the context.

Hostcalls that do not act on a request or response (`sleep`, `compile_cel`,
`log`) take the vtable `data` pointer instead and may be called at any time.

A body stream handed to the host with `set_body_stream` is polled after the
future completes, so it cannot use the context. Body streams the host takes
(`set_body_stream`, direct responses, `http_call` request bodies) may also
outlive the plugin instance that created them, for example after a config
reload drops the instance mid-response. A stream must own, or share ownership
of, everything it uses; it must not point into instance state. The plugin
library itself is never unloaded, so its code stays valid.

Response processing is represented as a plugin-owned response session returned
from the request phase. The host stores that session with the in-flight request
and runs it once when the final response is available. If response processing is
skipped, the session is dropped.

## Wakers

Poll functions receive a borrowed `const AgwRawWaker *`, valid only for that
call. To be woken later, clone it through its vtable; the clone is owned and
must be released with `drop` or consumed with `wake`. Rust code uses
`with_borrowed_waker` (caller) and `with_context` (callee).

## Bodies

Body access is lazy. If a plugin does not read or replace a body, the host
continues streaming the original body.

The ABI exposes bodies as one-shot streams of `AgwBodyChunk` values. Data and
trailers are separate frames, followed by an end frame. There is no buffered
body ABI; SDKs may provide `collect` helpers on top of the stream.

Taking the body stream removes it from the request or response. If a plugin
reads the body without calling `set_body_stream`, nothing is sent on. The Rust
SDK's `collect` hands the buffered bytes back automatically; plugins that read
chunk by chunk must replace the body themselves.

When a plugin passes a body stream to the host (`set_body_stream`, or the
request body of `http_call`), the host always takes ownership, whatever status
the call returns. The plugin must not drop or poll the stream afterwards, even
if the call failed.

`http_call` also accepts a null context, for calls made outside any request
(e.g. from a background thread).

Direct responses and HTTP callouts carry body streams as nested handles. A
handle that can be taken out of a result (a body stream, a response session)
must not depend on the result's owner: the receiver takes it, clears the field,
and then frees the result, and the handle must stay valid. To take a handle,
copy it out and zero the original, so freeing the result skips it.

## Runtime Model

The host polls plugin futures directly. It does not enter a Tokio runtime for
the plugin and does not provide Tokio IO, timers, or spawning.

Hostcalls are the portability boundary for async work that needs proxy
integration: timers, HTTP callouts, body reads, cancellation, and request
context. Plain async combinators are fine when they only poll futures.

Current behavior to keep in mind:

- `tokio::select!` over host futures works.
- Simple `tokio::sync::mpsc` send/recv works.
- `tokio::spawn`, `tokio::net::TcpStream`, and `tokio::time::sleep` require a
  Tokio runtime and fail unless the plugin provides one.

To provide one, the plugin owns its own runtime: create it lazily in a static,
spawn the work onto it, and await the `JoinHandle`. A `JoinHandle` can be
awaited from any executor, so the request future can wait on it while the host
polls it:

```rust
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("plugin runtime")
});

async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<()>> {
    let answer = RUNTIME.spawn(lookup_over_tcp()).await??;
    // ... use `answer` ...
    Ok(HttpRequestAction::Continue { response: ResponsePhase::Skip })
}
```

Don't call `RUNTIME.block_on` from a request; that blocks a host thread.

For background work that only needs to make HTTP requests, such as flushing
buffered data, it is simpler to skip the runtime and call
`agentgateway_sdk::http_call`, which works outside of any request.

## Threading

Plugins run on the host's multi-threaded runtime:

- A plugin instance's `apply` is called concurrently from many threads.
  Instance state must be safe for shared use.
- Futures, response sessions, and body streams may be created on one thread
  and polled or dropped on another, but never concurrently.
- Wakers, including clones handed to the host, may be woken or dropped from
  any thread.
- Host-provided futures and streams follow the same rules.

## Unsafe Contract

The ABI assumes both sides are honest and correct:

- pointers are valid for the documented lengths
- UTF-8 strings are actually UTF-8
- borrowed data is not retained past its lifetime
- owned handles are dropped exactly once
- futures are not polled after completion
- pending futures wake when progress is possible
- plugin libraries remain loaded while any plugin-owned callback may still run

Violating these rules is a bug in the host, the plugin, or the SDK layer. The
ABI does not try to defend against it.

## Header Generation

`agentgateway_abi.h` is generated by the repo generation flow using `cbindgen`:

```sh
make generate-abi
```

or:

```sh
cargo xtask abi
```

Header generation is fail-open. If `cbindgen` cannot parse the crate or config,
the task emits a warning and continues. The checked-in header is the source for
C and Go users. Building `agentgateway-abi` itself does not run `cbindgen`; ABI
importers should not need the header generator.

The header should parse as C11:

```sh
printf '#include "crates/abi/agentgateway_abi.h"\n' \
  | clang -x c -std=c11 -fsyntax-only -I. -
```
