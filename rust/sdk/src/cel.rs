//! CEL compilation, evaluation, properties, and metadata helpers.

use super::*;

/// Compiles a CEL expression against the host.
///
/// Use [`CelExpression`] for expressions that should be compiled lazily and
/// reused by a plugin instance.
pub fn compile_cel(expression: &str) -> Result<CompiledCelExpression> {
	CompiledCelExpression::compile(&context::host()?, expression)
}

fn eval_cel(handle: &ContextHandle, expression: &CompiledCelExpression) -> Result<CelResult> {
	handle.call(
		|vtable| vtable.eval_cel,
		"host did not expose the CEL capability",
		|eval_cel, ctx| {
			let mut out = crate::ffi::HostOutput::<abi::AgwCelResult>::new();
			let mut err = crate::ffi::HostOutput::<abi::AgwError>::new();
			let status = unsafe { eval_cel(ctx, &expression.expression, &mut out.0, &mut err.0) };
			match status {
				abi::AGW_STATUS_OK => unsafe { CelResult::from_raw(std::mem::take(&mut out.0)) }?
					.ok_or_else(|| Error::msg("host returned no value evaluating CEL")),
				abi::AGW_STATUS_ERROR => Err(error_from_abi(&err.0)),
				other => Err(Error::msg(format!(
					"host returned unknown status {other} evaluating CEL"
				))),
			}
		},
	)
}

pub(crate) fn property(handle: &ContextHandle, path: &[&str]) -> Result<Option<CelResult>> {
	handle.call(
		|vtable| vtable.get_property,
		"host did not expose the property capability",
		|get_property, ctx| {
			let path = path
				.iter()
				.map(|segment| abi::AgwString::from_borrowed(segment))
				.collect::<Vec<_>>();
			let mut out = crate::ffi::HostOutput::<abi::AgwCelResult>::new();
			let mut err = crate::ffi::HostOutput::<abi::AgwError>::new();
			let status = unsafe {
				get_property(
					ctx,
					abi::AgwStringSlice::from_slice(&path),
					&mut out.0,
					&mut err.0,
				)
			};
			match status {
				abi::AGW_STATUS_OK => unsafe { CelResult::from_raw(std::mem::take(&mut out.0)) },
				abi::AGW_STATUS_ERROR => Err(error_from_abi(&err.0)),
				other => Err(Error::msg(format!(
					"host returned unknown status {other} getting property"
				))),
			}
		},
	)
}

pub(crate) fn set_metadata<T: MetadataValue + ?Sized>(
	handle: &ContextHandle,
	key: &str,
	value: &T,
) -> Result<()> {
	if key.is_empty() {
		return Err(Error::msg("metadata key must not be empty"));
	}
	let value = value.metadata()?;
	value.with_abi(|value| {
		handle.call(
			|vtable| vtable.set_metadata,
			"host did not expose the metadata capability",
			|set_metadata, ctx| {
				let mut err = crate::ffi::HostOutput::<abi::AgwError>::new();
				let status =
					unsafe { set_metadata(ctx, abi::AgwString::from_borrowed(key), value, &mut err.0) };
				match status {
					abi::AGW_STATUS_OK => Ok(()),
					abi::AGW_STATUS_ERROR => Err(error_from_abi(&err.0)),
					other => Err(Error::msg(format!(
						"host returned unknown status {other} setting metadata"
					))),
				}
			},
		)
	})
}

/// Lazily compiled CEL expression.
///
/// Store this on a plugin instance when the same expression is evaluated many
/// times. The first evaluation compiles the expression; later evaluations reuse
/// the host-owned compiled handle.
pub struct CelExpression {
	expression: Cow<'static, str>,
	compiled: OnceLock<std::result::Result<CompiledCelExpression, String>>,
}

// SAFETY: `CelExpression` owns immutable expression text and a `OnceLock`.
// Compiled handles are host-owned resources whose thread-safety is part of the
// host ABI contract.
unsafe impl Send for CelExpression {}
// SAFETY: Same as `Send`; mutation is limited to `OnceLock` initialization.
unsafe impl Sync for CelExpression {}

impl CelExpression {
	/// Creates an expression from a static string.
	pub const fn new(expression: &'static str) -> Self {
		Self {
			expression: Cow::Borrowed(expression),
			compiled: OnceLock::new(),
		}
	}

