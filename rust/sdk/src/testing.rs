//! Test harness for Rust plugins.
//!
//! Enable the `testing` feature in plugin unit tests to run plugin callbacks
//! against an in-process mock host:
//!
//! ```rust,ignore
//! # use agentgateway_sdk::{HttpPlugin, Result};
//! # async fn test_plugin<P: HttpPlugin>(plugin: P) -> Result<()> {
//! use agentgateway_sdk::testing::{TestEnvironment, TestRequest};
//! use http::{HeaderName, HeaderValue};
//!
//! let mut env = TestEnvironment::default();
//! let action = env
//!     .run_request(&plugin, TestRequest::get("/hello").header(
//!         HeaderName::from_static("x-input"),
//!         HeaderValue::from_static("yes"),
//!     ))
//!     .await?;
//! assert!(matches!(action, agentgateway_sdk::HttpRequestAction::Continue { .. }));
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use super::*;

/// Mock host used to run plugin callbacks in unit tests.
#[derive(Clone)]
pub struct TestEnvironment {
	state: Arc<Mutex<TestState>>,
}

impl Default for TestEnvironment {
	fn default() -> Self {
		Self::new()
	}
}

/// Invalidates a context when a run ends however it ends: completion, the run
/// future being dropped mid-callback, or a panic. A plugin may keep a context
/// past its callback, and the environment's state may be gone by then.
struct InvalidateOnDrop(Arc<ContextHandle>);

impl Drop for InvalidateOnDrop {
	fn drop(&mut self) {
		self.0.invalidate();
	}
}

impl TestEnvironment {
	/// Creates an empty test environment.
	pub fn new() -> Self {
		Self {
			state: Arc::new(Mutex::new(TestState::default())),
		}
	}

	/// Registers a property value returned by [`RequestContext::property`] or
	/// [`ResponseContext::property`]. It reads back as the CEL value of its JSON
	/// form: numbers are `int` if they fit, else `uint`, else `double`.
	pub fn insert_property(
		&mut self,
		path: impl IntoIterator<Item = impl Into<String>>,
		value: impl serde::Serialize,
	) -> Result<()> {
		let path = path.into_iter().map(Into::into).collect::<Vec<_>>();
		let value = serde_json::to_value(value).map_err(Error::msg)?;
		self.state.lock().unwrap().properties.insert(path, value);
		Ok(())
	}

	/// Runs a plugin request callback with the supplied mock request.
	pub async fn run_request<P>(
		&mut self,
		plugin: &P,
		request: TestRequest,
	) -> Result<HttpRequestAction<P::ResponseState>>
	where
		P: HttpPlugin + ?Sized,
	{
		{
			let mut state = self.state.lock().unwrap();
			state.active = ActivePhase::Request;
			state.request = request.into_state();
			state.response = TestResponse::default().into_state();
			state.metadata.clear();
			state.logs.clear();
			state.last_error = None;
		}
		let host = self.host();
		let handle = ContextHandle::new(host, self.ctx());
		let _invalidate = InvalidateOnDrop(handle.clone());
		let request = RequestContext { handle };
		let mut future = Box::pin(plugin.on_request(request));
		poll_with_host(host, |cx| future.as_mut().poll(cx)).await
	}

	/// Runs a plugin response callback with the supplied response-phase state.
	pub async fn run_response<P>(
		&mut self,
		plugin: &P,
		state: P::ResponseState,
		response: TestResponse,
	) -> Result<ResponseAction>
	where
		P: HttpPlugin + ?Sized,
	{
		{
			let mut test_state = self.state.lock().unwrap();
			test_state.active = ActivePhase::Response;
			test_state.response = response.into_state();
			test_state.last_error = None;
		}
		let host = self.host();
		let handle = ContextHandle::new(host, self.ctx());
		let _invalidate = InvalidateOnDrop(handle.clone());
		let response = ResponseContext { handle };
		let mut future = Box::pin(plugin.on_response(state, response));
		poll_with_host(host, |cx| future.as_mut().poll(cx)).await
	}

