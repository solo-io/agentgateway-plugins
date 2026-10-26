//! Stable C ABI surface for agentgateway native plugins.
//!
//! This crate intentionally exposes only `#[repr(C)]` data structures, raw
//! pointers, and `extern "C"` function pointers. Higher-level Rust ergonomics
//! live in `agentgateway-sdk`.

use std::ffi::c_void;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};
use std::{mem, ptr};

/// First public plugin ABI version.
///
/// This ABI is still native/trusted code, but this value is the compatibility
/// boundary between host and plugin. Breaking layout or callback signature
/// changes must bump it; append-only `AgwHostVTable` additions are covered by
/// the vtable `size` field.
pub const AGW_PLUGIN_ABI_VERSION: u32 = 1;
pub const AGW_PLUGIN_ENTRYPOINT: &[u8] = b"agentgateway_plugin_entrypoint\0";

pub const AGW_STATUS_OK: u32 = 0;
pub const AGW_STATUS_ERROR: u32 = 1;

pub const AGW_POLL_PENDING: u32 = 0;
pub const AGW_POLL_READY: u32 = 1;

pub const AGW_APPLY_CONTINUE: u32 = 0;
pub const AGW_APPLY_DIRECT_RESPONSE: u32 = 1;
pub const AGW_APPLY_ERROR: u32 = 2;

/// Nonzero `AgwError.code` values. More specific codes may be added later, so
/// consumers must treat any unknown nonzero code as a generic error.
pub const AGW_ERROR_UNKNOWN: u32 = 1;

pub const AGW_BODY_FRAME_DATA: u32 = 0;
pub const AGW_BODY_FRAME_TRAILERS: u32 = 1;
pub const AGW_BODY_FRAME_END: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgwBytes {
	/// Borrowed byte pointer. The ABI never infers ownership from this field.
	/// Lifetime is defined by the enclosing struct or callback that returned it.
	pub ptr: *const u8,
	pub len: usize,
}

impl AgwBytes {
	pub const fn empty() -> Self {
		Self {
			ptr: std::ptr::null(),
			len: 0,
		}
	}

	pub const fn from_static(s: &'static str) -> Self {
		Self {
			ptr: s.as_ptr(),
			len: s.len(),
		}
	}

	pub fn from_slice(slice: &[u8]) -> Self {
		Self {
			ptr: slice.as_ptr(),
			len: slice.len(),
		}
	}

	/// # Safety
	///
	/// `ptr` must be valid for `len` bytes and live at least as long as the
	/// returned borrow.
	pub unsafe fn as_slice<'a>(&self) -> &'a [u8] {
		if self.len == 0 {
			return &[];
		}
		unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
	}
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgwString {
	/// Borrowed UTF-8 byte pointer. This is intentionally not NUL-terminated so
	/// plugin SDKs can pass slices without allocating.
	pub ptr: *const u8,
	pub len: usize,
}

impl AgwString {
	pub const fn empty() -> Self {
		Self {
			ptr: std::ptr::null(),
			len: 0,
		}
	}

	pub const fn from_static(s: &'static str) -> Self {
		Self {
			ptr: s.as_ptr(),
			len: s.len(),
		}
	}

	pub fn from_borrowed(s: &str) -> Self {
		Self::from_ref(s)
	}

	pub fn from_ref(s: &str) -> Self {
		Self {
			ptr: s.as_ptr(),
			len: s.len(),
		}
	}

	/// # Safety
	///
	/// `ptr` must be valid for `len` bytes, live at least as long as the
	/// returned borrow, and contain valid UTF-8.
	pub unsafe fn as_str<'a>(&self) -> &'a str {
		if self.len == 0 {
			return "";
		}
		let bytes = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
		unsafe { std::str::from_utf8_unchecked(bytes) }
	}

	/// # Safety
	///
	/// `ptr` must be valid for `len` bytes and live at least as long as the
	/// returned borrow. The caller is responsible for UTF-8 validation if needed.
	pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
		if self.len == 0 {
			return &[];
		}
		unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
	}
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgwStringSlice {
	/// Borrowed contiguous array of `AgwString` values. The strings themselves
	/// have independent borrowed lifetimes owned by the caller.
	pub ptr: *const AgwString,
	pub len: usize,
}

impl AgwStringSlice {
	pub fn from_slice(strings: &[AgwString]) -> Self {
		Self {
			ptr: strings.as_ptr(),
			len: strings.len(),
		}
	}

