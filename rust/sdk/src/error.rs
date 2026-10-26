//! SDK error and result aliases.

use super::*;

/// Error type used by the Rust plugin SDK.
pub type Error = anyhow::Error;

/// Result type used by the Rust plugin SDK.
pub type Result<T> = anyhow::Result<T>;

pub(crate) fn owned_error(message: impl Into<String>) -> abi::AgwError {
	let bytes = message.into().into_bytes();
	let boxed = Box::new(bytes);
	let message = std::str::from_utf8(boxed.as_slice()).expect("SDK errors are valid UTF-8");
	abi::AgwError {
		code: abi::AGW_ERROR_UNKNOWN,
		message: abi::AgwString::from_borrowed(message),
		private_data: Box::into_raw(boxed).cast::<c_void>(),
		drop: Some(drop_vec_u8),
	}
}

pub(crate) unsafe fn write_error(out: *mut abi::AgwError, message: impl Into<String>) {
	if !out.is_null() {
		unsafe {
			*out = owned_error(message);
		}
	}
}

pub(crate) fn error_from_abi(error: &abi::AgwError) -> Error {
	Error::msg(unsafe { error.message.as_str() }.to_owned())
}

unsafe extern "C" fn drop_vec_u8(data: *mut c_void) {
	if !data.is_null() {
		unsafe {
			drop(Box::from_raw(data.cast::<Vec<u8>>()));
		}
	}
}
