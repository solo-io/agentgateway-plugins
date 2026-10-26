//! Shows what happens when a plugin fails.
//!
//! | Path        | Does                                                      |
//! | ----------- | --------------------------------------------------------- |
//! | `/panic`    | Panics. The SDK catches it, so only this request fails.   |
//! | `/segfault` | Writes through a null pointer. The whole gateway crashes. |
//! | anything    | Continues upstream.                                       |
//!
//! Plugins run inside the gateway process. The SDK catches Rust panics at the
//! ABI boundary and turns them into a request error (a safeguard: plugins
//! should still return `Err` rather than panic), but nothing can catch a
//! segfault, abort, or other fatal signal: it kills every connection on the
//! gateway, not just the request that triggered it.

use agentgateway_sdk::{
	HttpPlugin, HttpPluginFactory, HttpRequestAction, RequestContext, ResponsePhase, Result,
	async_trait, export_plugin,
};

struct Crash;

impl HttpPluginFactory for Crash {
	fn new(_config: serde_json::Value) -> Result<Self> {
		Ok(Self)
	}
}

#[async_trait]
impl HttpPlugin for Crash {
	type ResponseState = ();

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		match request.path().as_ref().map(|p| p.path()) {
			Some("/panic") => panic!("crash plugin: /panic requested"),
			Some("/segfault") => segfault(),
			_ => Ok(HttpRequestAction::Continue {
				response: ResponsePhase::Skip,
			}),
		}
	}
}

/// Deliberately crashes the process with SIGSEGV.
fn segfault() -> ! {
	// SAFETY: it isn't. This is undefined behavior on purpose; the volatile write
	// keeps the compiler from optimizing it away.
	unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
	unreachable!("wrote through a null pointer");
}

export_plugin!(Crash, name = "crash", factory);

#[cfg(test)]
mod tests;