	/// # Safety
	///
	/// `ptr` must be valid for `len` string entries and live at least as long as
	/// the returned borrow.
	pub unsafe fn as_slice<'a>(&self) -> &'a [AgwString] {
		if self.len == 0 {
			return &[];
		}
		unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
	}
}

/// Runs the body of an `extern "C"` entry point, returning `on_panic` if it
/// panics. A panic unwinding across the ABI aborts the process, so SDKs wrap
/// every callback that can run plugin-author code (callbacks, `Drop` impls).
pub fn catch_panic<R>(on_panic: R, f: impl FnOnce() -> R) -> R {
	std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(on_panic)
}

/// Drops opaque owned state created by the side that supplied this callback.
///
/// The pointer passed to this function must be the exact owner pointer stored in
/// the enclosing ABI value. The callback must be called at most once, and only
/// while the dynamic library that provided it remains loaded.
pub type AgwDropOwned = Option<unsafe extern "C" fn(*mut c_void)>;
unsafe fn free_owned(ptr: &mut *mut c_void, drop: &mut AgwDropOwned) {
	if let Some(drop) = drop.take()
		&& !ptr.is_null()
	{
		unsafe { drop(*ptr) };
	}
	*ptr = std::ptr::null_mut();
}

/// ABI structs that can grow append-only. The first field is `size: usize`, the
/// number of bytes of the struct populated by its producer; fields past `size`
/// are treated as their defaults.
///
/// Rules for adding fields:
/// - Append only, and start each new field at or after the previous
///   `size_of`, never in the old layout's tail padding.
/// - New fields must not own resources independently: anything they reference
///   must be released by an owner field that already existed (such as
///   `private_data`), since an older consumer never sees the new field.
/// - Structs embedded by value in another struct (`AgwDirectResponse`,
///   `AgwPluginResponseSession`, `AgwError`, `AgwBodyStream`, ...) are frozen.
///   Extend the outer sized struct instead.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]` with `size: usize` as their first field,
/// `Default` must set `size` to `size_of::<Self>()`, and `MIN_SIZE` must be the
/// end of the last field in the first ABI version.
pub unsafe trait AgwSized: Default {
	/// Smallest valid `size`: every field of the first ABI version, including
	/// ownership fields, is present. Never changes once released.
	const MIN_SIZE: usize;
}

/// End offset of `$field` in `$ty`, for `AgwSized::MIN_SIZE`.
macro_rules! end_of {
	($ty:ty, $field:ident: $field_ty:ty) => {
		mem::offset_of!($ty, $field) + mem::size_of::<$field_ty>()
	};
}

/// Reads a struct supplied by a producer that may be older or newer than this
/// side. Only the bytes both sides know about are copied.
///
/// # Safety
///
/// `ptr` must be null or point to at least its advertised `size` readable bytes,
/// and the populated fields must be valid for `T`.
///
/// This copies bytes, so it duplicates any owned handles in `T` (body streams,
/// sessions, ...): the caller must make sure each handle is freed or used
/// through exactly one copy, per the ABI's ownership rules.
pub unsafe fn read_sized<T: AgwSized>(ptr: *const T) -> Option<T> {
	if ptr.is_null() {
		return None;
	}
	let advertised_size = unsafe { ptr.cast::<usize>().read_unaligned() };
	if advertised_size < T::MIN_SIZE {
		return None;
	}
	let copy_size = advertised_size.min(mem::size_of::<T>());
	let mut out = T::default();
	unsafe {
		ptr::copy_nonoverlapping(
			ptr.cast::<u8>(),
			(&mut out as *mut T).cast::<u8>(),
			copy_size,
		);
		(&mut out as *mut T).cast::<usize>().write(copy_size);
	}
	Some(out)
}

/// Writes `value` into a consumer-allocated output whose `size` field holds the
/// consumer's capacity. Only the bytes both sides know about are written, and
/// `out.size` is set to the number of bytes written. If `out` is null or its
/// capacity is below `T::MIN_SIZE`, `value` is returned unwritten so the caller
/// can release it.
///
/// # Safety
///
/// `out` must be null or point to at least its advertised `size` writable bytes.
pub unsafe fn write_sized<T: AgwSized>(out: *mut T, value: T) -> Result<(), T> {
	if out.is_null() {
		return Err(value);
	}
	let capacity = unsafe { out.cast::<usize>().read_unaligned() };
	if capacity < T::MIN_SIZE {
		return Err(value);
	}
	let copy_size = capacity.min(mem::size_of::<T>());
	let value = mem::ManuallyDrop::new(value);
	unsafe {
		ptr::copy_nonoverlapping(
			(&*value as *const T).cast::<u8>(),
			out.cast::<u8>(),
			copy_size,
		);
		out.cast::<usize>().write_unaligned(copy_size);
	}
	Ok(())
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwError {
	/// Zero means no error. Nonzero values use `AGW_ERROR_*`; treat unknown
	/// nonzero values as a generic error.
	pub code: u32,
	/// Borrowed view into `private_data` or static memory.
	pub message: AgwString,
	/// Opaque owner for `message`; only `drop` may interpret it.
	pub private_data: *mut c_void,
	/// Releases `private_data`. May be absent for borrowed/static errors.
	pub drop: AgwDropOwned,
}

impl Default for AgwError {
	fn default() -> Self {
		Self {
			code: 0,
			message: AgwString::empty(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

impl AgwError {
	pub fn is_empty(&self) -> bool {
		self.code == 0 && self.message.len == 0
	}

	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this error, and the error must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
		self.code = 0;
		self.message = AgwString::empty();
	}
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwOwnedBytes {
	/// Borrowed view into `private_data`.
	pub bytes: AgwBytes,
	/// Opaque owner for `bytes`.
	pub private_data: *mut c_void,
	/// Releases `private_data`.
	pub drop: AgwDropOwned,
}

impl Default for AgwOwnedBytes {
	fn default() -> Self {
		Self {
			bytes: AgwBytes::empty(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

impl AgwOwnedBytes {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this value, and the value must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
		self.bytes = AgwBytes::empty();
	}
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgwHeader {
	pub name: AgwString,
	pub value: AgwBytes,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AgwHeaders {
	pub ptr: *const AgwHeader,
	pub len: usize,
}

impl AgwHeaders {
	pub fn from_slice(headers: &[AgwHeader]) -> Self {
		Self {
			ptr: headers.as_ptr(),
			len: headers.len(),
		}
	}

	/// # Safety
	///
	/// `ptr` must be valid for `len` header entries and live at least as long as
	/// the returned borrow.
	pub unsafe fn as_slice<'a>(&self) -> &'a [AgwHeader] {
		if self.len == 0 {
			return &[];
		}
		unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
	}
}

#[repr(C)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwHeaderIterator {
	/// Iterator state owned by the producer.
	pub state: *mut c_void,
	/// Writes a borrowed header view into the second argument and returns
	/// `AGW_STATUS_OK`; a header with an empty name means the iterator is
	/// exhausted. The view is valid until the next `next` call or iterator drop
	/// unless the producer documents more. `next: None` is an empty iterator.
	pub next: AgwHeaderIteratorNext,
	/// Releases `state`. `drop: None` means there is nothing to free.
	pub drop: AgwDropOwned,
}

impl Default for AgwHeaderIterator {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			next: None,
			drop: None,
		}
	}
}

impl AgwHeaderIterator {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this iterator, and the iterator must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		unsafe { free_owned(&mut self.state, &mut self.drop) };
		self.next = None;
	}
}

/// Opaque host-owned context for one request or response phase.
///
/// The host passes it to `apply` (or a response session's `apply`). It stays
/// valid until the returned future completes or is dropped, and is passed back
/// as the first argument to request/response hostcalls. It may be used from any
/// thread while valid, but never from two threads at once.
///
/// Only ever handled by pointer. It is not `repr(C)`, so the C header declares
/// it as an incomplete type.
pub struct AgwContext {
	_private: [u8; 0],
	// Not Send, Sync or Unpin: the host owns it and decides how it is used.
	_marker: std::marker::PhantomData<(*mut u8, std::marker::PhantomPinned)>,
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwBodyChunk {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	/// One of `AGW_BODY_FRAME_*`.
	pub kind: u32,
	/// DATA frame payload.
	pub bytes: AgwBytes,
	/// TRAILERS frame headers.
	pub trailers: AgwHeaders,
	/// Error set by a body producer when polling failed. This is not an HTTP frame.
	pub error: AgwError,
	/// Opaque owner for the borrowed fields in this chunk.
	pub private_data: *mut c_void,
	pub drop: AgwDropOwned,
}

impl Default for AgwBodyChunk {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			kind: AGW_BODY_FRAME_END,
			bytes: AgwBytes::empty(),
			trailers: AgwHeaders::default(),
			error: AgwError::default(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

unsafe impl AgwSized for AgwBodyChunk {
	const MIN_SIZE: usize = end_of!(AgwBodyChunk, drop: AgwDropOwned);
}

impl AgwBodyChunk {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this chunk, and the chunk must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		// Nested owned handles first: they may depend on the outer owner.
		unsafe { self.error.free() };
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
		self.kind = AGW_BODY_FRAME_END;
		self.bytes = AgwBytes::empty();
		self.trailers = AgwHeaders::default();
	}
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct AgwDirectResponse {
	/// Nonzero if the direct response is present.
	pub present: u8,
	pub status: u16,
	pub headers: AgwHeaders,
	pub body_stream: AgwBodyStream,
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwApplyResult {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	/// One of `AGW_APPLY_*`.
	pub status: u32,
	pub direct_response: AgwDirectResponse,
	/// Optional response phase state returned by request apply.
	pub response_session: AgwPluginResponseSession,
	pub error: AgwError,
	/// Opaque owner for any borrowed result fields. Everything the result
	/// borrows (header and error strings, ...) must be owned through this, not by
	/// the future: the host may drop the future before it reads the result.
	pub private_data: *mut c_void,
	pub drop: AgwDropOwned,
}

impl Default for AgwApplyResult {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			status: AGW_APPLY_CONTINUE,
			direct_response: AgwDirectResponse::default(),
			response_session: AgwPluginResponseSession::default(),
			error: AgwError::default(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

unsafe impl AgwSized for AgwApplyResult {
	const MIN_SIZE: usize = end_of!(AgwApplyResult, drop: AgwDropOwned);
}

impl AgwApplyResult {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this result, and the result must not be dropped more than once.
	/// Releases the nested handles (response session, direct-response body,
	/// error) and then the outer owner. Handles already taken out are skipped.
	pub unsafe fn free(&mut self) {
		let session = mem::take(&mut self.response_session);
		if let Some(drop) = session.drop
			&& !session.state.is_null()
		{
			unsafe { drop(session.state) };
		}
		unsafe { self.direct_response.body_stream.free() };
		unsafe { self.error.free() };
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
	}
}

#[repr(C)]
pub struct AgwRawWaker {
	/// Opaque task/waker state.
	pub data: *const c_void,
	/// Waker operations for `data`. The pair is a C ABI equivalent of
	/// `std::task::RawWaker`.
	pub vtable: *const AgwRawWakerVTable,
}

impl AgwRawWaker {
	pub const fn new(data: *const c_void, vtable: *const AgwRawWakerVTable) -> Self {
		Self { data, vtable }
	}
}

#[repr(C)]
pub struct AgwRawWakerVTable {
	/// Duplicates the waker state for another owner.
	pub clone: AgwRawWakerClone,
	/// Consumes one owned waker reference and wakes the task.
	pub wake: AgwRawWakerWake,
	/// Wakes without consuming the waker reference.
	pub wake_by_ref: AgwRawWakerWake,
	/// Releases one owned waker reference.
	pub drop: AgwRawWakerWake,
}

unsafe extern "C" fn noop_waker_drop(_data: *const c_void) {}

// A `Waker`'s methods are arbitrary code and may panic; a panic must not unwind
// out of these `extern "C"` callbacks, which would abort the process.

unsafe extern "C" fn clone_std_waker(data: *const c_void) -> AgwRawWaker {
	// If cloning panics, hand back a waker that does nothing. The task may then
	// miss a wakeup, which is better than aborting the process.
	catch_panic(NOOP_AGW_WAKER, || {
		let waker = unsafe { &*(data.cast::<Waker>()) };
		raw_waker_from_waker(waker)
	})
}

unsafe extern "C" fn wake_std_waker(data: *const c_void) {
	catch_panic((), || {
		let waker = unsafe { Box::from_raw(data.cast_mut().cast::<Waker>()) };
		waker.wake();
	})
}

unsafe extern "C" fn wake_std_waker_by_ref(data: *const c_void) {
	catch_panic((), || {
		let waker = unsafe { &*(data.cast::<Waker>()) };
		waker.wake_by_ref();
	})
}

unsafe extern "C" fn drop_std_waker(data: *const c_void) {
	catch_panic((), || unsafe {
		drop(Box::from_raw(data.cast_mut().cast::<Waker>()));
	})
}

unsafe extern "C" fn noop_waker_clone(_data: *const c_void) -> AgwRawWaker {
	NOOP_AGW_WAKER
}

static NOOP_AGW_WAKER_VTABLE: AgwRawWakerVTable = AgwRawWakerVTable {
	clone: Some(noop_waker_clone),
	wake: Some(noop_waker_drop),
	wake_by_ref: Some(noop_waker_drop),
	drop: Some(noop_waker_drop),
};

const NOOP_AGW_WAKER: AgwRawWaker = AgwRawWaker {
	data: std::ptr::null(),
	vtable: &NOOP_AGW_WAKER_VTABLE,
};

static STD_WAKER_TO_AGW_VTABLE: AgwRawWakerVTable = AgwRawWakerVTable {
	clone: Some(clone_std_waker),
	wake: Some(wake_std_waker),
	wake_by_ref: Some(wake_std_waker_by_ref),
	drop: Some(drop_std_waker),
};

// A borrowed waker does not own `data`, so consuming wake and drop must not free it.
static BORROWED_STD_WAKER_TO_AGW_VTABLE: AgwRawWakerVTable = AgwRawWakerVTable {
	clone: Some(clone_std_waker),
	wake: Some(wake_std_waker_by_ref),
	wake_by_ref: Some(wake_std_waker_by_ref),
	drop: Some(noop_waker_drop),
};

/// Calls `f` with a borrowed `AgwRawWaker` for `waker`, without allocating. This
/// is what poll callers pass to a poll function: the callee may clone it to keep
/// it, but must not retain the pointer past the call.
pub fn with_borrowed_waker<R>(waker: &Waker, f: impl FnOnce(*const AgwRawWaker) -> R) -> R {
	let raw = AgwRawWaker {
		data: (waker as *const Waker).cast::<c_void>(),
		vtable: &BORROWED_STD_WAKER_TO_AGW_VTABLE,
	};
	f(&raw)
}

fn raw_waker_from_waker(waker: &Waker) -> AgwRawWaker {
	let boxed = Box::new(waker.clone());
	AgwRawWaker {
		data: Box::into_raw(boxed).cast::<c_void>(),
		vtable: &STD_WAKER_TO_AGW_VTABLE,
	}
}

// A waker from the other side may leave any callback unset, or even the vtable
// null (e.g. a C plugin's do-nothing waker). Missing callbacks are no-ops rather
// than panics: these run inside the executor's waker handling, where a panic in
// a `Drop` during unwinding aborts the process.
unsafe fn agw_waker_vtable<'a>(raw: &AgwRawWaker) -> Option<&'a AgwRawWakerVTable> {
	unsafe { raw.vtable.as_ref() }
}

unsafe fn clone_agw_waker(data: *const ()) -> RawWaker {
	let raw = unsafe { &*(data.cast::<AgwRawWaker>()) };
	let cloned = match unsafe { agw_waker_vtable(raw) }.and_then(|vtable| vtable.clone) {
		Some(clone) => unsafe { clone(raw.data) },
		None => NOOP_AGW_WAKER,
	};
	RawWaker::new(
		Box::into_raw(Box::new(cloned)).cast::<()>(),
		&AGW_TO_STD_WAKER_VTABLE,
	)
}

unsafe fn wake_agw_waker(data: *const ()) {
	let raw = unsafe { Box::from_raw(data.cast_mut().cast::<AgwRawWaker>()) };
	let Some(vtable) = (unsafe { agw_waker_vtable(&raw) }) else {
		return;
	};
	match vtable.wake {
		Some(wake) => unsafe { wake(raw.data) },
		// Without a consuming wake, wake by reference and then release the waker.
		None => {
			if let Some(wake_by_ref) = vtable.wake_by_ref {
				unsafe { wake_by_ref(raw.data) };
			}
			if let Some(drop) = vtable.drop {
				unsafe { drop(raw.data) };
			}
		},
	}
}

unsafe fn wake_agw_waker_by_ref(data: *const ()) {
	let raw = unsafe { &*(data.cast::<AgwRawWaker>()) };
	if let Some(wake_by_ref) = unsafe { agw_waker_vtable(raw) }.and_then(|vtable| vtable.wake_by_ref)
	{
		unsafe { wake_by_ref(raw.data) };
	}
}

unsafe fn drop_agw_waker(data: *const ()) {
	let raw = unsafe { Box::from_raw(data.cast_mut().cast::<AgwRawWaker>()) };
	if let Some(drop) = unsafe { agw_waker_vtable(&raw) }.and_then(|vtable| vtable.drop) {
		unsafe { drop(raw.data) };
	}
}

static AGW_TO_STD_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
	clone_agw_waker,
	wake_agw_waker,
	wake_agw_waker_by_ref,
	drop_agw_waker,
);

unsafe fn drop_borrowed_agw_waker(_data: *const ()) {}

// `data` points at a caller-owned `AgwRawWaker`; clones become owned wakers.
static BORROWED_AGW_TO_STD_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
	clone_agw_waker,
	wake_agw_waker_by_ref,
	wake_agw_waker_by_ref,
	drop_borrowed_agw_waker,
);

/// Calls `f` with a `Context` backed by the borrowed waker passed to a poll
/// function. Futures that need the waker later clone it, which produces an owned
/// waker through the AGW vtable's `clone`.
///
/// # Safety
///
/// `raw` must be null or point to an `AgwRawWaker` that stays valid for the
/// duration of `f`, with a vtable whose operations follow the
/// `std::task::RawWaker` contract. A null `raw` uses a no-op waker.
pub unsafe fn with_context<R>(raw: *const AgwRawWaker, f: impl FnOnce(&mut Context<'_>) -> R) -> R {
	if raw.is_null() {
		return f(&mut Context::from_waker(Waker::noop()));
	}
	let waker = unsafe {
		Waker::from_raw(RawWaker::new(
			raw.cast::<()>(),
			&BORROWED_AGW_TO_STD_WAKER_VTABLE,
		))
	};
	f(&mut Context::from_waker(&waker))
}

#[repr(C)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwApplyFuture {
	/// Opaque future state owned by the future producer.
	pub state: *mut c_void,
	pub poll: AgwApplyPoll,
	pub drop: AgwDropOwned,
}

impl Default for AgwApplyFuture {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			poll: None,
			drop: None,
		}
	}
}

#[repr(C)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwUnitFuture {
	/// Opaque future state owned by the future producer.
	pub state: *mut c_void,
	pub poll: AgwUnitPoll,
	pub drop: AgwDropOwned,
}

impl Default for AgwUnitFuture {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			poll: None,
			drop: None,
		}
	}
}

#[repr(C)]
#[derive(Debug)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwBodyStream {
	/// Opaque stream state owned by the stream producer.
	pub state: *mut c_void,
	/// Produces one `AgwBodyChunk` per ready poll.
	pub poll_next: AgwBodyStreamPoll,
	pub drop: AgwDropOwned,
}

impl Default for AgwBodyStream {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			poll_next: None,
			drop: None,
		}
	}
}

impl AgwBodyStream {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this stream, and the stream must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		unsafe { free_owned(&mut self.state, &mut self.drop) };
		self.poll_next = None;
	}
}

#[repr(C)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwHttpFuture {
	/// Opaque future state owned by the future producer.
	pub state: *mut c_void,
	pub poll: AgwHttpPoll,
	pub drop: AgwDropOwned,
}

impl Default for AgwHttpFuture {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			poll: None,
			drop: None,
		}
	}
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwHttpRequest {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	/// Reserved for host backend references; must be empty. Requests are sent
	/// to the absolute `http`/`https` URL in `uri`.
	pub backend: AgwString,
	pub method: AgwString,
	pub uri: AgwString,
	pub headers: AgwHeaders,
	/// Request body stream. Empty stream means an empty body. `http_call`
	/// always takes ownership of it, including when the call fails.
	pub body_stream: AgwBodyStream,
}

