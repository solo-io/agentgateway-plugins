//! Header helper types used by plugin action results.

use super::*;

/// Header value used by action results and direct responses.
#[derive(Debug, Clone)]
pub struct Header {
	/// Header name.
	pub name: HeaderName,
	/// Header value.
	pub value: HeaderValue,
}

impl Header {
	/// Creates a direct-response or callout header.
	pub fn new(name: HeaderName, value: HeaderValue) -> Self {
		Self { name, value }
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PseudoHeader {
	Method,
	Scheme,
	Authority,
	Path,
	Status,
}

impl PseudoHeader {
	pub(crate) const fn as_str(self) -> &'static str {
		match self {
			Self::Method => ":method",
			Self::Scheme => ":scheme",
			Self::Authority => ":authority",
			Self::Path => ":path",
			Self::Status => ":status",
		}
	}
}

/// Reads the first value of a header or pseudo-header. Returns `None` if it is
/// absent or the context is no longer valid.
pub(crate) fn header_value_from_host(handle: &ContextHandle, name: &str) -> Option<HeaderValue> {
	handle
		.call(
			|vtable| vtable.get_header,
			"host did not expose header get capability",
			|get_header, ctx| {
				let mut value = crate::ffi::HostOutput::<abi::AgwOwnedBytes>::new();
				let status = unsafe { get_header(ctx, abi::AgwString::from_borrowed(name), &mut value.0) };
				status_to_result(status, "get header")?;
				if value.0.bytes.ptr.is_null() && value.0.bytes.len == 0 {
					return Ok(None);
				}
				Ok(HeaderValue::from_bytes(unsafe { value.0.bytes.as_slice() }).ok())
			},
		)
		.ok()
		.flatten()
}

pub(crate) fn headers_from_abi(headers: abi::AgwHeaders) -> Result<HeaderMap> {
	let mut map = HeaderMap::with_capacity(headers.len);
	for header in unsafe { headers.as_slice() } {
		let name = HeaderName::from_bytes(unsafe { header.name.as_bytes() }).map_err(Error::msg)?;
		let value = HeaderValue::from_bytes(unsafe { header.value.as_slice() }).map_err(Error::msg)?;
		map.append(name, value);
	}
	Ok(map)
}