	/// Returns request headers after the last request callback.
	pub fn request_headers(&self) -> HeaderMap {
		headers_to_map(&self.state.lock().unwrap().request.headers)
	}

	/// Returns response headers after the last response callback.
	pub fn response_headers(&self) -> HeaderMap {
		headers_to_map(&self.state.lock().unwrap().response.headers)
	}

	/// Returns the response status after the last response callback.
	pub fn response_status(&self) -> Option<StatusCode> {
		self
			.state
			.lock()
			.unwrap()
			.response
			.pseudo_headers
			.iter()
			.find(|(name, _)| name == PseudoHeader::Status.as_str())
			.and_then(|(_, value)| StatusCode::from_bytes(value.as_bytes()).ok())
	}

	/// Returns the request body after the last request callback.
	pub fn request_body(&self) -> bytes::Bytes {
		bytes::Bytes::copy_from_slice(&self.state.lock().unwrap().request.body)
	}

	/// Returns the response body after the last response callback.
	pub fn response_body(&self) -> bytes::Bytes {
		bytes::Bytes::copy_from_slice(&self.state.lock().unwrap().response.body)
	}

	/// Returns metadata set by the plugin.
	/// Metadata the plugin set, as JSON.
	pub fn metadata(&self) -> HashMap<String, serde_json::Value> {
		self.state.lock().unwrap().metadata.clone()
	}

	/// Returns log records emitted by the plugin.
	pub fn logs(&self) -> Vec<TestLog> {
		self.state.lock().unwrap().logs.clone()
	}

	// The mock host uses the shared test state as both its vtable data and the
	// request/response context.
	fn ctx(&self) -> *const abi::AgwContext {
		Arc::as_ptr(&self.state).cast::<abi::AgwContext>()
	}

	fn host(&self) -> Host {
		Host {
			vtable: abi::AgwHostVTable {
				data: Arc::as_ptr(&self.state).cast::<c_void>(),
				get_header: Some(test_get_header),
				headers: Some(test_headers),
				body_stream: Some(test_body_stream),
				add_header: Some(test_add_header),
				set_header: Some(test_set_header),
				remove_header: Some(test_remove_header),
				set_body_stream: Some(test_set_body_stream),
				get_property: Some(test_get_property),
				set_metadata: Some(test_set_metadata),
				log: Some(test_log),
				..Default::default()
			},
		}
	}
}

/// Mock request supplied to [`TestEnvironment::run_request`].
#[derive(Debug, Clone)]
pub struct TestRequest {
	method: Method,
	scheme: Scheme,
	authority: Option<Authority>,
	path: PathAndQuery,
	headers: Vec<(HeaderName, HeaderValue)>,
	body: bytes::Bytes,
}

impl Default for TestRequest {
	fn default() -> Self {
		Self::get("/")
	}
}

impl TestRequest {
	/// Creates a GET request with the supplied path and query.
	pub fn get(path: impl AsRef<str>) -> Self {
		Self::new(Method::GET, path)
	}

	/// Creates a request with the supplied method and path/query.
	pub fn new(method: Method, path: impl AsRef<str>) -> Self {
		Self {
			method,
			scheme: Scheme::HTTP,
			authority: Some(Authority::from_static("example.com")),
			path: path
				.as_ref()
				.parse()
				.unwrap_or_else(|_| PathAndQuery::from_static("/")),
			headers: Vec::new(),
			body: bytes::Bytes::new(),
		}
	}

	/// Sets the request scheme.
	pub fn scheme(mut self, scheme: Scheme) -> Self {
		self.scheme = scheme;
		self
	}

	/// Sets the request authority.
	pub fn authority(mut self, authority: Authority) -> Self {
		self.authority = Some(authority);
		self
	}

