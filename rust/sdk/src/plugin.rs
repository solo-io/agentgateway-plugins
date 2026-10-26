//! Plugin traits implemented by Rust plugin authors.

use super::*;

/// Plugin with request and optional response phases.
///
/// A plugin always receives the request phase first. Returning
/// [`HttpRequestAction::Continue`] continues the request. Use
/// [`ResponsePhase::Run`] to also pass plugin-owned state back into
/// [`HttpPlugin::on_response`].
#[async_trait]
pub trait HttpPlugin: Send + Sync + 'static {
	/// State carried from the request phase into the response phase.
	///
	/// Use `()` for request-only plugins.
	type ResponseState: Send + 'static;

	/// Runs during the request phase.
	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>>;

	/// Runs during the response phase when `on_request` returned response state.
	#[allow(unused_variables)]
	async fn on_response(
		&self,
		state: Self::ResponseState,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		Ok(ResponseAction::Continue)
	}
}

/// Factory for plugins that need custom setup from JSON configuration.
pub trait HttpPluginFactory: HttpPlugin + Sized {
	/// Creates a plugin instance from the configured JSON value.
	fn new(config: Value) -> Result<Self>;
}
