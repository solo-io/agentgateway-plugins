//! Response-phase plugin APIs.

use super::*;

/// Response handle for the active response-phase callback.
#[derive(Clone)]
pub struct ResponseContext {
	pub(crate) handle: Arc<ContextHandle>,
}

impl std::fmt::Debug for ResponseContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ResponseContext").finish_non_exhaustive()
	}
}

impl ResponseContext {
	fn pseudo_header(&self, name: PseudoHeader) -> Option<HeaderValue> {
		header_value_from_host(&self.handle, name.as_str())
	}

	/// Returns the HTTP response status.
	pub fn status(&self) -> Option<StatusCode> {
		self
			.pseudo_header(PseudoHeader::Status)
			.and_then(|value| StatusCode::from_bytes(value.as_bytes()).ok())
	}

	/// Returns the first response header value with this name.
	pub fn header(&self, name: &HeaderName) -> Option<HeaderValue> {
		header_value_from_host(&self.handle, name.as_str())
	}

	/// Returns every response header value with this name, in order.
	pub fn header_all(&self, name: &HeaderName) -> Vec<HeaderValue> {
		self
			.headers()
			.filter(|(n, _)| n == name)
			.map(|(_, value)| value)
			.collect()
	}

	/// Iterates over response headers visible to the plugin.
	///
	/// Header values are copied out of the host as they are yielded.
	pub fn headers(&self) -> ResponseHeaders<'_> {
		let iterator = self
			.handle
			.call(
				|vtable| vtable.headers,
				"host did not expose header iteration capability",
				|headers, ctx| Ok(unsafe { headers(ctx) }),
			)
			.unwrap_or_default();
		ResponseHeaders {
			iterator,
			_response: self,
		}
	}

	/// Returns the response body as a one-shot HTTP body stream.
	///
	/// A plugin should call this at most once. The stream is consumed by polling,
	/// collecting, or passing it to another HTTP stack.
	pub fn body(&self) -> ResponseBody {
		ResponseBody::new(self.handle.clone())
	}

	/// Appends a response header value, keeping any existing values for `name`.
	pub fn add_header(&self, name: HeaderName, value: HeaderValue) -> Result<()> {
		self.handle.call(
			|vtable| vtable.add_header,
			"host did not expose response header add capability",
			|add_header, ctx| mutate_response_header(ctx, add_header, name, value),
		)
	}

	/// Replaces all response header values for `name` with `value`.
	pub fn set_header(&self, name: HeaderName, value: HeaderValue) -> Result<()> {
		self.handle.call(
			|vtable| vtable.set_header,
			"host did not expose response header set capability",
			|set_header, ctx| mutate_response_header(ctx, set_header, name, value),
		)
	}

	/// Removes all response header values for `name`.
	pub fn remove_header(&self, name: HeaderName) -> Result<()> {
		self.handle.call(
			|vtable| vtable.remove_header,
			"host did not expose response header remove capability",
			|remove_header, ctx| unsafe {
				status_to_result(
					remove_header(ctx, abi::AgwString::from_borrowed(name.as_str())),
					"remove response header",
				)
			},
		)
	}

	/// Sets the response status code.
	pub fn set_status(&self, status: u16) -> Result<()> {
		let status = status.to_string();
		self.handle.call(
			|vtable| vtable.set_header,
			"host did not expose response status set capability",
			|set_header, ctx| unsafe {
				status_to_result(
					set_header(
						ctx,
						abi::AgwString::from_static(PseudoHeader::Status.as_str()),
						abi::AgwBytes::from_slice(status.as_bytes()),
					),
					"set response status",
				)
			},
		)
	}

	/// Replaces the response body with one buffered chunk.
	pub fn set_body(&self, body: impl AsRef<[u8]>) -> Result<()> {
		self
			.set_body_stream(Full::new(bytes::Bytes::copy_from_slice(body.as_ref())).map_err(Error::msg))
	}

	/// Replaces the response body with a streaming HTTP body.
	///
	/// The body is consumed by the host after the response phase returns.
	pub fn set_body_stream<B>(&self, body: B) -> Result<()>
	where
		B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
		B::Error: Display,
	{
		self.handle.call(
			|vtable| vtable.set_body_stream,
			"host did not expose response body replacement capability",
			|set_body_stream, ctx| unsafe {
				// The host owns the stream even when this fails.
				let status = set_body_stream(ctx, body_to_abi(body));
				status_to_result(status, "set response body stream")
			},
		)
	}

	/// Sends an HTTP request through the host on behalf of this response.
	///
	/// The callout is traced and attributed as part of this response. The returned
	/// future may outlive the response phase.
	///
	/// ```rust,ignore
	/// let resp = response.http_call(HttpRequest::get(Uri::from_static("https://issuer.example.com/jwks")))?.await?;
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

fn mutate_response_header(
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
			"mutate response header",
		)
	}
}

/// Streaming response body.
///
/// The body is one-shot. Polling or collecting it consumes the host body stream.
pub struct ResponseBody {
	inner: BodyStream,
	handle: Arc<ContextHandle>,
}

impl ResponseBody {
	fn new(handle: Arc<ContextHandle>) -> Self {
		Self {
			inner: BodyStream::new(&handle, "response"),
			handle,
		}
	}

	/// Buffers the remaining response body up to `limit` bytes.
	///
	/// Reading consumes the host body, so the buffered bytes are handed back to
	/// the host and still sent on unless the plugin replaces the body. Bodies read
	/// with [`Self::next_chunk`] are not restored.
	pub async fn collect(self, limit: usize) -> Result<Vec<u8>> {
		let (body, trailers) = self.inner.collect_with_trailers(limit).await?;
		body::restore_body(&self.handle, &body, trailers)?;
		Ok(body)
	}

	/// Reads the next data chunk from the response body.
	pub fn next_chunk(&mut self) -> BodyStreamNext<'_> {
		self.inner.next_chunk()
	}
}

impl http_body::Body for ResponseBody {
	type Data = bytes::Bytes;
	type Error = Error;

	fn poll_frame(
		mut self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
		Pin::new(&mut self.inner).poll_frame(cx)
	}
}

/// Iterator over response headers.
pub struct ResponseHeaders<'a> {
	iterator: abi::AgwHeaderIterator,
	_response: &'a ResponseContext,
}

impl Iterator for ResponseHeaders<'_> {
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
						debug_assert!(false, "host returned invalid response header name");
						return None;
					},
				};
				let value = match HeaderValue::from_bytes(value) {
					Ok(value) => value,
					Err(_) => {
						debug_assert!(false, "host returned invalid response header value");
						return None;
					},
				};
				Some((name, value))
			},
			other => {
				debug_assert!(
					false,
					"host returned unexpected status {other} iterating response headers"
				);
				None
			},
		}
	}
}

impl Drop for ResponseHeaders<'_> {
	fn drop(&mut self) {
		unsafe { self.iterator.free() };
	}
}

/// Result of a response-phase plugin callback.
#[derive(Default)]
pub enum ResponseAction {
	/// Continue processing the upstream response.
	#[default]
	Continue,
	/// Replace the upstream response with a direct response from the plugin.
	DirectResponse {
		/// HTTP status code to return.
		status: u16,
		/// Headers to return.
		headers: Vec<Header>,
		/// Response body to return.
		body: DirectBody,
	},
}

/// Whether a request callback should install response-phase state.
#[derive(Debug, Clone, Default)]
pub enum ResponsePhase<S> {
	/// Do not run the plugin during the response phase.
	#[default]
	Skip,
	/// Run the plugin during the response phase with this state.
	Run(S),
}
