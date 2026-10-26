//! Request-phase plugin APIs.

use super::*;

/// Request handle for the active plugin callback.
///
/// Hostcalls made through this value are valid only while the plugin's request
/// future is alive.
#[derive(Clone)]
pub struct RequestContext {
	pub(crate) handle: Arc<ContextHandle>,
}

impl std::fmt::Debug for RequestContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("RequestContext").finish_non_exhaustive()
	}
}

impl RequestContext {
	fn pseudo_header(&self, name: PseudoHeader) -> Option<HeaderValue> {
		header_value_from_host(&self.handle, name.as_str())
	}

	/// Returns the HTTP request method.
	pub fn method(&self) -> Option<Method> {
		self
			.pseudo_header(PseudoHeader::Method)
			.map(|value| Method::from_bytes(value.as_bytes()))
			.transpose()
			.ok()
			.flatten()
	}

	/// Returns the URI scheme pseudo-header, if present.
	pub fn scheme(&self) -> Option<Scheme> {
		self
			.pseudo_header(PseudoHeader::Scheme)
			.and_then(|value| value.to_str().ok()?.parse().ok())
	}

	/// Returns the authority pseudo-header, if present.
	pub fn authority(&self) -> Option<Authority> {
		self
			.pseudo_header(PseudoHeader::Authority)
			.and_then(|value| value.to_str().ok()?.parse().ok())
	}

	/// Returns the request path and query.
	pub fn path(&self) -> Option<PathAndQuery> {
		self
			.pseudo_header(PseudoHeader::Path)
			.and_then(|value| value.to_str().ok()?.parse().ok())
	}

	/// Returns the first request header value with this name.
	pub fn header(&self, name: &HeaderName) -> Option<HeaderValue> {
		header_value_from_host(&self.handle, name.as_str())
	}

	/// Returns every request header value with this name, in order.
	pub fn header_all(&self, name: &HeaderName) -> Vec<HeaderValue> {
		self
			.headers()
			.filter(|(n, _)| n == name)
			.map(|(_, value)| value)
			.collect()
	}