impl Default for AgwHttpRequest {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			backend: AgwString::empty(),
			method: AgwString::empty(),
			uri: AgwString::empty(),
			headers: AgwHeaders::default(),
			body_stream: AgwBodyStream::default(),
		}
	}
}

unsafe impl AgwSized for AgwHttpRequest {
	const MIN_SIZE: usize = end_of!(AgwHttpRequest, body_stream: AgwBodyStream);
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwHttpResponse {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	pub status: u16,
	pub headers: AgwHeaders,
	pub body_stream: AgwBodyStream,
	/// Error is set when the host call failed before producing a response.
	pub error: AgwError,
	/// Opaque owner for response fields.
	pub private_data: *mut c_void,
	pub drop: AgwDropOwned,
}

impl Default for AgwHttpResponse {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			status: 0,
			headers: AgwHeaders::default(),
			body_stream: AgwBodyStream::default(),
			error: AgwError::default(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

unsafe impl AgwSized for AgwHttpResponse {
	const MIN_SIZE: usize = end_of!(AgwHttpResponse, drop: AgwDropOwned);
}

impl AgwHttpResponse {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this response, and the response must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		// Nested owned handles first: they may depend on the outer owner.
		unsafe { self.body_stream.free() };
		unsafe { self.error.free() };
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
	}
}

#[repr(C)]
#[derive(Debug)]
pub struct AgwCelExpression {
	/// Host-owned compiled CEL expression.
	pub state: *mut c_void,
	pub drop: AgwDropOwned,
}

impl Default for AgwCelExpression {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			drop: None,
		}
	}
}

