use super::super::*;

#[doc(hidden)]
pub mod __export {
	use super::*;

	/// Builds a descriptor for a deserializeable HTTP plugin.
	pub const fn descriptor<T>(name: &'static str, version: &'static str) -> abi::AgwPluginDescriptor
	where
		T: HttpPlugin + DeserializeOwned,
	{
		descriptor_with_create(name, version, create_deserialize::<T>)
	}

	/// Builds a descriptor for an HTTP plugin factory.
	pub const fn factory_descriptor<T>(
		name: &'static str,
		version: &'static str,
	) -> abi::AgwPluginDescriptor
	where
		T: HttpPluginFactory,
	{
		descriptor_with_create(name, version, create_factory::<T>)
	}

	const fn descriptor_with_create(
		name: &'static str,
		version: &'static str,
		create: unsafe extern "C" fn(
			abi::AgwBytes,
			*const abi::AgwHostVTable,
			*mut abi::AgwPluginInstance,
			*mut abi::AgwError,
		) -> u32,
	) -> abi::AgwPluginDescriptor {
		abi::AgwPluginDescriptor {
			size: std::mem::size_of::<abi::AgwPluginDescriptor>(),
			abi_version: abi::AGW_PLUGIN_ABI_VERSION,
			name: abi::AgwString::from_static(name),
			version: abi::AgwString::from_static(version),
			create: Some(create),
		}
	}

	struct HttpPluginHolder<T: HttpPlugin> {
		plugin: Arc<T>,
		host: Host,
	}

	type HttpRequestActionFuture<T> = Pin<
		Box<dyn Future<Output = Result<HttpRequestAction<<T as HttpPlugin>::ResponseState>>> + Send>,
	>;
	type ResponseActionFuture = Pin<Box<dyn Future<Output = Result<ResponseAction>> + Send>>;

	struct HttpRequestFuture<T: HttpPlugin> {
		inner: HttpRequestActionFuture<T>,
		/// The request's context; invalidated before the host can free it.
		handle: Arc<ContextHandle>,
		host: Host,
		plugin: Arc<T>,
	}

	impl<T: HttpPlugin> Drop for HttpRequestFuture<T> {
		fn drop(&mut self) {
			self.handle.invalidate();
		}
	}

	struct ResponseSessionHolder<T: HttpPlugin> {
		plugin: Arc<T>,
		state: Option<T::ResponseState>,
		host: Host,
	}

	struct ResponseApplyFuture {
		inner: ResponseActionFuture,
		/// The response's context; invalidated before the host can free it.
		handle: Arc<ContextHandle>,
	}

	impl Drop for ResponseApplyFuture {
		fn drop(&mut self) {
			self.handle.invalidate();
		}
	}

	unsafe extern "C" fn create_deserialize<T>(
		config: abi::AgwBytes,
		host: *const abi::AgwHostVTable,
		out: *mut abi::AgwPluginInstance,
		err: *mut abi::AgwError,
	) -> u32
	where
		T: HttpPlugin + DeserializeOwned,
	{
		unsafe {
			create::<T, _>(config, host, out, err, |config| {
				serde_json::from_value(config).map_err(Error::msg)
			})
		}
	}

	unsafe extern "C" fn create_factory<T>(
		config: abi::AgwBytes,
		host: *const abi::AgwHostVTable,
		out: *mut abi::AgwPluginInstance,
		err: *mut abi::AgwError,
	) -> u32
	where
		T: HttpPluginFactory,
	{
		unsafe { create::<T, _>(config, host, out, err, T::new) }
	}

	unsafe fn create<T, F>(
		config: abi::AgwBytes,
		host: *const abi::AgwHostVTable,
		out: *mut abi::AgwPluginInstance,
		err: *mut abi::AgwError,
		new: F,
	) -> u32
	where
		T: HttpPlugin,
		F: FnOnce(Value) -> Result<T>,
	{
		let result = catch_unwind(AssertUnwindSafe(|| unsafe {
			let host = abi::read_sized(host)
				.ok_or_else(|| Error::msg("host vtable pointer is null or invalid"))?;
			context::set_host(Host { vtable: host });
			let config = deserialize_plugin_config(config.as_slice())?;
			let plugin = context::with_host(Host { vtable: host }, || new(config))?;
			let holder = Box::new(HttpPluginHolder {
				plugin: Arc::new(plugin),
				host: Host { vtable: host },
			});
			let instance = abi::AgwPluginInstance {
				state: Box::into_raw(holder).cast::<c_void>(),
				apply: Some(apply_http_request::<T>),
				drop: Some(drop_http_plugin::<T>),
				..Default::default()
			};
			if let Err(instance) = abi::write_sized(out, instance) {
				drop_http_plugin::<T>(instance.state);
				return Err(Error::msg(
					"plugin instance output pointer is null or invalid",
				));
			}
			Ok(())
		}));

		match result {
			Ok(Ok(())) => abi::AGW_STATUS_OK,
			Ok(Err(e)) => {
				unsafe { write_error(err, e.to_string()) };
				abi::AGW_STATUS_ERROR
			},
			Err(_) => {
				unsafe { write_error(err, "plugin config deserialization panicked") };
				abi::AGW_STATUS_ERROR
			},
		}
	}

