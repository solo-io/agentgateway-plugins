use super::super::*;

pub(crate) fn body_to_abi<B>(body: B) -> abi::AgwBodyStream
where
	B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
	B::Error: Display,
{
	abi::AgwBodyStream {
		state: Box::into_raw(Box::new(Box::pin(body))).cast::<c_void>(),
		poll_next: Some(poll_plugin_body_stream::<B>),
		drop: Some(drop_plugin_body_stream::<B>),
	}
}

unsafe extern "C" fn poll_plugin_body_stream<B>(
	state: *mut c_void,
	waker: *const abi::AgwRawWaker,
	out: *mut abi::AgwBodyChunk,
) -> u32
where
	B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
	B::Error: Display,
{
	let result = catch_unwind(AssertUnwindSafe(|| unsafe {
		let body = &mut *(state.cast::<Pin<Box<B>>>());
		loop {
			match abi::with_context(waker, |cx| body.as_mut().poll_frame(cx)) {
				Poll::Pending => return abi::AGW_POLL_PENDING,
				Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
					Ok(bytes) => {
						write_owned_data_chunk(out, bytes.to_vec());
						break;
					},
					Err(frame) => match frame.into_trailers() {
						Ok(trailers) => {
							write_owned_trailer_chunk(out, trailers);
							break;
						},
						Err(_) => continue,
					},
				},
				Poll::Ready(Some(Err(error))) => {
					write_owned_error_chunk(out, error.to_string());
					break;
				},
				Poll::Ready(None) => {
					write_owned_end_chunk(out);
					break;
				},
			}
		}
		abi::AGW_POLL_READY
	}));
	match result {
		Ok(status) => status,
		Err(_) => {
			unsafe {
				write_owned_error_chunk(out, "plugin body stream panicked");
			}
			abi::AGW_POLL_READY
		},
	}
}

unsafe extern "C" fn drop_plugin_body_stream<B>(state: *mut c_void)
where
	B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
	B::Error: Display,
{
	abi::catch_panic((), || {
		if !state.is_null() {
			unsafe {
				drop(Box::from_raw(state.cast::<Pin<Box<B>>>()));
			}
		}
	})
}

struct OwnedChunk {
	bytes: Vec<u8>,
	_trailers: Vec<FfiOwnedHeader>,
	trailer_views: Vec<abi::AgwHeader>,
	error_message: Vec<u8>,
}

struct FfiOwnedHeader {
	name: Vec<u8>,
	value: Vec<u8>,
}

pub(crate) unsafe fn write_owned_data_chunk(out: *mut abi::AgwBodyChunk, bytes: Vec<u8>) {
	unsafe {
		write_owned_chunk(
			out,
			abi::AGW_BODY_FRAME_DATA,
			bytes,
			HeaderMap::new(),
			Vec::new(),
		)
	};
}

pub(crate) unsafe fn write_owned_trailer_chunk(out: *mut abi::AgwBodyChunk, trailers: HeaderMap) {
	unsafe {
		write_owned_chunk(
			out,
			abi::AGW_BODY_FRAME_TRAILERS,
			Vec::new(),
			trailers,
			Vec::new(),
		)
	};
}

pub(crate) unsafe fn write_owned_end_chunk(out: *mut abi::AgwBodyChunk) {
	unsafe {
		write_owned_chunk(
			out,
			abi::AGW_BODY_FRAME_END,
			Vec::new(),
			HeaderMap::new(),
			Vec::new(),
		)
	};
}

pub(crate) unsafe fn write_owned_error_chunk(
	out: *mut abi::AgwBodyChunk,
	message: impl Into<String>,
) {
	unsafe {
		write_owned_chunk(
			out,
			abi::AGW_BODY_FRAME_END,
			Vec::new(),
			HeaderMap::new(),
			message.into().into_bytes(),
		)
	};
}

unsafe fn write_owned_chunk(
	out: *mut abi::AgwBodyChunk,
	kind: u32,
	bytes: Vec<u8>,
	trailers: HeaderMap,
	error_message: Vec<u8>,
) {
	if !out.is_null() {
		let trailers = trailers
			.iter()
			.map(|(name, value)| FfiOwnedHeader {
				name: name.as_str().as_bytes().to_vec(),
				value: value.as_bytes().to_vec(),
			})
			.collect::<Vec<_>>();
		let trailer_views = trailers
			.iter()
			.map(|header| abi::AgwHeader {
				name: abi::AgwString::from_borrowed(
					std::str::from_utf8(&header.name).expect("header name is valid UTF-8"),
				),
				value: abi::AgwBytes::from_slice(&header.value),
			})
			.collect::<Vec<_>>();
		let boxed = Box::new(OwnedChunk {
			bytes,
			_trailers: trailers,
			trailer_views,
			error_message,
		});
		let chunk = Box::into_raw(boxed);
		let owned = unsafe { &*chunk };
		let error_message =
			std::str::from_utf8(&owned.error_message).unwrap_or("plugin body stream error");
		let value = abi::AgwBodyChunk {
			kind,
			bytes: abi::AgwBytes::from_slice(&owned.bytes),
			trailers: abi::AgwHeaders::from_slice(&owned.trailer_views),
			error: abi::AgwError {
				code: if owned.error_message.is_empty() {
					0
				} else {
					abi::AGW_ERROR_UNKNOWN
				},
				message: abi::AgwString::from_borrowed(error_message),
				private_data: std::ptr::null_mut(),
				drop: None,
			},
			private_data: chunk.cast::<c_void>(),
			drop: Some(drop_owned_chunk),
			..Default::default()
		};
		if let Err(mut value) = unsafe { abi::write_sized(out, value) } {
			unsafe { value.free() };
		}
	}
}

unsafe extern "C" fn drop_owned_chunk(data: *mut c_void) {
	if !data.is_null() {
		unsafe {
			drop(Box::from_raw(data.cast::<OwnedChunk>()));
		}
	}
}