impl AgwCelExpression {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the same plugin library that
	/// created this value, and the value must not be dropped more than once.
	pub unsafe fn free(&mut self) {
		unsafe { free_owned(&mut self.state, &mut self.drop) };
	}
}

/// No value. A default-initialized output has this kind; `get_property` leaves
/// it for a property that does not exist. Distinct from `AGW_CEL_NULL`.
pub const AGW_CEL_UNSET: u32 = 0;
/// CEL `null`. No payload.
pub const AGW_CEL_NULL: u32 = 1;
/// `value.int_value`, 0 or 1.
pub const AGW_CEL_BOOL: u32 = 2;
/// `value.int_value`.
pub const AGW_CEL_INT: u32 = 3;
/// `value.uint_value`.
pub const AGW_CEL_UINT: u32 = 4;
/// `value.double_value`.
pub const AGW_CEL_DOUBLE: u32 = 5;
/// `value.bytes`, valid UTF-8.
pub const AGW_CEL_STRING: u32 = 6;
/// `value.bytes`.
pub const AGW_CEL_BYTES: u32 = 7;
/// `value.items`: `len` values.
pub const AGW_CEL_LIST: u32 = 8;
/// `value.items`: `len` entries stored as `2 * len` values, alternating key and
/// value. Keys are BOOL, INT, UINT or STRING.
pub const AGW_CEL_MAP: u32 = 9;
/// `value.time`: `seconds` since the Unix epoch (UTC) plus `nanos` (0..1e9), and
/// the timestamp's UTC offset in `offset_seconds`.
pub const AGW_CEL_TIMESTAMP: u32 = 10;
/// `value.time`: `seconds` plus `nanos` (-1e9..1e9), both carrying the
/// duration's sign, so `-0.5s` is `seconds = 0, nanos = -500_000_000`.
/// `offset_seconds` is zero.
pub const AGW_CEL_DURATION: u32 = 11;
/// A CEL type, such as the result of `type(1)`: `value.bytes` holds its name
/// (`int`), valid UTF-8. Distinct from the string `"int"`.
pub const AGW_CEL_TYPE: u32 = 12;