	fn deserialize_plugin_config(config: &[u8]) -> Result<Value> {
		if config.is_empty() || config == b"null" {
			Ok(Value::Object(Default::default()))
		} else {
			serde_json::from_slice(config).map_err(|e| Error::msg(format!("invalid plugin config: {e}")))
		}
	}

	unsafe extern "C" fn apply_http_request<T: HttpPlugin>(
		state: *mut c_void,
		ctx: *const abi::AgwContext,
	) -> abi::AgwApplyFuture {
		let holder = unsafe { &*(state.cast::<HttpPluginHolder<T>>()) };
		let plugin = holder.plugin.clone();
		let host = holder.host;
		let handle = ContextHandle::new(host, ctx);
		let inner: HttpRequestActionFuture<T> = if ctx.is_null() {
			Box::pin(async move { Err(Error::msg("request context is null")) })
		} else {
			let request = RequestContext {
				handle: handle.clone(),
			};
			let plugin = plugin.clone();
			Box::pin(async move { plugin.on_request(request).await })
		};
		abi::AgwApplyFuture {
			state: Box::into_raw(Box::new(HttpRequestFuture::<T> {
				inner,
				handle,
				host,
				plugin,
			}))
			.cast::<c_void>(),
			poll: Some(poll_http_request_apply::<T>),
			drop: Some(drop_http_request_apply::<T>),
		}
	}

	unsafe extern "C" fn poll_http_request_apply<T: HttpPlugin>(
		state: *mut c_void,
		waker: *const abi::AgwRawWaker,
		out: *mut abi::AgwApplyResult,
	) -> u32 {
		let result = catch_unwind(AssertUnwindSafe(|| unsafe {
			let future = &mut *(state.cast::<HttpRequestFuture<T>>());
			let host = future.host;
			match context::with_host(host, || {
				abi::with_context(waker, |cx| future.inner.as_mut().poll(cx))
			}) {
				Poll::Pending => abi::AGW_POLL_PENDING,
				Poll::Ready(result) => {
					// The host may free the context as soon as this returns.
					future.handle.invalidate();
					write_apply_result(
						out,
						OwnedApplyResult::from_http_request_result(result, future.plugin.clone(), future.host)
							.into_ffi(),
					);
					abi::AGW_POLL_READY
				},
			}
		}));
		match result {
			Ok(poll) => poll,
			Err(_) => {
				// READY lets the host free the context; a panicking future must not keep it.
				unsafe { (*state.cast::<HttpRequestFuture<T>>()).handle.invalidate() };
				unsafe {
					write_apply_result(
						out,
						OwnedApplyResult::from_error(Error::msg("plugin apply future panicked")).into_ffi(),
					)
				};
				abi::AGW_POLL_READY
			},
		}
	}

	unsafe extern "C" fn apply_http_response<T: HttpPlugin>(
		state: *mut c_void,
		ctx: *const abi::AgwContext,
	) -> abi::AgwApplyFuture {
		let holder = unsafe { &mut *(state.cast::<ResponseSessionHolder<T>>()) };
		let handle = ContextHandle::new(holder.host, ctx);
		let plugin = holder.plugin.clone();
		let inner: ResponseActionFuture = match (ctx.is_null(), holder.state.take()) {
			(true, _) => Box::pin(async move { Err(Error::msg("response context is null")) }),
			(false, None) => Box::pin(async move { Err(Error::msg("response session has already run")) }),
			(false, Some(state)) => {
				let response = ResponseContext {
					handle: handle.clone(),
				};
				Box::pin(async move { plugin.on_response(state, response).await })
			},
		};
		abi::AgwApplyFuture {
			state: Box::into_raw(Box::new(ResponseApplyFuture { inner, handle })).cast::<c_void>(),
			poll: Some(poll_response_apply),
			drop: Some(drop_response_apply),
		}
	}

