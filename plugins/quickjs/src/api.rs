//! The JavaScript API: `req`/`res` objects wrapping the SDK request and
//! response contexts, plus the `sleep`, `httpCall`, `log`, and `console`
//! globals. Types are declared for users in `agentgateway.d.ts`.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use agentgateway_sdk::{
	CelExpression, HttpRequest, LogLevel, RequestContext, ResponseContext, http_call, log, sleep,
};
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Uri};
use http_body_util::Full;
use rquickjs::convert::Coerced;
use rquickjs::function::{Async, Opt, Rest};
use rquickjs::{Ctx, Exception, Function, Object, Value};
use serde::{Deserialize, Serialize};

/// Body size limit for `text()` and `httpCall` when the caller doesn't pass one.
const DEFAULT_BODY_LIMIT: usize = 1024 * 1024;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The operations shared by requests and responses.
pub trait Message: Clone + Send + Sync + 'static {
	fn header(&self, name: &HeaderName) -> Option<HeaderValue>;
	fn headers(&self) -> Vec<(HeaderName, HeaderValue)>;
	fn add_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()>;
	fn set_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()>;
	fn remove_header(&self, name: HeaderName) -> agentgateway_sdk::Result<()>;
	fn read_body(&self, limit: usize) -> BoxFuture<agentgateway_sdk::Result<Vec<u8>>>;
	fn set_body(&self, body: Bytes) -> agentgateway_sdk::Result<()>;
	fn property(&self, path: &[&str]) -> agentgateway_sdk::Result<Option<serde_json::Value>>;
	fn set_metadata(&self, key: &str, value: &serde_json::Value) -> agentgateway_sdk::Result<()>;
	fn cel(&self, expression: &str) -> agentgateway_sdk::Result<serde_json::Value>;
}

impl Message for RequestContext {
	fn header(&self, name: &HeaderName) -> Option<HeaderValue> {
		RequestContext::header(self, name)
	}
	fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
		RequestContext::headers(self).collect()
	}
	fn add_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()> {
		RequestContext::add_header(self, name, value)
	}
	fn set_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()> {
		RequestContext::set_header(self, name, value)
	}
	fn remove_header(&self, name: HeaderName) -> agentgateway_sdk::Result<()> {
		RequestContext::remove_header(self, name)
	}
	fn read_body(&self, limit: usize) -> BoxFuture<agentgateway_sdk::Result<Vec<u8>>> {
		Box::pin(self.body().collect(limit))
	}
	fn set_body(&self, body: Bytes) -> agentgateway_sdk::Result<()> {
		self.set_body_stream(Full::new(body))
	}
	fn property(&self, path: &[&str]) -> agentgateway_sdk::Result<Option<serde_json::Value>> {
		RequestContext::property(self, path)?
			.map(|v| v.to_json())
			.transpose()
	}
	fn set_metadata(&self, key: &str, value: &serde_json::Value) -> agentgateway_sdk::Result<()> {
		RequestContext::set_metadata(self, key, value)
	}
	fn cel(&self, expression: &str) -> agentgateway_sdk::Result<serde_json::Value> {
		CelExpression::owned(expression).evaluate(self)?.to_json()
	}
}

impl Message for ResponseContext {
	fn header(&self, name: &HeaderName) -> Option<HeaderValue> {
		ResponseContext::header(self, name)
	}
	fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
		ResponseContext::headers(self).collect()
	}
	fn add_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()> {
		ResponseContext::add_header(self, name, value)
	}
	fn set_header(&self, name: HeaderName, value: HeaderValue) -> agentgateway_sdk::Result<()> {
		ResponseContext::set_header(self, name, value)
	}
	fn remove_header(&self, name: HeaderName) -> agentgateway_sdk::Result<()> {
		ResponseContext::remove_header(self, name)
	}
	fn read_body(&self, limit: usize) -> BoxFuture<agentgateway_sdk::Result<Vec<u8>>> {
		Box::pin(self.body().collect(limit))
	}
	fn set_body(&self, body: Bytes) -> agentgateway_sdk::Result<()> {
		ResponseContext::set_body(self, body)
	}
	fn property(&self, path: &[&str]) -> agentgateway_sdk::Result<Option<serde_json::Value>> {
		ResponseContext::property(self, path)?
			.map(|v| v.to_json())
			.transpose()
	}
	fn set_metadata(&self, key: &str, value: &serde_json::Value) -> agentgateway_sdk::Result<()> {
		ResponseContext::set_metadata(self, key, value)
	}
	fn cel(&self, expression: &str) -> agentgateway_sdk::Result<serde_json::Value> {
		CelExpression::owned(expression)
			.evaluate_response(self)?
			.to_json()
	}
}