	/// Appends a request header.
	pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
		self.headers.push((name, value));
		self
	}

	/// Sets the request body.
	pub fn body(mut self, body: impl Into<bytes::Bytes>) -> Self {
		self.body = body.into();
		self
	}

	fn into_state(self) -> MessageState {
		let mut pseudo_headers = Vec::new();
		pseudo_headers.push((
			PseudoHeader::Method.as_str().to_owned(),
			HeaderValue::from_bytes(self.method.as_str().as_bytes()).expect("method is valid"),
		));
		pseudo_headers.push((
			PseudoHeader::Scheme.as_str().to_owned(),
			HeaderValue::from_bytes(self.scheme.as_str().as_bytes()).expect("scheme is valid"),
		));
		if let Some(authority) = self.authority {
			pseudo_headers.push((
				PseudoHeader::Authority.as_str().to_owned(),
				HeaderValue::from_bytes(authority.as_str().as_bytes()).expect("authority is valid"),
			));
		}
		pseudo_headers.push((
			PseudoHeader::Path.as_str().to_owned(),
			HeaderValue::from_bytes(self.path.as_str().as_bytes()).expect("path is valid"),
		));
		MessageState {
			headers: self.headers,
			pseudo_headers,
			body: self.body.to_vec(),
		}
	}
}

/// Mock response supplied to [`TestEnvironment::run_response`].
#[derive(Debug, Clone)]
pub struct TestResponse {
	status: StatusCode,
	headers: Vec<(HeaderName, HeaderValue)>,
	body: bytes::Bytes,
}

impl Default for TestResponse {
	fn default() -> Self {
		Self::new(StatusCode::OK)
	}
}

impl TestResponse {
	/// Creates a response with the supplied status.
	pub fn new(status: StatusCode) -> Self {
		Self {
			status,
			headers: Vec::new(),
			body: bytes::Bytes::new(),
		}
	}

	/// Appends a response header.
	pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
		self.headers.push((name, value));
		self
	}

	/// Sets the response body.
	pub fn body(mut self, body: impl Into<bytes::Bytes>) -> Self {
		self.body = body.into();
		self
	}

	fn into_state(self) -> MessageState {
		let pseudo_headers = vec![(
			PseudoHeader::Status.as_str().to_owned(),
			HeaderValue::from_bytes(self.status.as_str().as_bytes()).expect("status is valid"),
		)];
		MessageState {
			headers: self.headers,
			pseudo_headers,
			body: self.body.to_vec(),
		}
	}
}

/// Log record captured by [`TestEnvironment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestLog {
	/// Log level.
	pub level: LogLevel,
	/// Log message.
	pub message: String,
}

#[derive(Default)]
struct TestState {
	active: ActivePhase,
	request: MessageState,
	response: MessageState,
	properties: HashMap<Vec<String>, serde_json::Value>,
	metadata: HashMap<String, serde_json::Value>,
	logs: Vec<TestLog>,
	last_error: Option<String>,
}

#[derive(Clone, Copy, Default)]
enum ActivePhase {
	#[default]
	Request,
	Response,
}

#[derive(Default)]
struct MessageState {
	headers: Vec<(HeaderName, HeaderValue)>,
	pseudo_headers: Vec<(String, HeaderValue)>,
	body: Vec<u8>,
}

impl TestState {
	fn active_message(&self) -> &MessageState {
		match self.active {
			ActivePhase::Request => &self.request,
			ActivePhase::Response => &self.response,
		}
	}

	fn active_message_mut(&mut self) -> &mut MessageState {
		match self.active {
			ActivePhase::Request => &mut self.request,
			ActivePhase::Response => &mut self.response,
		}
	}
}