	unsafe extern "C" fn poll_response_apply(
		state: *mut c_void,
		waker: *const abi::AgwRawWaker,
		out: *mut abi::AgwApplyResult,
	) -> u32 {
		let result = catch_unwind(AssertUnwindSafe(|| unsafe {
			let future = &mut *(state.cast::<ResponseApplyFuture>());
			let host = *future.handle.host();
			match context::with_host(host, || {
				abi::with_context(waker, |cx| future.inner.as_mut().poll(cx))
			}) {
				Poll::Pending => abi::AGW_POLL_PENDING,
				Poll::Ready(result) => {
					// The host may free the context as soon as this returns.
					future.handle.invalidate();
					write_apply_result(
						out,
						OwnedApplyResult::from_response_result(result).into_ffi(),
					);
					abi::AGW_POLL_READY
				},
			}
		}));
		match result {
			Ok(poll) => poll,
			Err(_) => {
				// READY lets the host free the context; a panicking future must not keep it.
				unsafe { (*state.cast::<ResponseApplyFuture>()).handle.invalidate() };
				unsafe {
					write_apply_result(
						out,
						OwnedApplyResult::from_error(Error::msg("plugin apply future panicked")).into_ffi(),
					)
				};
				abi::AGW_POLL_READY
			},
		}
	}

	unsafe extern "C" fn drop_http_request_apply<T: HttpPlugin>(state: *mut c_void) {
		abi::catch_panic((), || {
			if !state.is_null() {
				unsafe {
					drop(Box::from_raw(state.cast::<HttpRequestFuture<T>>()));
				}
			}
		})
	}

	unsafe extern "C" fn drop_response_apply(state: *mut c_void) {
		abi::catch_panic((), || {
			if !state.is_null() {
				unsafe {
					drop(Box::from_raw(state.cast::<ResponseApplyFuture>()));
				}
			}
		})
	}

	unsafe extern "C" fn drop_http_plugin<T: HttpPlugin>(state: *mut c_void) {
		abi::catch_panic((), || {
			if !state.is_null() {
				unsafe {
					drop(Box::from_raw(state.cast::<HttpPluginHolder<T>>()));
				}
			}
		})
	}

	unsafe extern "C" fn drop_response_session<T: HttpPlugin>(state: *mut c_void) {
		abi::catch_panic((), || {
			if !state.is_null() {
				unsafe {
					drop(Box::from_raw(state.cast::<ResponseSessionHolder<T>>()));
				}
			}
		})
	}

	unsafe fn write_apply_result(out: *mut abi::AgwApplyResult, result: abi::AgwApplyResult) {
		if let Err(mut result) = unsafe { abi::write_sized(out, result) } {
			unsafe { result.free() };
		}
	}

	#[cfg(test)]
	pub(crate) fn owned_bytes(bytes: Vec<u8>) -> abi::AgwOwnedBytes {
		let boxed = Box::new(bytes);
		abi::AgwOwnedBytes {
			bytes: abi::AgwBytes::from_slice(boxed.as_slice()),
			private_data: Box::into_raw(boxed).cast::<c_void>(),
			drop: Some(drop_vec_u8),
		}
	}

	#[cfg(test)]
	unsafe extern "C" fn drop_vec_u8(data: *mut c_void) {
		if !data.is_null() {
			unsafe {
				drop(Box::from_raw(data.cast::<Vec<u8>>()));
			}
		}
	}

	struct OwnedHeader {
		name: Vec<u8>,
		value: Vec<u8>,
	}

	impl From<Header> for OwnedHeader {
		fn from(value: Header) -> Self {
			Self {
				name: value.name.as_str().as_bytes().to_vec(),
				value: value.value.as_bytes().to_vec(),
			}
		}
	}

	struct OwnedApplyResult {
		status: u32,
		response_session: abi::AgwPluginResponseSession,
		direct_status: u16,
		direct_headers: Vec<OwnedHeader>,
		direct_header_views: Vec<abi::AgwHeader>,
		direct_body_stream: abi::AgwBodyStream,
		error_message: Vec<u8>,
	}