	/// Iterates over request headers visible to the plugin.
	///
	/// Header values are copied out of the host as they are yielded.
	pub fn headers(&self) -> RequestHeaders<'_> {
		let iterator = self
			.handle
			.call(
				|vtable| vtable.headers,
				"host did not expose header iteration capability",
				|headers, ctx| Ok(unsafe { headers(ctx) }),
			)
			.unwrap_or_default();
		RequestHeaders {
			iterator,
			_request: self,
		}
	}

	/// Returns the request body as a one-shot HTTP body stream.
	///
	/// A plugin should call this at most once. The stream is consumed by polling,
	/// collecting, or passing it to another HTTP stack.
	pub fn body(&self) -> RequestBody {
		RequestBody::new(self.handle.clone())
	}

	/// Materializes request metadata into an [`http::Request`].
	///
	/// Headers are copied into a standard [`HeaderMap`]; the body remains
	/// streaming and one-shot.
	pub fn to_http_request(&self) -> Result<http::Request<RequestBody>> {
		let method = self.method().unwrap_or(Method::GET);
		let path = self
			.path()
			.unwrap_or_else(|| PathAndQuery::from_static("/"));
		let mut request = http::Request::builder()
			.method(method)
			.uri(path)
			.body(self.body())
			.map_err(Error::msg)?;
		for (name, value) in self.headers() {
			request.headers_mut().append(name, value);
		}
		Ok(request)
	}

	/// Appends a request header value, keeping any existing values for `name`.
	pub fn add_header(&self, name: HeaderName, value: HeaderValue) -> Result<()> {
		self.handle.call(
			|vtable| vtable.add_header,
			"host did not expose request header add capability",
			|add_header, ctx| self.mutate_header(ctx, add_header, name, value),
		)
	}

	/// Replaces all request header values for `name` with `value`.
	pub fn set_header(&self, name: HeaderName, value: HeaderValue) -> Result<()> {
		self.handle.call(
			|vtable| vtable.set_header,
			"host did not expose request header set capability",
			|set_header, ctx| self.mutate_header(ctx, set_header, name, value),
		)
	}

	/// Sets the request method.
	pub fn set_method(&self, method: Method) -> Result<()> {
		self.set_pseudo_header(PseudoHeader::Method, method.as_str())
	}

	/// Sets the request URI scheme.
	pub fn set_scheme(&self, scheme: Scheme) -> Result<()> {
		self.set_pseudo_header(PseudoHeader::Scheme, scheme.as_str())
	}

	/// Sets the request authority (host and optional port).
	pub fn set_authority(&self, authority: Authority) -> Result<()> {
		self.set_pseudo_header(PseudoHeader::Authority, authority.as_str())
	}

	/// Sets the request path and query.
	///
	/// Gateway-level plugins run before the route is selected, so changing the path
	/// there can change which route matches. Route-level plugins only change the
	/// path sent upstream.
	pub fn set_path(&self, path: PathAndQuery) -> Result<()> {
		self.set_pseudo_header(PseudoHeader::Path, path.as_str())
	}

	fn set_pseudo_header(&self, name: PseudoHeader, value: &str) -> Result<()> {
		self.handle.call(
			|vtable| vtable.set_header,
			"host did not expose request header set capability",
			|set_header, ctx| unsafe {
				status_to_result(
					set_header(
						ctx,
						abi::AgwString::from_static(name.as_str()),
						abi::AgwBytes::from_slice(value.as_bytes()),
					),
					"set request pseudo-header",
				)
			},
		)
	}

	/// Removes all request header values for `name`.
	pub fn remove_header(&self, name: HeaderName) -> Result<()> {
		self.handle.call(
			|vtable| vtable.remove_header,
			"host did not expose request header remove capability",
			|remove_header, ctx| unsafe {
				status_to_result(
					remove_header(ctx, abi::AgwString::from_borrowed(name.as_str())),
					"remove request header",
				)
			},
		)
	}

	/// Replaces the request body with a streaming HTTP body.
	///
	/// The body is consumed by the host after the request phase returns.
	pub fn set_body_stream<B>(&self, body: B) -> Result<()>
	where
		B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
		B::Error: Display,
	{
		self.handle.call(
			|vtable| vtable.set_body_stream,
			"host did not expose request body replacement capability",
			|set_body_stream, ctx| unsafe {
				// The host owns the stream even when this fails.
				let status = set_body_stream(ctx, body_to_abi(body));
				status_to_result(status, "set request body stream")
			},
		)
	}

	fn mutate_header(
		&self,
		ctx: *const abi::AgwContext,
		mutate: unsafe extern "C" fn(*const abi::AgwContext, abi::AgwString, abi::AgwBytes) -> u32,
		name: HeaderName,
		value: HeaderValue,
	) -> Result<()> {
		unsafe {
			status_to_result(
				mutate(
					ctx,
					abi::AgwString::from_borrowed(name.as_str()),
					abi::AgwBytes::from_slice(value.as_bytes()),
				),
				"mutate request header",
			)
		}
	}

	/// Sends an HTTP request through the host on behalf of this request.
	///
	/// The callout is traced and attributed as part of this request. The returned
	/// future may outlive the request phase.
	///
	/// ```rust,ignore
	/// let resp = request.http_call(HttpRequest::get(Uri::from_static("https://issuer.example.com/jwks")))?.await?;
	/// ```
	pub fn http_call(&self, call: HttpRequest) -> Result<HttpCall> {
		callout::http_call_with(&self.handle, call)
	}

	/// Reads a value from the same root CEL context agentgateway policy
	/// expressions see (`request`, `jwt`, and so on), plus plugin metadata
	/// under `<name>.*`. Returns `None` if the property is not set.
	///
	/// `path` is a list of property segments:
	///
	/// ```ignore
	/// // Same value as `jwt.sub` in CEL; the &str points into the host's buffer.
	/// let subject = ctx.property(&["jwt", "sub"])?;
	/// let subject = subject.as_deref().and_then(CelValue::as_str);
	/// // Metadata the `example` plugin set under the key `decision`.
	/// let decision = ctx.property(&["example", "decision"])?;
	/// ```
	pub fn property(&self, path: &[&str]) -> Result<Option<CelResult>> {
		property(&self.handle, path)
	}

	/// Adds plugin metadata to the CEL context.
	///
	/// The host stores the value under the configured plugin name, so plugins
	/// can choose nested keys but cannot choose the CEL top-level key.
	pub fn set_metadata<T: MetadataValue + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
		set_metadata(&self.handle, key, value)
	}
}