async fn poll_with_host<T>(host: Host, mut poll: impl FnMut(&mut Context<'_>) -> Poll<T>) -> T {
	std::future::poll_fn(|cx| context::with_host(host, || poll(cx))).await
}

unsafe extern "C" fn test_get_header(
	ctx: *const abi::AgwContext,
	name: abi::AgwString,
	out: *mut abi::AgwOwnedBytes,
) -> u32 {
	let data = ctx.cast::<c_void>();
	let state = unsafe { state(data) };
	let name = unsafe { name.as_str() };
	let state = state.lock().unwrap();
	let message = state.active_message();
	let value = message
		.pseudo_headers
		.iter()
		.find(|(header, _)| header.eq_ignore_ascii_case(name))
		.map(|(_, value)| value.as_bytes().to_vec())
		.or_else(|| {
			message
				.headers
				.iter()
				.find(|(header, _)| header.as_str().eq_ignore_ascii_case(name))
				.map(|(_, value)| value.as_bytes().to_vec())
		});
	if !out.is_null() {
		unsafe {
			*out = value.map(owned_bytes).unwrap_or_default();
		}
	}
	abi::AGW_STATUS_OK
}

unsafe extern "C" fn test_headers(ctx: *const abi::AgwContext) -> abi::AgwHeaderIterator {
	let data = ctx.cast::<c_void>();
	let headers = unsafe { state(data) }
		.lock()
		.unwrap()
		.active_message()
		.headers
		.iter()
		.filter(|(name, _)| !name.as_str().starts_with(':'))
		.map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
		.collect::<Vec<_>>();
	abi::AgwHeaderIterator {
		state: Box::into_raw(Box::new(HeaderIteratorState { headers, index: 0 })).cast::<c_void>(),
		next: Some(test_next_header),
		drop: Some(drop_box::<HeaderIteratorState>),
	}
}

unsafe extern "C" fn test_next_header(state: *mut c_void, out: *mut abi::AgwHeader) -> u32 {
	let iterator = unsafe { &mut *(state.cast::<HeaderIteratorState>()) };
	let Some((name, value)) = iterator.headers.get(iterator.index) else {
		if !out.is_null() {
			unsafe { *out = abi::AgwHeader::default() };
		}
		return abi::AGW_STATUS_OK;
	};
	iterator.index += 1;
	if !out.is_null() {
		unsafe {
			*out = abi::AgwHeader {
				name: abi::AgwString::from_ref(name),
				value: abi::AgwBytes::from_slice(value),
			};
		}
	}
	abi::AGW_STATUS_OK
}

unsafe extern "C" fn test_add_header(
	ctx: *const abi::AgwContext,
	name: abi::AgwString,
	value: abi::AgwBytes,
) -> u32 {
	let data = ctx.cast::<c_void>();
	let name = unsafe { header_key(name) };
	let value = unsafe { header_value(value) };
	match (name, value) {
		(Ok(name), Ok(value)) => {
			let mut state = unsafe { state(data) }.lock().unwrap();
			match name {
				HeaderKey::Header(name) => state.active_message_mut().headers.push((name, value)),
				HeaderKey::Pseudo(name) => state
					.active_message_mut()
					.pseudo_headers
					.push((name, value)),
			}
			abi::AGW_STATUS_OK
		},
		(Err(error), _) | (_, Err(error)) => {
			unsafe { set_last_error(data, error) };
			abi::AGW_STATUS_ERROR
		},
	}
}

unsafe extern "C" fn test_set_header(
	ctx: *const abi::AgwContext,
	name: abi::AgwString,
	value: abi::AgwBytes,
) -> u32 {
	let data = ctx.cast::<c_void>();
	let name = unsafe { header_key(name) };
	let value = unsafe { header_value(value) };
	match (name, value) {
		(Ok(name), Ok(value)) => {
			let mut state = unsafe { state(data) }.lock().unwrap();
			let message = state.active_message_mut();
			match name {
				HeaderKey::Header(name) => {
					message.headers.retain(|(header, _)| header != name);
					message.headers.push((name, value));
				},
				HeaderKey::Pseudo(name) => {
					message.pseudo_headers.retain(|(header, _)| header != &name);
					message.pseudo_headers.push((name, value));
				},
			}
			abi::AGW_STATUS_OK
		},
		(Err(error), _) | (_, Err(error)) => {
			unsafe { set_last_error(data, error) };
			abi::AGW_STATUS_ERROR
		},
	}
}

unsafe extern "C" fn test_remove_header(ctx: *const abi::AgwContext, name: abi::AgwString) -> u32 {
	let data = ctx.cast::<c_void>();
	let name = unsafe { header_key(name) };
	match name {
		Ok(name) => {
			let mut state = unsafe { state(data) }.lock().unwrap();
			let message = state.active_message_mut();
			match name {
				HeaderKey::Header(name) => message.headers.retain(|(header, _)| header != name),
				HeaderKey::Pseudo(name) => message.pseudo_headers.retain(|(header, _)| header != &name),
			}
			abi::AGW_STATUS_OK
		},
		Err(error) => {
			unsafe { set_last_error(data, error) };
			abi::AGW_STATUS_ERROR
		},
	}
}

unsafe extern "C" fn test_body_stream(ctx: *const abi::AgwContext) -> abi::AgwBodyStream {
	let data = ctx.cast::<c_void>();
	let body = unsafe { state(data) }
		.lock()
		.unwrap()
		.active_message()
		.body
		.clone();
	abi::AgwBodyStream {
		state: Box::into_raw(Box::new(TestBodyStream {
			chunks: VecDeque::from([body]),
		}))
		.cast::<c_void>(),
		poll_next: Some(test_poll_body_stream),
		drop: Some(drop_box::<TestBodyStream>),
	}
}

unsafe extern "C" fn test_poll_body_stream(
	state: *mut c_void,
	_waker: *const abi::AgwRawWaker,
	out: *mut abi::AgwBodyChunk,
) -> u32 {
	let stream = unsafe { &mut *(state.cast::<TestBodyStream>()) };
	if let Some(chunk) = stream.chunks.pop_front() {
		unsafe { write_data_chunk(out, chunk) };
	} else if !out.is_null() {
		let _ = unsafe { abi::write_sized(out, abi::AgwBodyChunk::default()) };
	}
	abi::AGW_POLL_READY
}

unsafe extern "C" fn test_set_body_stream(
	ctx: *const abi::AgwContext,
	mut stream: abi::AgwBodyStream,
) -> u32 {
	let data = ctx.cast::<c_void>();
	match collect_abi_body_stream(&mut stream) {
		Ok(body) => {
			unsafe { state(data) }
				.lock()
				.unwrap()
				.active_message_mut()
				.body = body;
			unsafe { stream.free() };
			abi::AGW_STATUS_OK
		},
		Err(error) => {
			unsafe { set_last_error(data, error.to_string()) };
			unsafe { stream.free() };
			abi::AGW_STATUS_ERROR
		},
	}
}

unsafe extern "C" fn test_get_property(
	ctx: *const abi::AgwContext,
	path: abi::AgwStringSlice,
	out: *mut abi::AgwCelResult,
	err: *mut abi::AgwError,
) -> u32 {
	let data = ctx.cast::<c_void>();
	let path = unsafe { path.as_slice() }
		.iter()
		.map(|segment| unsafe { segment.as_str() }.to_owned())
		.collect::<Vec<_>>();
	if path.is_empty() {
		unsafe { write_error(err, "property path must not be empty") };
		return abi::AGW_STATUS_ERROR;
	}
	let value = unsafe { state(data) }
		.lock()
		.unwrap()
		.properties
		.get(&path)
		.cloned();
	// A missing property leaves the output as the caller initialized it.
	if let Some(value) = value
		&& let Err(mut result) = unsafe { abi::write_sized(out, crate::value::json_result(value)) }
	{
		unsafe { result.free() };
	}
	abi::AGW_STATUS_OK
}

unsafe extern "C" fn test_set_metadata(
	ctx: *const abi::AgwContext,
	key: abi::AgwString,
	value: *const abi::AgwCelValue,
	err: *mut abi::AgwError,
) -> u32 {
	let data = ctx.cast::<c_void>();
	let key = unsafe { key.as_str() };
	if value.is_null() {
		unsafe { write_error(err, "metadata value is null") };
		return abi::AGW_STATUS_ERROR;
	}
	match unsafe { crate::value::borrowed_json(&*value) } {
		Ok(value) => {
			unsafe { state(data) }
				.lock()
				.unwrap()
				.metadata
				.insert(key.to_owned(), value);
			abi::AGW_STATUS_OK
		},
		Err(error) => {
			unsafe { write_error(err, error.to_string()) };
			abi::AGW_STATUS_ERROR
		},
	}
}

unsafe extern "C" fn test_log(data: *const c_void, level: u32, message: abi::AgwString) {
	let level = match level {
		abi::AGW_LOG_TRACE => LogLevel::Trace,
		abi::AGW_LOG_DEBUG => LogLevel::Debug,
		abi::AGW_LOG_INFO => LogLevel::Info,
		abi::AGW_LOG_WARN => LogLevel::Warn,
		abi::AGW_LOG_ERROR => LogLevel::Error,
		_ => LogLevel::Error,
	};
	let message = unsafe { message.as_str() }.to_owned();
	unsafe { state(data) }
		.lock()
		.unwrap()
		.logs
		.push(TestLog { level, message });
}

fn collect_abi_body_stream(stream: &mut abi::AgwBodyStream) -> Result<Vec<u8>> {
	let poll_next = stream
		.poll_next
		.ok_or_else(|| Error::msg("body stream has no poll_next callback"))?;
	let waker = Waker::noop();
	let mut body = Vec::new();
	loop {
		let mut out = abi::AgwBodyChunk::default();
		let status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			poll_next(stream.state, raw_waker, &mut out)
		});
		match status {
			abi::AGW_POLL_READY => match out.kind {
				abi::AGW_BODY_FRAME_DATA => {
					body.extend_from_slice(unsafe { out.bytes.as_slice() });
					unsafe { out.free() };
				},
				abi::AGW_BODY_FRAME_TRAILERS => unsafe { out.free() },
				abi::AGW_BODY_FRAME_END if out.error.is_empty() => {
					unsafe { out.free() };
					return Ok(body);
				},
				abi::AGW_BODY_FRAME_END => {
					let error = error_from_abi(&out.error);
					unsafe { out.free() };
					return Err(error);
				},
				other => {
					unsafe { out.free() };
					return Err(Error::msg(format!("unknown body frame kind {other}")));
				},
			},
			abi::AGW_POLL_PENDING => {
				unsafe { out.free() };
				return Err(Error::msg(
					"testing host cannot collect a pending replacement body",
				));
			},
			other => {
				unsafe { out.free() };
				return Err(Error::msg(format!("unknown body poll status {other}")));
			},
		}
	}
}