/// Builds the `req` object passed to `onRequest`.
pub fn request_object<'js>(
	ctx: &Ctx<'js>,
	request: RequestContext,
) -> rquickjs::Result<Object<'js>> {
	let obj = message_object(ctx, request.clone())?;
	obj.set("method", request.method().map(|m| m.to_string()))?;
	obj.set("scheme", request.scheme().map(|s| s.to_string()))?;
	obj.set("authority", request.authority().map(|a| a.to_string()))?;
	obj.set("path", request.path().map(|p| p.to_string()))?;
	Ok(obj)
}

/// Builds the `res` object passed to `onResponse`.
pub fn response_object<'js>(
	ctx: &Ctx<'js>,
	response: ResponseContext,
) -> rquickjs::Result<Object<'js>> {
	let obj = message_object(ctx, response.clone())?;
	obj.set("status", response.status().map(|s| s.as_u16()))?;
	obj.set(
		"setStatus",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, status: u16| {
			response.set_status(status).map_err(|e| throw(&ctx, e))
		})?,
	)?;
	Ok(obj)
}

fn message_object<'js, M: Message>(ctx: &Ctx<'js>, msg: M) -> rquickjs::Result<Object<'js>> {
	let obj = Object::new(ctx.clone())?;

	let m = msg.clone();
	obj.set(
		"header",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, name: String| {
			Ok::<_, rquickjs::Error>(m.header(&header_name(&ctx, name)?).map(header_string))
		})?,
	)?;

	let m = msg.clone();
	obj.set(
		"headers",
		Function::new(ctx.clone(), move || {
			m.headers()
				.into_iter()
				.map(|(name, value)| vec![name.to_string(), header_string(value)])
				.collect::<Vec<_>>()
		})?,
	)?;

	for (method, op) in [
		(
			"setHeader",
			Message::set_header as fn(&M, HeaderName, HeaderValue) -> _,
		),
		("addHeader", Message::add_header),
	] {
		let m = msg.clone();
		obj.set(
			method,
			Function::new(
				ctx.clone(),
				move |ctx: Ctx<'js>, name: String, value: String| {
					let name = header_name(&ctx, name)?;
					let value = HeaderValue::try_from(value).map_err(|e| throw(&ctx, e))?;
					op(&m, name, value).map_err(|e| throw(&ctx, e))
				},
			)?,
		)?;
	}

	let m = msg.clone();
	obj.set(
		"removeHeader",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, name: String| {
			m.remove_header(header_name(&ctx, name)?)
				.map_err(|e| throw(&ctx, e))
		})?,
	)?;

	let m = msg.clone();
	obj.set(
		"text",
		Function::new(
			ctx.clone(),
			Async(move |ctx: Ctx<'js>, Opt(limit): Opt<usize>| {
				let body = m.read_body(limit.unwrap_or(DEFAULT_BODY_LIMIT));
				async move {
					let body = body.await.map_err(|e| throw(&ctx, e))?;
					Ok::<_, rquickjs::Error>(String::from_utf8_lossy(&body).into_owned())
				}
			}),
		)?,
	)?;

	let m = msg.clone();
	obj.set(
		"setBody",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, body: String| {
			m.set_body(Bytes::from(body)).map_err(|e| throw(&ctx, e))
		})?,
	)?;

	let m = msg.clone();
	obj.set(
		"property",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, path: Vec<String>| {
			let path = path.iter().map(String::as_str).collect::<Vec<_>>();
			let value = m.property(&path).map_err(|e| throw(&ctx, e))?;
			to_js(&ctx, &value.unwrap_or(serde_json::Value::Null))
		})?,
	)?;

	let m = msg.clone();
	obj.set(
		"setMetadata",
		Function::new(
			ctx.clone(),
			move |ctx: Ctx<'js>, key: String, value: Value<'js>| {
				let value = from_js(&ctx, value)?;
				m.set_metadata(&key, &value).map_err(|e| throw(&ctx, e))
			},
		)?,
	)?;

	obj.set(
		"cel",
		Function::new(ctx.clone(), move |ctx: Ctx<'js>, expression: String| {
			let value = msg.cel(&expression).map_err(|e| throw(&ctx, e))?;
			to_js(&ctx, &value)
		})?,
	)?;

	Ok(obj)
}

#[derive(Deserialize, Default)]
struct HttpCallInit {
	method: Option<String>,
	#[serde(default)]
	headers: BTreeMap<String, String>,
	body: Option<String>,
	#[serde(rename = "bodyLimit")]
	body_limit: Option<usize>,
}

#[derive(Serialize)]
struct HttpCallResponse {
	status: u16,
	headers: BTreeMap<String, String>,
	body: String,
}