/// Payload of a TIMESTAMP or DURATION.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgwCelTime {
	pub seconds: i64,
	pub nanos: i32,
	/// TIMESTAMP: the UTC offset it was expressed in, in seconds east of UTC
	/// (`seconds` is always UTC). DURATION: zero.
	pub offset_seconds: i32,
}

/// Payload of a LIST or MAP.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AgwCelItems {
	pub ptr: *const AgwCelValue,
	/// LIST: number of values. MAP: number of entries (`ptr` holds twice as many).
	pub len: usize,
}

/// The payload of an `AgwCelValue`; its `kind` says which member is set.
#[repr(C)]
#[derive(Clone, Copy)]
pub union AgwCelPayload {
	pub int_value: i64,
	pub uint_value: u64,
	pub double_value: f64,
	pub bytes: AgwBytes,
	pub items: AgwCelItems,
	pub time: AgwCelTime,
}

/// A CEL value: a `kind` and the payload member it selects.
///
/// This struct is stored in arrays (`AgwCelItems`), so its layout is frozen: it
/// is not `AgwSized` and can never grow. New kinds must fit the payload (or
/// point to a separate struct), and a receiver rejects kinds or flags it does
/// not know. Every pointer is a borrowed view owned by whatever owns the
/// outermost value (for example an `AgwCelResult`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AgwCelValue {
	/// One of `AGW_CEL_*`.
	pub kind: u32,
	/// No flags are defined; producers set zero.
	pub flags: u32,
	pub value: AgwCelPayload,
}

impl Default for AgwCelValue {
	fn default() -> Self {
		Self {
			kind: AGW_CEL_UNSET,
			flags: 0,
			value: AgwCelPayload {
				items: AgwCelItems {
					ptr: std::ptr::null(),
					len: 0,
				},
			},
		}
	}
}

impl std::fmt::Debug for AgwCelValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AgwCelValue")
			.field("kind", &self.kind)
			.field("flags", &self.flags)
			.finish_non_exhaustive()
	}
}

impl AgwCelValue {
	pub fn new(kind: u32, value: AgwCelPayload) -> Self {
		Self {
			kind,
			flags: 0,
			value,
		}
	}

	/// The values of a LIST, or the alternating keys and values of a MAP. Empty
	/// for any other kind.
	///
	/// # Safety
	///
	/// `value.items` must point to as many values as `kind` and `len` describe.
	pub unsafe fn items<'a>(&self) -> &'a [AgwCelValue] {
		let items = match self.kind {
			AGW_CEL_LIST | AGW_CEL_MAP => unsafe { self.value.items },
			_ => return &[],
		};
		let len = match self.kind {
			AGW_CEL_MAP => items.len.saturating_mul(2),
			_ => items.len,
		};
		if len == 0 || items.ptr.is_null() {
			return &[];
		}
		unsafe { std::slice::from_raw_parts(items.ptr, len) }
	}
}

/// A CEL value returned to the caller, such as the output of `eval_cel` and
/// `get_property`. `value` and everything it points to are owned through
/// `private_data`.
#[repr(C)]
#[derive(Debug)]
pub struct AgwCelResult {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	pub value: AgwCelValue,
	/// Opaque owner for `value`'s borrowed fields.
	pub private_data: *mut c_void,
	/// Releases `private_data`.
	pub drop: AgwDropOwned,
}

impl Default for AgwCelResult {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			value: AgwCelValue::default(),
			private_data: std::ptr::null_mut(),
			drop: None,
		}
	}
}

unsafe impl AgwSized for AgwCelResult {
	const MIN_SIZE: usize = end_of!(AgwCelResult, drop: AgwDropOwned);
}

impl AgwCelResult {
	/// # Safety
	///
	/// The owner pointer and drop function must come from the side that wrote
	/// this result, and the result must not be freed more than once.
	pub unsafe fn free(&mut self) {
		self.value = AgwCelValue::default();
		unsafe { free_owned(&mut self.private_data, &mut self.drop) };
	}
}