pub(crate) fn status_to_result(status: u32, operation: &'static str) -> Result<()> {
	match status {
		abi::AGW_STATUS_OK => Ok(()),
		abi::AGW_STATUS_ERROR => Err(Error::msg(format!("host failed to {operation}"))),
		other => Err(Error::msg(format!(
			"host returned unknown status {other} for {operation}"
		))),
	}
}

/// Streaming request body.
///
/// The body is one-shot. Polling or collecting it consumes the host body stream.
pub struct RequestBody {
	inner: BodyStream,
	handle: Arc<ContextHandle>,
}

impl RequestBody {
	fn new(handle: Arc<ContextHandle>) -> Self {
		Self {
			inner: BodyStream::new(&handle, "request"),
			handle,
		}
	}

	/// Buffers the remaining request body up to `limit` bytes.
	///
	/// Reading consumes the host body, so the buffered bytes are handed back to
	/// the host and still sent on unless the plugin replaces the body. Bodies read
	/// with [`Self::next_chunk`] are not restored.
	pub async fn collect(self, limit: usize) -> Result<Vec<u8>> {
		let (body, trailers) = self.inner.collect_with_trailers(limit).await?;
		body::restore_body(&self.handle, &body, trailers)?;
		Ok(body)
	}

	/// Reads the next data chunk from the request body.
	pub fn next_chunk(&mut self) -> BodyStreamNext<'_> {
		self.inner.next_chunk()
	}
}

impl http_body::Body for RequestBody {
	type Data = bytes::Bytes;
	type Error = Error;

	fn poll_frame(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
		Pin::new(&mut self.inner).poll_frame(cx)
	}
}

/// Iterator over request headers.
pub struct RequestHeaders<'a> {
	iterator: abi::AgwHeaderIterator,
	_request: &'a RequestContext,
}

impl Iterator for RequestHeaders<'_> {
	type Item = (HeaderName, HeaderValue);

	fn next(&mut self) -> Option<Self::Item> {
		let next = self.iterator.next?;
		let mut header = abi::AgwHeader::default();
		let status = unsafe { next(self.iterator.state, &mut header) };
		match status {
			abi::AGW_STATUS_OK => {
				if header.name.ptr.is_null() && header.name.len == 0 {
					return None;
				}
				let name = unsafe { header.name.as_bytes() };
				let value = unsafe { header.value.as_slice() };
				let name = match HeaderName::from_bytes(name) {
					Ok(name) => name,
					Err(_) => {
						debug_assert!(false, "host returned invalid request header name");
						return None;
					},
				};
				let value = match HeaderValue::from_bytes(value) {
					Ok(value) => value,
					Err(_) => {
						debug_assert!(false, "host returned invalid request header value");
						return None;
					},
				};
				Some((name, value))
			},
			other => {
				debug_assert!(
					false,
					"host returned unexpected status {other} iterating request headers"
				);
				None
			},
		}
	}
}

impl Drop for RequestHeaders<'_> {
	fn drop(&mut self) {
		unsafe { self.iterator.free() };
	}
}

/// Result of an HTTP plugin request callback.
pub enum HttpRequestAction<S> {
	/// Continue processing the request.
	Continue {
		/// Whether to install response-phase state for this request.
		response: ResponsePhase<S>,
	},
	/// Stop request processing and return this response immediately.
	DirectResponse {
		/// HTTP status code to return.
		status: u16,
		/// Headers to return.
		headers: Vec<Header>,
		/// Response body to return.
		body: DirectBody,
	},
}

impl<S> Default for HttpRequestAction<S> {
	fn default() -> Self {
		Self::Continue {
			response: ResponsePhase::Skip,
		}
	}
}
