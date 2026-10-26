//! HTTP body types used by plugin actions and host streams.

use super::*;

/// Boxed HTTP body accepted by SDK APIs.
pub type BoxBody = Pin<Box<dyn http_body::Body<Data = bytes::Bytes, Error = Error> + Send>>;

/// Body used for direct responses and host HTTP callouts.
pub enum DirectBody {
	/// A buffered body emitted as a single data chunk.
	Bytes(bytes::Bytes),
	/// A streaming body emitted frame by frame.
	Body(BoxBody),
}

impl DirectBody {
	/// Wraps any compatible HTTP body for a direct response or callout.
	pub fn body<B>(body: B) -> Self
	where
		B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
		B::Error: Display,
	{
		Self::Body(Box::pin(
			body.map_err(|error| Error::msg(error.to_string())),
		))
	}
}

impl From<Vec<u8>> for DirectBody {
	fn from(value: Vec<u8>) -> Self {
		Self::Bytes(value.into())
	}
}

impl From<bytes::Bytes> for DirectBody {
	fn from(value: bytes::Bytes) -> Self {
		Self::Bytes(value)
	}
}

impl From<&'static [u8]> for DirectBody {
	fn from(value: &'static [u8]) -> Self {
		Self::Bytes(bytes::Bytes::from_static(value))
	}
}

impl From<&'static str> for DirectBody {
	fn from(value: &'static str) -> Self {
		Self::Bytes(bytes::Bytes::from_static(value.as_bytes()))
	}
}

impl From<String> for DirectBody {
	fn from(value: String) -> Self {
		Self::Bytes(value.into())
	}
}

pub(crate) fn body_stream_from_direct_body(body: DirectBody) -> abi::AgwBodyStream {
	match body {
		DirectBody::Bytes(bytes) if bytes.is_empty() => abi::AgwBodyStream::default(),
		DirectBody::Bytes(bytes) => body_to_abi(Full::new(bytes).map_err(Error::msg)),
		DirectBody::Body(body) => body_to_abi(body),
	}
}

/// Host or plugin body stream implementing [`http_body::Body`].
///
/// Use this directly when passing a request or response body into another HTTP
/// stack. [`BodyStream::next_chunk`] is available for simple byte-chunk loops
/// that do not need trailers.
pub struct BodyStream {
	stream: abi::AgwBodyStream,
	context_name: &'static str,
	ended: bool,
	ready_error: Option<Error>,
}

// SAFETY: `BodyStream` owns a host stream. The stream's state is opaque and is
// only accessed through host-provided poll/drop callbacks.
unsafe impl Send for BodyStream {}

impl BodyStream {
	pub(crate) fn new(handle: &ContextHandle, context_name: &'static str) -> Self {
		let result = handle.call(
			|vtable| vtable.body_stream,
			"host did not expose the body stream capability",
			|body_stream, ctx| Ok(unsafe { body_stream(ctx) }),
		);
		let (stream, ready_error) = match result {
			Ok(stream) => (stream, None),
			Err(error) => (abi::AgwBodyStream::default(), Some(error)),
		};
		Self {
			stream,
			context_name,
			ended: false,
			ready_error,
		}
	}

	pub(crate) fn from_abi(context_name: &'static str, stream: abi::AgwBodyStream) -> Self {
		Self {
			stream,
			context_name,
			ended: false,
			ready_error: None,
		}
	}

	/// Reads the next data chunk, returning `None` at end of stream.
	///
	/// Trailer frames are skipped. Use the [`http_body::Body`] implementation if
	/// trailers matter for the plugin.
	pub fn next_chunk(&mut self) -> BodyStreamNext<'_> {
		BodyStreamNext { stream: self }
	}

	/// Buffers all remaining chunks up to `limit` bytes.
	///
	/// This consumes the stream and returns an error if the buffered body would
	/// exceed `limit`.
	pub async fn collect(self, limit: usize) -> Result<Vec<u8>> {
		Ok(self.collect_with_trailers(limit).await?.0)
	}

	/// Like [`Self::collect`], but also returns the trailers, if any.
	pub(crate) async fn collect_with_trailers(
		mut self,
		limit: usize,
	) -> Result<(Vec<u8>, Option<HeaderMap>)> {
		let mut body = Vec::new();
		let mut trailers: Option<HeaderMap> = None;
		while let Some(frame) = self.frame().await {
			let frame = match frame?.into_data() {
				Ok(chunk) => {
					if body.len().saturating_add(chunk.len()) > limit {
						return Err(Error::msg(format!(
							"{} body exceeded limit of {limit} bytes",
							self.context_name
						)));
					}
					body.extend_from_slice(&chunk);
					continue;
				},
				Err(frame) => frame,
			};
			match frame.into_trailers() {
				Ok(t) => trailers.get_or_insert_default().extend(t),
				Err(_) => {
					return Err(Error::msg(format!(
						"host {} body stream returned an unsupported frame",
						self.context_name
					)));
				},
			}
		}
		Ok((body, trailers))
	}

	fn drop_inner(&mut self) {
		// Later polls must not reach the host with a freed state.
		self.stream.poll_next = None;
		if let Some(drop) = self.stream.drop.take()
			&& !self.stream.state.is_null()
		{
			unsafe { drop(self.stream.state) };
		}
		self.stream.state = std::ptr::null_mut();
	}
}