pub const AGW_LOG_TRACE: u32 = 0;
pub const AGW_LOG_DEBUG: u32 = 1;
pub const AGW_LOG_INFO: u32 = 2;
pub const AGW_LOG_WARN: u32 = 3;
pub const AGW_LOG_ERROR: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AgwHostVTable {
	/// `AgwHostVTable` is append-only. New callbacks must be added after the
	/// existing fields so older plugins can use `size` to detect missing entries.
	/// Byte size of this struct as populated by the host. Plugins should check
	/// this before reading fields added in newer ABI versions.
	pub size: usize,
	/// Host data pointer passed as the first argument to host callbacks that do
	/// not act on a request or response (`sleep`, `compile_cel`, `log`). Those
	/// may be called at any time. All other callbacks take an `AgwContext`.
	pub data: *const c_void,
	/// Fetch the first value of a header, or a pseudo-header, from the
	/// request/response context. Use `headers` to see every value of a
	/// repeated header.
	pub get_header: AgwHostGetHeader,
	/// Iterate materialized headers from the request/response context.
	pub headers: AgwHostHeaders,
	/// Take a stream over the context's body.
	pub body_stream: AgwHostBodyStream,
	/// Appends a value to a header, keeping any existing values. With `a: 1`
	/// already present, adding `a: 2` sends both `a: 1` and `a: 2`.
	/// Pseudo-headers cannot be added.
	pub add_header: AgwHostSetHeader,
	/// Replaces every existing value of a header with a single value. With
	/// `a: 1` and `a: 2` present, setting `a: 3` leaves only `a: 3`. This also
	/// sets pseudo-headers: `:method`, `:scheme`, `:authority` and `:path` on
	/// requests, `:status` on responses.
	pub set_header: AgwHostSetHeader,
	/// Removes every value of a header. Removing an absent header succeeds.
	/// Pseudo-headers cannot be removed.
	pub remove_header: AgwHostRemoveHeader,
	/// Replace the context's request/response body with streaming data. The
	/// host always takes ownership of the stream, including when the call
	/// fails; the caller must not free it afterwards.
	pub set_body_stream: AgwHostSetBodyStream,
	/// Host-provided async sleep so plugins do not need their own runtime.
	pub sleep: AgwHostSleep,
	/// CEL helpers. Expressions and properties resolve against the same root
	/// CEL context agentgateway policy expressions see (`request`, `jwt`, and
	/// so on), plus plugin metadata under `<name>.*`.
	pub compile_cel: AgwHostCompileCel,
	/// Evaluates a compiled expression and writes the result as an
	/// `AgwCelResult`.
	pub eval_cel: AgwHostEvalCel,
	/// Fast path for reading one property without compiling an expression.
	/// The path is passed as segments: `["jwt", "sub"]` reads the same value
	/// as the CEL expression `jwt.sub`, and `["example", "decision"]` reads
	/// metadata the `example` plugin set under the key `decision`. The value is
	/// written as an `AgwCelResult`; a missing property returns `AGW_STATUS_OK`
	/// and leaves the result's value `AGW_CEL_UNSET`.
	pub get_property: AgwHostGetProperty,
	/// Sets this plugin's metadata `key` to a CEL value, readable from CEL as
	/// `<name>.<key>` and from `get_property`. The value is borrowed for the call
	/// and copied; it may not be UNSET. Types are kept: a `uint` reads back as a
	/// `uint`.
	pub set_metadata: AgwHostSetMetadata,
	/// Host HTTP callout. `ctx` may be null to make a call outside any request
	/// (e.g. from a background thread); such calls are not attributed to a
	/// request and can be polled from any thread.
	pub http_call: AgwHostHttpCall,
	/// Host logging callback.
	pub log: AgwHostLog,
	/// Reports whether `log` at the given level would be recorded, so plugins
	/// can skip formatting disabled messages. Absent means "assume enabled".
	pub log_enabled: AgwHostLogEnabled,
}

impl Default for AgwHostVTable {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			data: std::ptr::null(),
			get_header: None,
			headers: None,
			body_stream: None,
			add_header: None,
			set_header: None,
			remove_header: None,
			set_body_stream: None,
			sleep: None,
			compile_cel: None,
			eval_cel: None,
			get_property: None,
			set_metadata: None,
			http_call: None,
			log: None,
			log_enabled: None,
		}
	}
}

unsafe impl AgwSized for AgwHostVTable {
	const MIN_SIZE: usize = end_of!(AgwHostVTable, log: AgwHostLog);
}

#[repr(C)]
#[derive(Debug)]
#[must_use = "an owned handle must be transferred or freed"]
pub struct AgwPluginResponseSession {
	/// Plugin-owned response phase state returned from request apply.
	pub state: *mut c_void,
	pub apply: AgwPluginResponseApply,
	pub drop: AgwPluginDrop,
}

impl Default for AgwPluginResponseSession {
	fn default() -> Self {
		Self {
			state: std::ptr::null_mut(),
			apply: None,
			drop: None,
		}
	}
}

#[repr(C)]
pub struct AgwPluginInstance {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	/// Plugin-owned instance state.
	pub state: *mut c_void,
	/// Request apply entrypoint for this instance.
	pub apply: AgwPluginApply,
	pub drop: AgwPluginDrop,
}

impl Default for AgwPluginInstance {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			state: std::ptr::null_mut(),
			apply: None,
			drop: None,
		}
	}
}

unsafe impl AgwSized for AgwPluginInstance {
	const MIN_SIZE: usize = end_of!(AgwPluginInstance, drop: AgwPluginDrop);
}

/// Returned by pointer from the plugin entrypoint. It must point to static data
/// that stays valid while the library is loaded.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AgwPluginDescriptor {
	/// Bytes populated by the producer. As an output, the caller first sets
	/// this to the size it allocated. See `AgwSized`.
	pub size: usize,
	/// ABI version implemented by this plugin.
	pub abi_version: u32,
	/// Static plugin name and version for logging and metadata scoping.
	pub name: AgwString,
	pub version: AgwString,
	/// Creates an instance from JSON config and the host vtable. The vtable
	/// pointer stays valid for the life of the process, so the plugin may keep it.
	/// May be null for a library that provides no HTTP policy (for example, only
	/// capabilities added in later ABI versions); the host rejects a null
	/// `create` only when the library is configured as a policy.
	pub create: AgwPluginCreate,
}

impl Default for AgwPluginDescriptor {
	fn default() -> Self {
		Self {
			size: mem::size_of::<Self>(),
			abi_version: 0,
			name: AgwString::empty(),
			version: AgwString::empty(),
			create: None,
		}
	}
}

