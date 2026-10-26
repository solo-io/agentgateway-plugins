//! Runs TypeScript or JavaScript from the plugin config.
//!
//! The code defines `onRequest` and/or `onResponse`. Both may be async, and
//! get a `req`/`res` object exposing the plugin host API. See
//! `agentgateway.d.ts` for the full API.
//!
//! Each phase runs in a fresh QuickJS runtime, so no JavaScript state is shared
//! between requests or between the request and response phases. Use the
//! `{ state }` return value to pass data from `onRequest` to `onResponse`.

mod api;
mod ts;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use agentgateway_sdk::{
	Header, HttpPlugin, HttpPluginFactory, HttpRequestAction, RequestContext, ResponseAction,
	ResponseContext, ResponsePhase, Result, async_trait, export_plugin,
};
use http::{HeaderName, HeaderValue};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Function, Value};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
	/// TypeScript or JavaScript source defining `onRequest` and/or `onResponse`.
	code: String,
	/// Maximum JavaScript execution time per phase, in milliseconds. Time spent
	/// awaiting host calls such as `sleep` counts too.
	#[serde(default = "default_timeout_ms")]
	timeout_ms: u64,
	/// Maximum JavaScript heap size per phase, in bytes.
	#[serde(default = "default_memory_limit")]
	memory_limit_bytes: usize,
}

fn default_timeout_ms() -> u64 {
	1000
}

fn default_memory_limit() -> usize {
	16 * 1024 * 1024
}

struct QuickJs {
	/// The user code with TypeScript types stripped.
	code: String,
	timeout: Duration,
	memory_limit: usize,
	has_on_request: bool,
	has_on_response: bool,
}

impl HttpPluginFactory for QuickJs {
	fn new(config: serde_json::Value) -> Result<Self> {
		let config: Config = serde_json::from_value(config)?;
		let mut plugin = Self {
			code: ts::strip_types(&config.code).map_err(agentgateway_sdk::Error::msg)?,
			timeout: Duration::from_millis(config.timeout_ms),
			memory_limit: config.memory_limit_bytes,
			has_on_request: false,
			has_on_response: false,
		};
		// Evaluate the code once up front, so syntax errors fail plugin creation
		// rather than every request, and so we know which phases to run.
		let (has_on_request, has_on_response) = block_on(plugin.run(async |ctx| {
			Ok((
				handler(&ctx, "onRequest")?.is_some(),
				handler(&ctx, "onResponse")?.is_some(),
			))
		}))?;
		if !has_on_request && !has_on_response {
			return Err(agentgateway_sdk::Error::msg(
				"code must define an onRequest or onResponse function",
			));
		}
		plugin.has_on_request = has_on_request;
		plugin.has_on_response = has_on_response;
		Ok(plugin)
	}
}

/// What a handler asked for, decoded from its return value.
#[derive(Debug, Default, Deserialize)]
struct HandlerResult {
	status: Option<u16>,
	#[serde(default)]
	headers: BTreeMap<String, String>,
	#[serde(default)]
	body: String,
	#[serde(default)]
	state: serde_json::Value,
}

impl HandlerResult {
	fn direct_response(&self) -> Result<Option<(u16, Vec<Header>)>> {
		let Some(status) = self.status else {
			return Ok(None);
		};
		let headers = self
			.headers
			.iter()
			.map(|(name, value)| {
				Ok(Header::new(
					HeaderName::try_from(name.as_str())?,
					HeaderValue::try_from(value.as_str())?,
				))
			})
			.collect::<Result<_>>()?;
		Ok(Some((status, headers)))
	}
}

