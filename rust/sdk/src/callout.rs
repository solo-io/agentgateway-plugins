//! Host HTTP callouts from plugins.

use super::*;

/// Sends an HTTP request through the host on behalf of the request or response
/// that owns `handle`. Use [`RequestContext::http_call`] or
/// [`ResponseContext::http_call`].
pub(crate) fn http_call_with(handle: &ContextHandle, call: HttpRequest) -> Result<HttpCall> {
	// Build the request, and with it the body stream, only once the call is
	// allowed: the host takes ownership of the stream, and nothing else frees it.
	let (future, ffi_call) = handle.call(
		|vtable| vtable.http_call,
		"host did not expose the HTTP call capability",
		|http_call, ctx| {
			let ffi_call = FfiHostHttpRequest::new(call);
			let future = unsafe { http_call(ctx, ffi_call.as_ffi()) };
			Ok((future, ffi_call))
		},
	)?;
	Ok(HttpCall {
		future,
		_request: ffi_call,
	})
}

/// Sends an HTTP request through the host outside of any request, e.g. from a
/// background thread flushing buffered data.
///
/// The call isn't attributed to a request; prefer [`RequestContext::http_call`]
/// when there is one. The returned future can be awaited from any thread or
/// executor.
pub fn http_call(call: HttpRequest) -> Result<HttpCall> {
	let host = context::host()?;
	let Some(http_call) = host.vtable.http_call else {
		return Err(Error::msg("host did not expose the HTTP call capability"));
	};
	let ffi_call = FfiHostHttpRequest::new(call);
	let future = unsafe { http_call(std::ptr::null(), ffi_call.as_ffi()) };
	Ok(HttpCall {
		future,
		_request: ffi_call,
	})
}

/// Host HTTP callout request.
///
/// `uri` must be an absolute `http` or `https` URL. HTTPS is verified against
/// the system trust roots.
#[non_exhaustive]
pub struct HttpRequest {
	/// HTTP method to send.
	pub method: Method,
	/// Absolute request URL, e.g. `https://issuer.example.com/jwks`.
	pub uri: Uri,
	/// Headers sent with the request.
	pub headers: HeaderMap,
	/// Request body.
	pub body: DirectBody,
}

impl HttpRequest {
	/// Creates a callout request with no headers and an empty body.
	pub fn new(method: Method, uri: Uri) -> Self {
		Self {
			method,
			uri,
			headers: HeaderMap::new(),
			body: Vec::new().into(),
		}
	}

	/// Creates a `GET` callout request.
	pub fn get(uri: Uri) -> Self {
		Self::new(Method::GET, uri)
	}
}

/// Streaming response from a host HTTP callout.
pub struct HttpResponse {
	/// HTTP status code returned by the backend.
	pub status: u16,
	/// Response headers returned by the backend.
	pub headers: HeaderMap,
	/// Streaming response body.
	pub body: BodyStream,
}

/// Future returned by [`http_call`].
pub struct HttpCall {
	future: abi::AgwHttpFuture,
	_request: FfiHostHttpRequest,
}

// SAFETY: `HttpCall` owns a host HTTP future plus request buffers that keep
// borrowed ABI views alive. The opaque future is only accessed through host
// poll/drop callbacks.
unsafe impl Send for HttpCall {}

impl Future for HttpCall {
	type Output = Result<HttpResponse>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let Some(poll) = self.future.poll else {
			return Poll::Ready(Err(Error::msg("host HTTP future has no poll function")));
		};
		let mut out = abi::AgwHttpResponse::default();
		match abi::with_borrowed_waker(cx.waker(), |waker| unsafe {
			poll(self.future.state, waker, &mut out)
		}) {
			abi::AGW_POLL_PENDING => {
				// Outputs are freed whatever the status; a PENDING poll may have written one.
				unsafe { out.free() };
				Poll::Pending
			},
			abi::AGW_POLL_READY => {
				self.drop_inner();
				let response = unsafe { response_from_ffi(&mut out) };
				unsafe { out.free() };
				Poll::Ready(response)
			},
			status => {
				unsafe { out.free() };
				self.drop_inner();
				Poll::Ready(Err(Error::msg(format!(
					"host HTTP future returned unknown poll status {status}"
				))))
			},
		}
	}
}

impl HttpCall {
	fn drop_inner(&mut self) {
		// Later polls must not reach the host with a freed state.
		self.future.poll = None;
		if let Some(drop) = self.future.drop.take()
			&& !self.future.state.is_null()
		{
			unsafe { drop(self.future.state) };
		}
		self.future.state = std::ptr::null_mut();
	}
}

impl Drop for HttpCall {
	fn drop(&mut self) {
		self.drop_inner();
	}
}