/// Hands a fully buffered body, and its trailers, back to the host as the active body.
pub(crate) fn restore_body(
	handle: &ContextHandle,
	body: &[u8],
	trailers: Option<HeaderMap>,
) -> Result<()> {
	handle.call(
		|vtable| vtable.set_body_stream,
		"host did not expose body replacement capability",
		|set_body_stream, ctx| unsafe {
			let body = Full::new(bytes::Bytes::copy_from_slice(body))
				.map_err(Error::msg)
				.with_trailers(std::future::ready(trailers.map(Ok)));
			// The host owns the stream even when this fails.
			let status = set_body_stream(ctx, body_to_abi(body));
			status_to_result(status, "restore body")
		},
	)
}

/// Future returned by [`BodyStream::next_chunk`].
pub struct BodyStreamNext<'a> {
	stream: &'a mut BodyStream,
}

impl Future for BodyStreamNext<'_> {
	type Output = Result<Option<bytes::Bytes>>;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.get_mut();
		loop {
			match http_body::Body::poll_frame(Pin::new(&mut *this.stream), cx) {
				Poll::Pending => return Poll::Pending,
				Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
					Ok(bytes) => return Poll::Ready(Ok(Some(bytes))),
					Err(frame) => match frame.into_trailers() {
						Ok(_) => continue,
						Err(_) => {
							return Poll::Ready(Err(Error::msg(
								"host body stream returned an unsupported frame",
							)));
						},
					},
				},
				Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
				Poll::Ready(None) => return Poll::Ready(Ok(None)),
			}
		}
	}
}

impl Drop for BodyStream {
	fn drop(&mut self) {
		self.drop_inner();
	}
}

impl http_body::Body for BodyStream {
	type Data = bytes::Bytes;
	type Error = Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
		let this = self.get_mut();
		if let Some(error) = this.ready_error.take() {
			this.ended = true;
			return Poll::Ready(Some(Err(error)));
		}
		if this.ended {
			return Poll::Ready(None);
		}
		let Some(poll_next) = this.stream.poll_next else {
			this.drop_inner();
			return Poll::Ready(Some(Err(Error::msg(format!(
				"host {} body stream has no poll_next function",
				this.context_name
			)))));
		};
		let mut out = abi::AgwBodyChunk::default();
		match abi::with_borrowed_waker(cx.waker(), |waker| unsafe {
			poll_next(this.stream.state, waker, &mut out)
		}) {
			abi::AGW_POLL_PENDING => {
				// Outputs are freed whatever the status; a PENDING poll may have written one.
				unsafe { out.free() };
				Poll::Pending
			},
			abi::AGW_POLL_READY => {
				let result = match out.kind {
					abi::AGW_BODY_FRAME_END if out.error.code != 0 || out.error.message.len != 0 => {
						Some(Err(error_from_abi(&out.error)))
					},
					abi::AGW_BODY_FRAME_DATA => {
						let bytes = bytes::Bytes::copy_from_slice(unsafe { out.bytes.as_slice() });
						Some(Ok(http_body::Frame::data(bytes)))
					},
					abi::AGW_BODY_FRAME_TRAILERS => {
						Some(headers_from_abi(out.trailers).map(http_body::Frame::trailers))
					},
					abi::AGW_BODY_FRAME_END => {
						this.ended = true;
						None
					},
					other => Some(Err(Error::msg(format!(
						"host {} body stream returned unknown frame kind {other}",
						this.context_name
					)))),
				};
				unsafe { out.free() };
				Poll::Ready(result)
			},
			status => {
				unsafe { out.free() };
				this.drop_inner();
				Poll::Ready(Some(Err(Error::msg(format!(
					"host {} body stream returned unknown poll status {status}",
					this.context_name
				)))))
			},
		}
	}
}

#[cfg(test)]
mod audit_tests {
	use std::ffi::c_void;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::task::Waker;

	use http_body::Body as _;

	use super::*;

	static OUTPUT_DROPS: AtomicUsize = AtomicUsize::new(0);

	unsafe extern "C" fn drop_output(data: *mut c_void) {
		OUTPUT_DROPS.fetch_add(1, Ordering::SeqCst);
		if !data.is_null() {
			unsafe { drop(Box::from_raw(data.cast::<u8>())) };
		}
	}

	unsafe extern "C" fn writes_output_then_returns_pending(
		_state: *mut c_void,
		_waker: *const abi::AgwRawWaker,
		out: *mut abi::AgwBodyChunk,
	) -> u32 {
		let owner = Box::into_raw(Box::new(1u8));
		let value = abi::AgwBodyChunk {
			private_data: owner.cast::<c_void>(),
			drop: Some(drop_output),
			..Default::default()
		};
		if let Err(mut value) = unsafe { abi::write_sized(out, value) } {
			unsafe { value.free() };
		}
		abi::AGW_POLL_PENDING
	}

	#[test]
	fn poll_frees_host_written_output_even_when_pending() {
		OUTPUT_DROPS.store(0, Ordering::SeqCst);
		let mut body = BodyStream::from_abi(
			"audit",
			abi::AgwBodyStream {
				state: std::ptr::null_mut(),
				poll_next: Some(writes_output_then_returns_pending),
				drop: None,
			},
		);
		let mut cx = Context::from_waker(Waker::noop());
		assert!(Pin::new(&mut body).poll_frame(&mut cx).is_pending());
		let drops = OUTPUT_DROPS.load(Ordering::SeqCst);
		assert_eq!(drops, 1);
	}
}