#[async_trait]
impl HttpPlugin for QuickJs {
	/// JSON passed from `onRequest` (`{ state }`) to `onResponse`.
	type ResponseState = serde_json::Value;

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		let result = if self.has_on_request {
			self
				.run(async |ctx| {
					let req = api::request_object(&ctx, request).map_err(|e| e.to_string())?;
					call_handler(&ctx, "onRequest", (req,)).await
				})
				.await?
		} else {
			HandlerResult::default()
		};
		if let Some((status, headers)) = result.direct_response()? {
			return Ok(HttpRequestAction::DirectResponse {
				status,
				headers,
				body: result.body.into(),
			});
		}
		Ok(HttpRequestAction::Continue {
			response: if self.has_on_response {
				ResponsePhase::Run(result.state)
			} else {
				ResponsePhase::Skip
			},
		})
	}

	async fn on_response(
		&self,
		state: Self::ResponseState,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		let result = self
			.run(async |ctx| {
				let res = api::response_object(&ctx, response).map_err(|e| e.to_string())?;
				let state = api::to_js(&ctx, &state).map_err(|e| e.to_string())?;
				call_handler(&ctx, "onResponse", (res, state)).await
			})
			.await?;
		if let Some((status, headers)) = result.direct_response()? {
			return Ok(ResponseAction::DirectResponse {
				status,
				headers,
				body: result.body.into(),
			});
		}
		Ok(ResponseAction::Continue)
	}
}

impl QuickJs {
	/// Creates a fresh runtime, evaluates the user code, then runs `f`.
	async fn run<T, F>(&self, f: F) -> Result<T>
	where
		F: AsyncFnOnce(Ctx<'_>) -> std::result::Result<T, String> + Send,
		T: Send + 'static,
	{
		let runtime = AsyncRuntime::new()?;
		runtime.set_memory_limit(self.memory_limit).await;
		let deadline = Instant::now() + self.timeout;
		runtime
			.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)))
			.await;
		let context = AsyncContext::full(&runtime).await?;
		let result = context
			.async_with(async |ctx| {
				api::install_globals(&ctx).map_err(|e| e.to_string())?;
				ctx
					.eval::<(), _>(self.code.as_str())
					.catch(&ctx)
					.map_err(|e| format!("failed to evaluate code: {e}"))?;
				f(ctx).await
			})
			.await;
		result.map_err(|e| {
			if Instant::now() > deadline {
				agentgateway_sdk::Error::msg(format!("code exceeded timeout of {:?}: {e}", self.timeout))
			} else {
				agentgateway_sdk::Error::msg(e)
			}
		})
	}
}

/// Looks up a top-level function. Uses `eval` rather than `globalThis` so
/// `const onRequest = async () => {}` works as well as `function onRequest`.
fn handler<'js>(ctx: &Ctx<'js>, name: &str) -> std::result::Result<Option<Function<'js>>, String> {
	let value: Value = ctx
		.eval(format!("typeof {name} === 'function' ? {name} : undefined"))
		.catch(ctx)
		.map_err(|e| e.to_string())?;
	Ok(value.into_function())
}

/// Calls a handler, awaits it if it returned a promise, and decodes the result.
async fn call_handler<'js>(
	ctx: &Ctx<'js>,
	name: &str,
	args: impl rquickjs::function::IntoArgs<'js>,
) -> std::result::Result<HandlerResult, String> {
	let Some(function) = handler(ctx, name)? else {
		return Ok(HandlerResult::default());
	};
	let mut value: Value = function
		.call(args)
		.catch(ctx)
		.map_err(|e| format!("{name} failed: {e}"))?;
	if let Some(promise) = value.as_promise() {
		value = promise
			.clone()
			.into_future::<Value>()
			.await
			.catch(ctx)
			.map_err(|e| format!("{name} failed: {e}"))?;
	}
	let json = api::from_js(ctx, value).map_err(|e| e.to_string())?;
	if json.is_null() {
		return Ok(HandlerResult::default());
	}
	serde_json::from_value(json).map_err(|e| format!("{name} returned an invalid result: {e}"))
}

/// Runs a future to completion outside of a plugin callback. Used once at
/// startup, where nothing awaited by the code can actually pend.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
	let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
	let mut future = std::pin::pin!(future);
	loop {
		if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
			return output;
		}
		std::thread::yield_now();
	}
}

export_plugin!(QuickJs, name = "quickjs", factory);

#[cfg(test)]
mod tests;