unsafe impl AgwSized for AgwPluginDescriptor {
	const MIN_SIZE: usize = end_of!(AgwPluginDescriptor, create: AgwPluginCreate);
}

// SAFETY: a descriptor is immutable static data: static strings and a function pointer.
unsafe impl Sync for AgwPluginDescriptor {}

/// Signature of the `agentgateway_plugin_entrypoint` symbol.
pub type AgwPluginEntrypoint = unsafe extern "C" fn() -> *const AgwPluginDescriptor;

pub type AgwHeaderIteratorNext = Option<unsafe extern "C" fn(*mut c_void, *mut AgwHeader) -> u32>;

pub type AgwRawWakerClone = Option<unsafe extern "C" fn(*const c_void) -> AgwRawWaker>;
pub type AgwRawWakerWake = Option<unsafe extern "C" fn(*const c_void)>;

// Poll functions receive a borrowed waker, valid only for the call. To be woken
// later, clone it through its vtable.
pub type AgwApplyPoll =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwRawWaker, *mut AgwApplyResult) -> u32>;
pub type AgwUnitPoll =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwRawWaker, *mut AgwError) -> u32>;
pub type AgwBodyStreamPoll =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwRawWaker, *mut AgwBodyChunk) -> u32>;
pub type AgwHttpPoll =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwRawWaker, *mut AgwHttpResponse) -> u32>;

pub type AgwHostGetHeader =
	Option<unsafe extern "C" fn(*const AgwContext, AgwString, *mut AgwOwnedBytes) -> u32>;
pub type AgwHostHeaders = Option<unsafe extern "C" fn(*const AgwContext) -> AgwHeaderIterator>;
pub type AgwHostBodyStream = Option<unsafe extern "C" fn(*const AgwContext) -> AgwBodyStream>;
pub type AgwHostSetHeader =
	Option<unsafe extern "C" fn(*const AgwContext, AgwString, AgwBytes) -> u32>;
pub type AgwHostRemoveHeader = Option<unsafe extern "C" fn(*const AgwContext, AgwString) -> u32>;
pub type AgwHostSetBodyStream =
	Option<unsafe extern "C" fn(*const AgwContext, AgwBodyStream) -> u32>;
pub type AgwHostSleep = Option<unsafe extern "C" fn(*const c_void, u64) -> AgwUnitFuture>;
pub type AgwHostCompileCel = Option<
	unsafe extern "C" fn(*const c_void, AgwString, *mut AgwCelExpression, *mut AgwError) -> u32,
>;
pub type AgwHostEvalCel = Option<
	unsafe extern "C" fn(
		*const AgwContext,
		*const AgwCelExpression,
		*mut AgwCelResult,
		*mut AgwError,
	) -> u32,
>;
pub type AgwHostGetProperty = Option<
	unsafe extern "C" fn(*const AgwContext, AgwStringSlice, *mut AgwCelResult, *mut AgwError) -> u32,
>;
pub type AgwHostSetMetadata = Option<
	unsafe extern "C" fn(*const AgwContext, AgwString, *const AgwCelValue, *mut AgwError) -> u32,
>;
pub type AgwHostHttpCall =
	Option<unsafe extern "C" fn(*const AgwContext, *const AgwHttpRequest) -> AgwHttpFuture>;
pub type AgwHostLog = Option<unsafe extern "C" fn(*const c_void, u32, AgwString)>;
pub type AgwHostLogEnabled = Option<unsafe extern "C" fn(*const c_void, u32) -> bool>;

pub type AgwPluginApply =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwContext) -> AgwApplyFuture>;
pub type AgwPluginResponseApply =
	Option<unsafe extern "C" fn(*mut c_void, *const AgwContext) -> AgwApplyFuture>;
pub type AgwPluginDrop = Option<unsafe extern "C" fn(*mut c_void)>;
pub type AgwPluginCreate = Option<
	unsafe extern "C" fn(
		AgwBytes,
		*const AgwHostVTable,
		*mut AgwPluginInstance,
		*mut AgwError,
	) -> u32,
>;

#[cfg(test)]
mod tests {
	use super::*;

	// An instance struct as a future ABI version might extend it.
	#[repr(C)]
	struct NewInstance {
		v1: AgwPluginInstance,
		extra: usize,
	}

	impl Default for NewInstance {
		fn default() -> Self {
			Self {
				v1: AgwPluginInstance {
					size: mem::size_of::<Self>(),
					..Default::default()
				},
				extra: 0,
			}
		}
	}

	unsafe impl AgwSized for NewInstance {
		const MIN_SIZE: usize = AgwPluginInstance::MIN_SIZE;
	}

	unsafe extern "C" fn test_drop(_: *mut c_void) {}

	#[test]
	fn newer_producer_writes_only_the_older_consumers_prefix() {
		let mut out = AgwPluginInstance::default();
		let value = NewInstance {
			v1: AgwPluginInstance {
				state: 0x1234usize as *mut c_void,
				drop: Some(test_drop),
				..Default::default()
			},
			extra: 7,
		};
		let out_ptr = (&mut out as *mut AgwPluginInstance).cast::<NewInstance>();
		assert!(unsafe { write_sized(out_ptr, value) }.is_ok());
		assert_eq!(out.size, mem::size_of::<AgwPluginInstance>());
		assert_eq!(out.state, 0x1234usize as *mut c_void);
		assert!(out.drop.is_some());
	}

	#[test]
	fn older_producer_leaves_newer_fields_at_defaults() {
		let mut out = NewInstance {
			extra: 7,
			..Default::default()
		};
		let value = AgwPluginInstance {
			state: 0x1234usize as *mut c_void,
			..Default::default()
		};
		let out_ptr = (&mut out as *mut NewInstance).cast::<AgwPluginInstance>();
		assert!(unsafe { write_sized(out_ptr, value) }.is_ok());
		assert_eq!(out.v1.size, mem::size_of::<AgwPluginInstance>());
		assert_eq!(out.v1.state, 0x1234usize as *mut c_void);
		assert_eq!(
			out.extra, 7,
			"fields past the producer's size are untouched"
		);
	}

	#[test]
	fn write_sized_rejects_null_and_too_small_outputs() {
		assert!(unsafe { write_sized(std::ptr::null_mut(), AgwApplyResult::default()) }.is_err());
		let mut out = AgwApplyResult {
			size: AgwApplyResult::MIN_SIZE - 1,
			..Default::default()
		};
		assert!(unsafe { write_sized(&mut out, AgwApplyResult::default()) }.is_err());
	}