unsafe fn state(data: *const c_void) -> &'static Mutex<TestState> {
	unsafe { &*(data.cast::<Mutex<TestState>>()) }
}

unsafe fn set_last_error(data: *const c_void, error: String) {
	unsafe { state(data) }.lock().unwrap().last_error = Some(error);
}

unsafe fn header_key(name: abi::AgwString) -> std::result::Result<HeaderKey, String> {
	let name = unsafe { name.as_str() };
	if name.starts_with(':') {
		Ok(HeaderKey::Pseudo(name.to_owned()))
	} else {
		HeaderName::from_bytes(name.as_bytes())
			.map(HeaderKey::Header)
			.map_err(|error| error.to_string())
	}
}

unsafe fn header_value(value: abi::AgwBytes) -> std::result::Result<HeaderValue, String> {
	HeaderValue::from_bytes(unsafe { value.as_slice() }).map_err(|error| error.to_string())
}

fn headers_to_map(headers: &[(HeaderName, HeaderValue)]) -> HeaderMap {
	let mut map = HeaderMap::new();
	for (name, value) in headers {
		if !name.as_str().starts_with(':') {
			map.append(name, value.clone());
		}
	}
	map
}

fn owned_bytes(bytes: Vec<u8>) -> abi::AgwOwnedBytes {
	let boxed = Box::new(bytes);
	let bytes = abi::AgwBytes::from_slice(boxed.as_slice());
	abi::AgwOwnedBytes {
		bytes,
		private_data: Box::into_raw(boxed).cast::<c_void>(),
		drop: Some(drop_box::<Vec<u8>>),
	}
}

