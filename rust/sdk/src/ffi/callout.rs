use super::super::*;

#[allow(dead_code)]
pub(crate) struct FfiHostHttpRequest {
	method: Vec<u8>,
	uri: Vec<u8>,
	headers: Vec<FfiOwnedHeader>,
	header_views: Vec<abi::AgwHeader>,
	request: abi::AgwHttpRequest,
}

impl FfiHostHttpRequest {
	pub(crate) fn new(request: HttpRequest) -> Self {
		let method = request.method.as_str().as_bytes().to_vec();
		let uri = request.uri.to_string().into_bytes();
		let headers = request
			.headers
			.iter()
			.map(|(name, value)| FfiOwnedHeader {
				name: name.as_str().as_bytes().to_vec(),
				value: value.as_bytes().to_vec(),
			})
			.collect::<Vec<_>>();
		let header_views = headers
			.iter()
			.map(|header| abi::AgwHeader {
				name: abi::AgwString::from_borrowed(
					std::str::from_utf8(&header.name).expect("header name is valid UTF-8"),
				),
				value: abi::AgwBytes::from_slice(&header.value),
			})
			.collect::<Vec<_>>();
		let body_stream = body_stream_from_direct_body(request.body);
		let request = abi::AgwHttpRequest {
			size: std::mem::size_of::<abi::AgwHttpRequest>(),
			// Reserved for host backend references; absolute URLs need none.
			backend: abi::AgwString::empty(),
			method: abi::AgwString::from_borrowed(std::str::from_utf8(&method).expect("method is UTF-8")),
			uri: abi::AgwString::from_borrowed(std::str::from_utf8(&uri).expect("URI is UTF-8")),
			headers: abi::AgwHeaders::from_slice(&header_views),
			body_stream,
		};
		Self {
			method,
			uri,
			headers,
			header_views,
			request,
		}
	}

	pub(crate) fn as_ffi(&self) -> &abi::AgwHttpRequest {
		// `request` contains borrowed pointers into this struct's owned buffers.
		// Returning `&self.request` ties those pointers to `self`'s lifetime.
		&self.request
	}
}

struct FfiOwnedHeader {
	name: Vec<u8>,
	value: Vec<u8>,
}

pub(crate) unsafe fn response_from_ffi(
	response: &mut abi::AgwHttpResponse,
) -> Result<HttpResponse> {
	if !response.error.is_empty() {
		return Err(error_from_abi(&response.error));
	}
	let headers = headers_from_abi(response.headers)?;
	let body = std::mem::take(&mut response.body_stream);
	Ok(HttpResponse {
		status: response.status,
		headers,
		body: BodyStream::from_abi("HTTP call response", body),
	})
}
