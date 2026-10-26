use super::*;

static HOST: OnceLock<Host> = OnceLock::new();

thread_local! {
	// The host of the plugin callback running on this thread, if any. Preferred
	// over `HOST` so in-process test hosts (one per `TestEnvironment`) stay
	// separate; real hosts pass the same vtable everywhere.
	static CALLBACK_HOST: Cell<Option<Host>> = const { Cell::new(None) };
	// Set while this thread is inside a hostcall made through a `ContextHandle`, so
	// a re-entrant hostcall (e.g. from a `Drop` the host runs) errors instead of
	// deadlocking on the handle's lock.
	static IN_HOSTCALL: Cell<bool> = const { Cell::new(false) };
}

/// Records the host vtable for hostcalls that do not act on a request or
/// response (`sleep`, `log`, `compile_cel`). The host passes the same vtable to
/// every plugin instance.
pub(crate) fn set_host(host: Host) {
	let _ = HOST.set(host);
}

/// Returns the host vtable for request-independent hostcalls.
pub(crate) fn host() -> Result<Host> {
	CALLBACK_HOST
		.get()
		.or_else(|| HOST.get().copied())
		.ok_or_else(|| Error::msg("plugin has not been created by a host"))
}

/// Runs a plugin callback (`create`, a future poll) with `host` as the
/// preferred host for request-independent hostcalls.
pub(crate) fn with_host<T>(host: Host, f: impl FnOnce() -> T) -> T {
	struct Restore(Option<Host>);
	impl Drop for Restore {
		fn drop(&mut self) {
			CALLBACK_HOST.set(self.0);
		}
	}
	let _restore = Restore(CALLBACK_HOST.replace(Some(host)));
	f()
}

/// One request or response phase's host context, shared by every clone of its
/// [`RequestContext`] or [`ResponseContext`].
///
/// The ABI allows a context to be used from any thread while its future is
/// alive, but never concurrently and never afterwards. The lock enforces both:
/// every hostcall holds it, and the SDK marks the context dead (under the lock)
/// before the host can free it.
pub(crate) struct ContextHandle {
	host: Host,
	ctx: *const abi::AgwContext,
	live: Mutex<bool>,
}

// SAFETY: `ctx` is only passed to hostcalls while `live` is held and true, which
// serializes access and guarantees the host has not freed it.
unsafe impl Send for ContextHandle {}
// SAFETY: See `Send`.
unsafe impl Sync for ContextHandle {}

impl ContextHandle {
	pub(crate) fn host(&self) -> &Host {
		&self.host
	}

	pub(crate) fn new(host: Host, ctx: *const abi::AgwContext) -> Arc<Self> {
		Arc::new(Self {
			host,
			ctx,
			live: Mutex::new(!ctx.is_null()),
		})
	}

	/// Marks the context dead, waiting for any in-flight hostcall. Must be called
	/// before the host may free the context.
	pub(crate) fn invalidate(&self) {
		*self.live.lock().unwrap_or_else(PoisonError::into_inner) = false;
	}

	/// Runs one hostcall against this context, if it is still valid.
	pub(crate) fn call<T, R>(
		&self,
		select: impl FnOnce(&abi::AgwHostVTable) -> Option<T>,
		missing: &'static str,
		f: impl FnOnce(T, *const abi::AgwContext) -> Result<R>,
	) -> Result<R> {
		let Some(callback) = select(&self.host.vtable) else {
			return Err(Error::msg(missing));
		};
		if IN_HOSTCALL.get() {
			return Err(Error::msg(
				"hostcalls cannot be made from inside another hostcall",
			));
		}
		let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
		if !*live {
			return Err(Error::msg(
				"plugin context is no longer valid; its request or response phase has ended",
			));
		}
		struct Reset;
		impl Drop for Reset {
			fn drop(&mut self) {
				IN_HOSTCALL.set(false);
			}
		}
		IN_HOSTCALL.set(true);
		let _reset = Reset;
		let result = f(callback, self.ctx);
		drop(live);
		result
	}
}

#[derive(Clone, Copy)]
pub(crate) struct Host {
	pub(crate) vtable: abi::AgwHostVTable,
}

// SAFETY: `Host` is a copy of the host vtable. The host owns `data` and all
// callbacks, and callbacks are required by the ABI contract to be safe to call
// from any thread for the lifetime of the loaded plugin.
unsafe impl Send for Host {}
// SAFETY: Same as `Send`; `Host` has no SDK-owned mutable state.
unsafe impl Sync for Host {}