	/// Creates an expression from owned text.
	pub fn owned(expression: impl Into<String>) -> Self {
		Self {
			expression: Cow::Owned(expression.into()),
			compiled: OnceLock::new(),
		}
	}

	/// Evaluates the expression against `request`.
	pub fn evaluate(&self, request: &RequestContext) -> Result<CelResult> {
		eval_cel(&request.handle, self.compiled(request.handle.host())?)
	}

	/// Evaluates the expression against `response`.
	pub fn evaluate_response(&self, response: &ResponseContext) -> Result<CelResult> {
		eval_cel(&response.handle, self.compiled(response.handle.host())?)
	}

	fn compiled(&self, host: &Host) -> Result<&CompiledCelExpression> {
		self
			.compiled
			.get_or_init(|| {
				CompiledCelExpression::compile(host, &self.expression).map_err(|error| error.to_string())
			})
			.as_ref()
			.map_err(|error| Error::msg(error.clone()))
	}
}

/// Host-owned compiled CEL expression.
///
/// Dropping this value releases the compiled expression on the host.
pub struct CompiledCelExpression {
	expression: abi::AgwCelExpression,
}

impl std::fmt::Debug for CompiledCelExpression {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CompiledCelExpression")
			.finish_non_exhaustive()
	}
}

// SAFETY: The handle is opaque host state with host-provided drop. The host ABI
// requires compiled CEL handles to remain valid across request polls until drop.
unsafe impl Send for CompiledCelExpression {}
// SAFETY: Same as `Send`; SDK never mutates the opaque handle directly.
unsafe impl Sync for CompiledCelExpression {}

impl CompiledCelExpression {
	fn compile(host: &Host, expression: &str) -> Result<Self> {
		let Some(compile_cel) = host.vtable.compile_cel else {
			return Err(Error::msg("host did not expose the CEL compile capability"));
		};
		let mut out = crate::ffi::HostOutput::<abi::AgwCelExpression>::new();
		let mut err = crate::ffi::HostOutput::<abi::AgwError>::new();
		let status = unsafe {
			compile_cel(
				host.vtable.data,
				abi::AgwString::from_borrowed(expression),
				&mut out.0,
				&mut err.0,
			)
		};
		match status {
			// Move the expression out of the guard; it is freed by `Self`'s drop.
			abi::AGW_STATUS_OK => Ok(Self {
				expression: std::mem::take(&mut out.0),
			}),
			abi::AGW_STATUS_ERROR => Err(error_from_abi(&err.0)),
			other => Err(Error::msg(format!(
				"host returned unknown status {other} compiling CEL"
			))),
		}
	}
}

impl Drop for CompiledCelExpression {
	fn drop(&mut self) {
		unsafe { self.expression.free() };
	}
}

#[cfg(test)]
mod audit_tests {
	use std::ffi::c_void;
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::*;

	static EXPRESSION_DROPS: AtomicUsize = AtomicUsize::new(0);

	unsafe extern "C" fn drop_expression(data: *mut c_void) {
		EXPRESSION_DROPS.fetch_add(1, Ordering::SeqCst);
		if !data.is_null() {
			unsafe { drop(Box::from_raw(data.cast::<u8>())) };
		}
	}

	unsafe extern "C" fn writes_expression_then_returns_error(
		_data: *const c_void,
		_expression: abi::AgwString,
		out: *mut abi::AgwCelExpression,
		err: *mut abi::AgwError,
	) -> u32 {
		unsafe {
			*out = abi::AgwCelExpression {
				state: Box::into_raw(Box::new(1u8)).cast::<c_void>(),
				drop: Some(drop_expression),
			};
			*err = abi::AgwError {
				code: abi::AGW_ERROR_UNKNOWN,
				message: abi::AgwString::from_static("compile failed"),
				private_data: std::ptr::null_mut(),
				drop: None,
			};
		}
		abi::AGW_STATUS_ERROR
	}

	#[test]
	fn compile_frees_expression_output_on_error() {
		EXPRESSION_DROPS.store(0, Ordering::SeqCst);
		let host = Host {
			vtable: abi::AgwHostVTable {
				compile_cel: Some(writes_expression_then_returns_error),
				..Default::default()
			},
		};
		assert!(CompiledCelExpression::compile(&host, "true").is_err());
		let drops = EXPRESSION_DROPS.load(Ordering::SeqCst);
		assert_eq!(drops, 1);
	}
}