unsafe fn write_data_chunk(out: *mut abi::AgwBodyChunk, bytes: Vec<u8>) {
	if out.is_null() {
		return;
	}
	let boxed = Box::new(bytes);
	let bytes = abi::AgwBytes::from_slice(boxed.as_slice());
	let value = abi::AgwBodyChunk {
		kind: abi::AGW_BODY_FRAME_DATA,
		bytes,
		private_data: Box::into_raw(boxed).cast::<c_void>(),
		drop: Some(drop_box::<Vec<u8>>),
		..Default::default()
	};
	if let Err(mut value) = unsafe { abi::write_sized(out, value) } {
		unsafe { value.free() };
	}
}

unsafe extern "C" fn drop_box<T>(data: *mut c_void) {
	if !data.is_null() {
		unsafe { drop(Box::from_raw(data.cast::<T>())) };
	}
}

struct HeaderIteratorState {
	headers: Vec<(String, Vec<u8>)>,
	index: usize,
}

struct TestBodyStream {
	chunks: VecDeque<Vec<u8>>,
}

enum HeaderKey {
	Header(HeaderName),
	Pseudo(String),
}

#[cfg(test)]
mod tests {
	use http_body_util::{BodyExt, Full};

	use super::*;

	struct HarnessPlugin;

	#[async_trait]
	impl HttpPlugin for HarnessPlugin {
		type ResponseState = String;