	impl OwnedApplyResult {
		fn from_http_request_result<T: HttpPlugin>(
			result: Result<HttpRequestAction<T::ResponseState>>,
			plugin: Arc<T>,
			host: Host,
		) -> Box<Self> {
			let mut owned = match result {
				Ok(HttpRequestAction::Continue { response }) => Self {
					status: abi::AGW_APPLY_CONTINUE,
					response_session: match response {
						ResponsePhase::Skip => abi::AgwPluginResponseSession::default(),
						ResponsePhase::Run(state) => {
							let holder = Box::new(ResponseSessionHolder {
								plugin,
								state: Some(state),
								host,
							});
							abi::AgwPluginResponseSession {
								state: Box::into_raw(holder).cast::<c_void>(),
								apply: Some(apply_http_response::<T>),
								drop: Some(drop_response_session::<T>),
							}
						},
					},
					direct_status: 0,
					direct_headers: Vec::new(),
					direct_header_views: Vec::new(),
					direct_body_stream: abi::AgwBodyStream::default(),
					error_message: Vec::new(),
				},
				Ok(HttpRequestAction::DirectResponse {
					status,
					headers,
					body,
				}) => Self {
					status: abi::AGW_APPLY_DIRECT_RESPONSE,
					response_session: abi::AgwPluginResponseSession::default(),
					direct_status: status,
					direct_headers: headers.into_iter().map(Into::into).collect(),
					direct_header_views: Vec::new(),
					direct_body_stream: body_stream_from_direct_body(body),
					error_message: Vec::new(),
				},
				Err(error) => Self::error(error),
			};
			owned.rebuild_views();
			Box::new(owned)
		}

		fn from_response_result(result: Result<ResponseAction>) -> Box<Self> {
			let mut owned = match result {
				Ok(ResponseAction::Continue) => Self::continue_without_response(),
				Ok(ResponseAction::DirectResponse {
					status,
					headers,
					body,
				}) => Self::direct_response(status, headers, body),
				Err(error) => Self::error(error),
			};
			owned.rebuild_views();
			Box::new(owned)
		}

		fn from_error(error: Error) -> Box<Self> {
			Box::new(Self::error(error))
		}

		fn continue_without_response() -> Self {
			Self {
				status: abi::AGW_APPLY_CONTINUE,
				response_session: abi::AgwPluginResponseSession::default(),
				direct_status: 0,
				direct_headers: Vec::new(),
				direct_header_views: Vec::new(),
				direct_body_stream: abi::AgwBodyStream::default(),
				error_message: Vec::new(),
			}
		}

		fn direct_response(status: u16, headers: Vec<Header>, body: DirectBody) -> Self {
			Self {
				status: abi::AGW_APPLY_DIRECT_RESPONSE,
				response_session: abi::AgwPluginResponseSession::default(),
				direct_status: status,
				direct_headers: headers.into_iter().map(Into::into).collect(),
				direct_header_views: Vec::new(),
				direct_body_stream: body_stream_from_direct_body(body),
				error_message: Vec::new(),
			}
		}

		fn error(error: Error) -> Self {
			Self {
				status: abi::AGW_APPLY_ERROR,
				response_session: abi::AgwPluginResponseSession::default(),
				direct_status: 0,
				direct_headers: Vec::new(),
				direct_header_views: Vec::new(),
				direct_body_stream: abi::AgwBodyStream::default(),
				error_message: error.to_string().into_bytes(),
			}
		}

		fn rebuild_views(&mut self) {
			self.direct_header_views = headers_to_ffi(&self.direct_headers);
		}

		fn into_ffi(mut self: Box<Self>) -> abi::AgwApplyResult {
			// Handles move into the result; the owner keeps only what the result borrows.
			let body_stream = std::mem::take(&mut self.direct_body_stream);
			let response_session = std::mem::take(&mut self.response_session);
			let owner = Box::into_raw(self);
			let owned = unsafe { &*owner };
			let error_message = std::str::from_utf8(&owned.error_message).unwrap_or("plugin error");
			abi::AgwApplyResult {
				size: std::mem::size_of::<abi::AgwApplyResult>(),
				status: owned.status,
				direct_response: abi::AgwDirectResponse {
					present: (owned.status == abi::AGW_APPLY_DIRECT_RESPONSE) as u8,
					status: owned.direct_status,
					headers: abi::AgwHeaders::from_slice(&owned.direct_header_views),
					body_stream,
				},
				response_session,
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
				private_data: owner.cast::<c_void>(),
				drop: Some(drop_owned_apply_result),
			}
		}
	}

	fn headers_to_ffi(headers: &[OwnedHeader]) -> Vec<abi::AgwHeader> {
		headers
			.iter()
			.map(|h| abi::AgwHeader {
				name: abi::AgwString::from_borrowed(
					std::str::from_utf8(&h.name).expect("header name is valid UTF-8"),
				),
				value: abi::AgwBytes::from_slice(&h.value),
			})
			.collect()
	}

	unsafe extern "C" fn drop_owned_apply_result(data: *mut c_void) {
		if !data.is_null() {
			unsafe {
				drop(Box::from_raw(data.cast::<OwnedApplyResult>()));
			}
		}
	}
}
