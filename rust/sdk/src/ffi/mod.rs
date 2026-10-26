pub(crate) mod body;
pub(crate) mod callout;
pub(crate) mod plugin;

/// Frees a host-written output (`AgwOwnedBytes`, `AgwError`) however the caller
/// returns, including `?` and early returns.
pub(crate) struct HostOutput<T: HostFree>(pub(crate) T);

pub(crate) trait HostFree {
	/// # Safety
	/// `self` must be default-initialized or written by the host.
	unsafe fn free(&mut self);
}

impl HostFree for agentgateway_abi::AgwOwnedBytes {
	unsafe fn free(&mut self) {
		unsafe { agentgateway_abi::AgwOwnedBytes::free(self) }
	}
}

impl HostFree for agentgateway_abi::AgwCelExpression {
	unsafe fn free(&mut self) {
		unsafe { agentgateway_abi::AgwCelExpression::free(self) }
	}
}

impl HostFree for agentgateway_abi::AgwCelResult {
	unsafe fn free(&mut self) {
		unsafe { agentgateway_abi::AgwCelResult::free(self) }
	}
}

impl HostFree for agentgateway_abi::AgwError {
	unsafe fn free(&mut self) {
		unsafe { agentgateway_abi::AgwError::free(self) }
	}
}

impl<T: HostFree + Default> HostOutput<T> {
	pub(crate) fn new() -> Self {
		Self(T::default())
	}
}

impl<T: HostFree> Drop for HostOutput<T> {
	fn drop(&mut self) {
		// SAFETY: only constructed default-initialized, then written by the host.
		unsafe { self.0.free() }
	}
}