		async fn on_request(
			&self,
			request: RequestContext,
		) -> Result<HttpRequestAction<Self::ResponseState>> {
			assert_eq!(request.method(), Some(Method::POST));
			assert_eq!(request.path(), Some(PathAndQuery::from_static("/unit")));
			assert_eq!(
				request.header(&HeaderName::from_static("x-input")),
				Some(HeaderValue::from_static("yes"))
			);
			assert_eq!(request.body().collect(1024).await?, b"hello");
			let group = request.property(&["jwt", "group"])?;
			assert_eq!(group.as_deref().and_then(CelValue::as_str), Some("admin"));
			request.set_metadata("decision", &serde_json::json!({ "allow": true }))?;
			log(LogLevel::Info, "request ran");
			request.set_header(
				HeaderName::from_static("x-output"),
				HeaderValue::from_static("ok"),
			)?;
			request
				.set_body_stream(Full::new(bytes::Bytes::from_static(b"patched")).map_err(Error::msg))?;
			Ok(HttpRequestAction::Continue {
				response: ResponsePhase::Run("request-state".to_owned()),
			})
		}

		async fn on_response(
			&self,
			state: Self::ResponseState,
			response: ResponseContext,
		) -> Result<ResponseAction> {
			assert_eq!(state, "request-state");
			assert_eq!(response.status(), Some(StatusCode::CREATED));
			assert_eq!(response.body().collect(1024).await?, b"upstream");
			response.set_status(202)?;
			response.set_header(
				HeaderName::from_static("x-response-output"),
				HeaderValue::from_static("ok"),
			)?;
			response.set_body(b"response patched")?;
			Ok(ResponseAction::Continue)
		}
	}