/// Installs the global functions available to user code.
pub fn install_globals<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<()> {
	let globals = ctx.globals();

	globals.set(
		"sleep",
		Function::new(
			ctx.clone(),
			Async(|ctx: Ctx<'js>, ms: u64| async move {
				sleep(Duration::from_millis(ms))
					.await
					.map_err(|e| throw(&ctx, e))
			}),
		)?,
	)?;

	globals.set(
		"httpCall",
		Function::new(
			ctx.clone(),
			Async(|ctx: Ctx<'js>, url: String, Opt(init): Opt<Value<'js>>| {
				let call = init
					.map(|init| from_js(&ctx, init))
					.transpose()
					.and_then(|init| build_http_call(&ctx, &url, init.unwrap_or_default()));
				async move {
					let (request, limit) = call?;
					let response = http_call(request)
						.map_err(|e| throw(&ctx, e))?
						.await
						.map_err(|e| throw(&ctx, e))?;
					let body = response
						.body
						.collect(limit)
						.await
						.map_err(|e| throw(&ctx, e))?;
					let response = HttpCallResponse {
						status: response.status,
						headers: response
							.headers
							.iter()
							.map(|(name, value)| (name.to_string(), header_string(value.clone())))
							.collect(),
						body: String::from_utf8_lossy(&body).into_owned(),
					};
					to_js(
						&ctx,
						&serde_json::to_value(response).map_err(|e| throw(&ctx, e))?,
					)
				}
			}),
		)?,
	)?;

	globals.set(
		"log",
		Function::new(
			ctx.clone(),
			|ctx: Ctx<'js>, level: String, message: String| {
				let level = match level.as_str() {
					"trace" => LogLevel::Trace,
					"debug" => LogLevel::Debug,
					"info" => LogLevel::Info,
					"warn" => LogLevel::Warn,
					"error" => LogLevel::Error,
					other => return Err(throw(&ctx, format!("unknown log level {other:?}"))),
				};
				log(level, message);
				Ok(())
			},
		)?,
	)?;

	let console = Object::new(ctx.clone())?;
	for (method, level) in [
		("debug", LogLevel::Debug),
		("log", LogLevel::Info),
		("info", LogLevel::Info),
		("warn", LogLevel::Warn),
		("error", LogLevel::Error),
	] {
		let level = level as u32;
		console.set(
			method,
			Function::new(ctx.clone(), move |Rest(args): Rest<Coerced<String>>| {
				let message = args.into_iter().map(|a| a.0).collect::<Vec<_>>().join(" ");
				log(log_level(level), message);
			})?,
		)?;
	}
	globals.set("console", console)?;

	Ok(())
}

fn log_level(level: u32) -> LogLevel {
	match level {
		l if l == LogLevel::Debug as u32 => LogLevel::Debug,
		l if l == LogLevel::Warn as u32 => LogLevel::Warn,
		l if l == LogLevel::Error as u32 => LogLevel::Error,
		_ => LogLevel::Info,
	}
}

fn build_http_call(
	ctx: &Ctx<'_>,
	url: &str,
	init: serde_json::Value,
) -> rquickjs::Result<(HttpRequest, usize)> {
	let init: HttpCallInit = serde_json::from_value(init).map_err(|e| throw(ctx, e))?;
	let uri: Uri = url.parse().map_err(|e| throw(ctx, e))?;
	let method = match init.method {
		Some(method) => Method::try_from(method.as_str()).map_err(|e| throw(ctx, e))?,
		None => Method::GET,
	};
	let mut request = HttpRequest::new(method, uri);
	for (name, value) in init.headers {
		let name = header_name(ctx, name)?;
		let value = HeaderValue::try_from(value).map_err(|e| throw(ctx, e))?;
		request.headers.append(name, value);
	}
	if let Some(body) = init.body {
		request.body = body.into();
	}
	Ok((request, init.body_limit.unwrap_or(DEFAULT_BODY_LIMIT)))
}

/// Converts a Rust error into a thrown JavaScript exception.
pub fn throw(ctx: &Ctx<'_>, error: impl Display) -> rquickjs::Error {
	Exception::throw_message(ctx, &error.to_string())
}

fn header_name(ctx: &Ctx<'_>, name: String) -> rquickjs::Result<HeaderName> {
	HeaderName::try_from(name).map_err(|e| throw(ctx, e))
}

fn header_string(value: HeaderValue) -> String {
	String::from_utf8_lossy(value.as_bytes()).into_owned()
}

/// Converts JSON into a JavaScript value.
pub fn to_js<'js>(ctx: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Result<Value<'js>> {
	ctx.json_parse(value.to_string())
}

/// Converts a JavaScript value into JSON. `undefined` and functions become
/// `null`.
pub fn from_js<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<serde_json::Value> {
	match ctx.json_stringify(value)? {
		Some(json) => serde_json::from_str(&json.to_string()?).map_err(|e| throw(ctx, e)),
		None => Ok(serde_json::Value::Null),
	}
}
