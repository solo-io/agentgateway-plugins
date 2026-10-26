//! Host runtime helpers available to plugins.

use super::*;

/// Sleeps on the host runtime without blocking the plugin thread.
pub fn sleep(duration: Duration) -> Sleep {
	Sleep {
		host: context::host().map(|host| host.vtable).ok(),
		duration,
		future: None,
	}
}

/// Reports whether a log record at `level` would be recorded by the host.
/// Use [`log!`](crate::log!) to skip formatting disabled messages.
pub fn log_enabled(level: LogLevel) -> bool {
	let Ok(host) = context::host() else {
		return false;
	};
	match host.vtable.log_enabled {
		Some(enabled) => unsafe { enabled(host.vtable.data, level as u32) },
		None => host.vtable.log.is_some(),
	}
}

/// Emits a plugin log record through the host logger, formatting the message
/// only if `level` is enabled.
///
/// ```rust,ignore
/// log!(LogLevel::Debug, "claims: {claims:?}");
/// ```
#[macro_export]
macro_rules! log {
	($level:expr, $($arg:tt)+) => {{
		let level = $level;
		if $crate::log_enabled(level) {
			$crate::log(level, ::std::format!($($arg)+));
		}
	}};
}

/// Emits a plugin log record through the host logger.
pub fn log(level: LogLevel, message: impl AsRef<str>) {
	if let Ok(host) = context::host()
		&& let Some(log) = host.vtable.log
	{
		unsafe {
			log(
				host.vtable.data,
				level as u32,
				abi::AgwString::from_borrowed(message.as_ref()),
			)
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// Log level for [`log`].
pub enum LogLevel {
	/// Very fine-grained diagnostic logging.
	Trace = abi::AGW_LOG_TRACE,
	/// Debug logging for plugin development and troubleshooting.
	Debug = abi::AGW_LOG_DEBUG,
	/// Informational logging for normal plugin events.
	Info = abi::AGW_LOG_INFO,
	/// Warning logging for unusual but non-fatal conditions.
	Warn = abi::AGW_LOG_WARN,
	/// Error logging for plugin failures.
	Error = abi::AGW_LOG_ERROR,
}

/// Future returned by [`sleep`].
pub struct Sleep {
	host: Option<abi::AgwHostVTable>,
	duration: Duration,
	future: Option<HostUnitFuture>,
}

// SAFETY: `Sleep` owns a host future. Host futures must support being held in
// the plugin future and polled/dropped by the host executor that owns the
// request.
unsafe impl Send for Sleep {}

impl Future for Sleep {
	type Output = Result<()>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		if self.future.is_none() {
			let Some(host) = self.host else {
				return Poll::Ready(Err(Error::msg("no active plugin host context")));
			};
			let Some(sleep) = host.sleep else {
				return Poll::Ready(Err(Error::msg("host did not expose the sleep capability")));
			};
			let millis = self.duration.as_millis().min(u64::MAX as u128) as u64;
			self.future = Some(HostUnitFuture::new("sleep", unsafe {
				sleep(host.data, millis)
			}));
		}

		let future = self.future.as_mut().expect("future initialized");
		Pin::new(future).poll(cx)
	}
}

struct HostUnitFuture {
	name: &'static str,
	future: abi::AgwUnitFuture,
}

// SAFETY: `HostUnitFuture` owns a host future. Host futures must support being
// held in the plugin future and polled/dropped by the host executor that owns
// the request.
unsafe impl Send for HostUnitFuture {}

impl HostUnitFuture {
	fn new(name: &'static str, future: abi::AgwUnitFuture) -> Self {
		Self { name, future }
	}

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

impl Future for HostUnitFuture {
	type Output = Result<()>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let Some(poll) = self.future.poll else {
			self.drop_inner();
			return Poll::Ready(Err(Error::msg(format!(
				"host {} future has no poll function",
				self.name
			))));
		};
		let mut err = abi::AgwError::default();
		let status = abi::with_borrowed_waker(cx.waker(), |waker| unsafe {
			poll(self.future.state, waker, &mut err)
		});
		match status {
			abi::AGW_POLL_PENDING => {
				// Outputs are freed whatever the status; a PENDING poll may have written one.
				unsafe { err.free() };
				Poll::Pending
			},
			abi::AGW_POLL_READY => {
				self.drop_inner();
				let result = if err.is_empty() {
					Poll::Ready(Ok(()))
				} else {
					Poll::Ready(Err(error_from_abi(&err)))
				};
				unsafe { err.free() };
				result
			},
			_ => {
				unsafe { err.free() };
				self.drop_inner();
				Poll::Ready(Err(Error::msg(format!(
					"host {} future returned unknown poll status {status}",
					self.name
				))))
			},
		}
	}
}

impl Drop for HostUnitFuture {
	fn drop(&mut self) {
		self.drop_inner();
	}
}