	#[test]
	fn test_environment_runs_request_and_response_plugins() {
		let mut env = TestEnvironment::new();
		env
			.insert_property(["jwt", "group"], serde_json::json!("admin"))
			.unwrap();
		let action = block_on(
			env.run_request(
				&HarnessPlugin,
				TestRequest::new(Method::POST, "/unit")
					.header(
						HeaderName::from_static("x-input"),
						HeaderValue::from_static("yes"),
					)
					.body(bytes::Bytes::from_static(b"hello")),
			),
		)
		.unwrap();
		let state = match action {
			HttpRequestAction::Continue {
				response: ResponsePhase::Run(state),
			} => state,
			_ => panic!("expected response phase state"),
		};
		assert_eq!(
			env
				.request_headers()
				.get(HeaderName::from_static("x-output")),
			Some(&HeaderValue::from_static("ok"))
		);
		assert_eq!(env.request_body(), bytes::Bytes::from_static(b"patched"));
		assert_eq!(
			env.metadata().get("decision"),
			Some(&serde_json::json!({ "allow": true }))
		);
		assert_eq!(
			env.logs(),
			vec![TestLog {
				level: LogLevel::Info,
				message: "request ran".to_owned()
			}]
		);

		let action = block_on(env.run_response(
			&HarnessPlugin,
			state,
			TestResponse::new(StatusCode::CREATED).body(bytes::Bytes::from_static(b"upstream")),
		))
		.unwrap();
		assert!(matches!(action, ResponseAction::Continue));
		assert_eq!(env.response_status(), Some(StatusCode::ACCEPTED));
		assert_eq!(
			env
				.response_headers()
				.get(HeaderName::from_static("x-response-output")),
			Some(&HeaderValue::from_static("ok"))
		);
		assert_eq!(
			env.response_body(),
			bytes::Bytes::from_static(b"response patched")
		);
	}

	/// A body that never produces a frame, so the test host fails to collect it.
	struct PendingBody;

	impl http_body::Body for PendingBody {
		type Data = bytes::Bytes;
		type Error = Error;

		fn poll_frame(
			self: Pin<&mut Self>,
			_cx: &mut Context<'_>,
		) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
			Poll::Pending
		}
	}

	struct SetsPendingBody;

	#[async_trait::async_trait]
	impl HttpPlugin for SetsPendingBody {
		type ResponseState = ();

		async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<()>> {
			assert!(request.set_body_stream(PendingBody).is_err());
			Ok(HttpRequestAction::default())
		}
	}

	// Regression: on a failed `set_body_stream` the host and the SDK both freed the stream.
	#[test]
	fn failed_set_body_stream_frees_the_stream_once() {
		let mut env = TestEnvironment::new();
		block_on(env.run_request(&SetsPendingBody, TestRequest::get("/"))).unwrap();
	}

	static RETAINED: Mutex<Option<RequestContext>> = Mutex::new(None);

	/// Keeps its context and never finishes.
	struct RetainsAndHangs;

	#[async_trait::async_trait]
	impl HttpPlugin for RetainsAndHangs {
		type ResponseState = ();

		async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<()>> {
			*RETAINED.lock().unwrap() = Some(request);
			std::future::pending().await
		}
	}

	// Regression: a run dropped mid-callback left the context live, so a retained
	// context could call into the dropped environment's state.
	#[test]
	fn cancelled_run_invalidates_the_context() {
		let mut env = TestEnvironment::new();
		{
			let mut run = Box::pin(env.run_request(&RetainsAndHangs, TestRequest::get("/")));
			let mut cx = Context::from_waker(Waker::noop());
			assert!(run.as_mut().poll(&mut cx).is_pending());
		}
		drop(env);
		let retained = RETAINED.lock().unwrap().take().unwrap();
		assert!(
			retained
				.set_header(
					HeaderName::from_static("x-late"),
					HeaderValue::from_static("1")
				)
				.is_err()
		);
	}

	fn block_on<T>(future: impl Future<Output = T>) -> T {
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		let mut future = Box::pin(future);
		loop {
			match future.as_mut().poll(&mut cx) {
				Poll::Ready(output) => return output,
				Poll::Pending => std::thread::yield_now(),
			}
		}
	}
}