	#[test]
	fn read_sized_accepts_v1_and_rejects_short_prefixes() {
		let vtable = AgwHostVTable {
			data: 0x1234usize as *const c_void,
			..Default::default()
		};
		let copied = unsafe { read_sized(&vtable) }.unwrap();
		assert_eq!(copied.data, vtable.data);

		#[repr(C)]
		struct ShortHostVTable {
			size: usize,
			data: *const c_void,
		}
		let short = ShortHostVTable {
			size: mem::size_of::<ShortHostVTable>(),
			data: std::ptr::null(),
		};
		let copied = unsafe { read_sized((&short as *const ShortHostVTable).cast::<AgwHostVTable>()) };
		assert!(copied.is_none());
	}

	struct PanickingWaker;

	impl std::task::Wake for PanickingWaker {
		fn wake(self: std::sync::Arc<Self>) {
			panic!("waker bug");
		}
	}

	// Regression: a panicking `Waker` unwound out of the `extern "C"` waker
	// callbacks, which aborts the process.
	#[test]
	fn panicking_waker_does_not_abort() {
		let waker = Waker::from(std::sync::Arc::new(PanickingWaker));
		with_borrowed_waker(&waker, |raw| unsafe {
			let vtable = &*(*raw).vtable;
			(vtable.wake_by_ref.unwrap())((*raw).data);
			let owned = (vtable.clone.unwrap())((*raw).data);
			let owned_vtable = &*owned.vtable;
			(owned_vtable.wake.unwrap())(owned.data);
		});
	}

	static FREE_ORDER: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

	unsafe extern "C" fn record_outer_drop(_: *mut c_void) {
		FREE_ORDER.lock().unwrap().push("outer");
	}

	unsafe extern "C" fn record_nested_drop(_: *mut c_void) {
		FREE_ORDER.lock().unwrap().push("nested");
	}

	fn dangling() -> *mut c_void {
		std::ptr::NonNull::dangling().as_ptr()
	}

	// Regression: `free` released the outer owner before nested handles, which may
	// depend on it.
	#[test]
	fn free_releases_nested_handles_before_the_outer_owner() {
		let nested_error = AgwError {
			code: AGW_ERROR_UNKNOWN,
			message: AgwString::empty(),
			private_data: dangling(),
			drop: Some(record_nested_drop),
		};
		FREE_ORDER.lock().unwrap().clear();
		let mut response = AgwHttpResponse {
			body_stream: AgwBodyStream {
				state: dangling(),
				poll_next: None,
				drop: Some(record_nested_drop),
			},
			error: nested_error,
			private_data: dangling(),
			drop: Some(record_outer_drop),
			..Default::default()
		};
		unsafe { response.free() };
		assert_eq!(*FREE_ORDER.lock().unwrap(), ["nested", "nested", "outer"]);

		FREE_ORDER.lock().unwrap().clear();
		let mut chunk = AgwBodyChunk {
			error: AgwError {
				private_data: dangling(),
				drop: Some(record_nested_drop),
				..Default::default()
			},
			private_data: dangling(),
			drop: Some(record_outer_drop),
			..Default::default()
		};
		unsafe { chunk.free() };
		assert_eq!(*FREE_ORDER.lock().unwrap(), ["nested", "outer"]);
	}

	// Regression: `AgwApplyResult::free` released only the outer owner, leaking
	// the response session, direct-response body and error.
	#[test]
	fn apply_result_free_releases_nested_handles_first() {
		FREE_ORDER.lock().unwrap().clear();
		let mut result = AgwApplyResult {
			response_session: AgwPluginResponseSession {
				state: dangling(),
				drop: Some(record_nested_drop),
				..Default::default()
			},
			direct_response: AgwDirectResponse {
				body_stream: AgwBodyStream {
					state: dangling(),
					poll_next: None,
					drop: Some(record_nested_drop),
				},
				..Default::default()
			},
			error: AgwError {
				private_data: dangling(),
				drop: Some(record_nested_drop),
				..Default::default()
			},
			private_data: dangling(),
			drop: Some(record_outer_drop),
			..Default::default()
		};
		unsafe { result.free() };
		assert_eq!(
			*FREE_ORDER.lock().unwrap(),
			["nested", "nested", "nested", "outer"]
		);
	}

	// Regression: a waker vtable with unset callbacks panicked inside the std
	// `Waker` methods, which can abort the process when it happens in a drop.
	#[test]
	fn agw_waker_with_missing_callbacks_is_a_no_op() {
		static EMPTY: AgwRawWakerVTable = AgwRawWakerVTable {
			clone: None,
			wake: None,
			wake_by_ref: None,
			drop: None,
		};
		// Clones work, but the clone has no wake or drop callbacks.
		unsafe extern "C" fn clone_only(_: *const c_void) -> AgwRawWaker {
			AgwRawWaker {
				data: std::ptr::null(),
				vtable: &CLONE_ONLY,
			}
		}
		static CLONE_ONLY: AgwRawWakerVTable = AgwRawWakerVTable {
			clone: Some(clone_only),
			wake: None,
			wake_by_ref: None,
			drop: None,
		};
		for raw in [
			AgwRawWaker {
				data: std::ptr::null(),
				vtable: &EMPTY,
			},
			AgwRawWaker {
				data: std::ptr::null(),
				vtable: &CLONE_ONLY,
			},
			AgwRawWaker {
				data: std::ptr::null(),
				vtable: std::ptr::null(),
			},
		] {
			let result = std::panic::catch_unwind(|| unsafe {
				with_context(&raw, |cx| {
					let waker = cx.waker().clone();
					waker.wake_by_ref();
					// A separate clone, to exercise the consuming `wake`.
					let consumed = waker.clone();
					consumed.wake();
					drop(waker);
				})
			});
			assert!(result.is_ok());
		}
	}

	// `AgwCelValue` is stored in arrays, so its layout can never change.
	#[cfg(target_pointer_width = "64")]
	#[test]
	fn cel_value_layout_is_frozen() {
		assert_eq!(mem::size_of::<AgwCelValue>(), 24);
		assert_eq!(mem::align_of::<AgwCelValue>(), 8);
		assert_eq!(mem::offset_of!(AgwCelValue, kind), 0);
		assert_eq!(mem::offset_of!(AgwCelValue, flags), 4);
		assert_eq!(mem::offset_of!(AgwCelValue, value), 8);
		assert_eq!(mem::size_of::<AgwCelPayload>(), 16);
		assert_eq!(mem::size_of::<AgwCelTime>(), 16);
		assert_eq!(mem::offset_of!(AgwCelTime, nanos), 8);
		assert_eq!(mem::offset_of!(AgwCelTime, offset_seconds), 12);
		assert_eq!(mem::offset_of!(AgwCelItems, len), 8);
	}
}
