#![cfg_attr(
	all(doc, feature = "doc-images"),
	doc = embed_doc_image::embed_image!(
	"plugin-architecture",
	"diagram.png"
)
)]
#![doc = include_str!("../README.md")]

use std::borrow::Cow;
use std::cell::Cell;
use std::ffi::c_void;
use std::fmt::Display;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

pub use agentgateway_abi as abi;
pub use async_trait::async_trait;
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub mod body;
pub mod callout;
pub mod cel;
mod context;
pub mod error;
mod ffi;
pub mod header;
pub mod plugin;
pub mod request;
pub mod response;
pub mod runtime;
#[cfg(feature = "testing")]
pub mod testing;
mod value;

pub use body::DirectBody;
pub(crate) use body::{BodyStream, BodyStreamNext, body_stream_from_direct_body};
pub use callout::{HttpCall, HttpRequest, HttpResponse, http_call};
pub use cel::{CelExpression, compile_cel};
pub(crate) use cel::{property, set_metadata};
pub(crate) use context::{ContextHandle, Host};
pub use error::{Error, Result};
pub(crate) use error::{error_from_abi, write_error};
pub(crate) use ffi::body::body_to_abi;
#[cfg(test)]
pub(crate) use ffi::body::{
	write_owned_data_chunk, write_owned_end_chunk, write_owned_trailer_chunk,
};
pub(crate) use ffi::callout::{FfiHostHttpRequest, response_from_ffi};
#[doc(hidden)]
pub use ffi::plugin::__export;
pub use header::Header;
pub(crate) use header::{PseudoHeader, header_value_from_host, headers_from_abi};
pub use plugin::{HttpPlugin, HttpPluginFactory};
#[cfg(test)]
pub(crate) use request::RequestBody;
pub(crate) use request::status_to_result;
pub use request::{HttpRequestAction, RequestContext};
pub use response::{ResponseAction, ResponseContext, ResponsePhase};
pub use runtime::{LogLevel, log, log_enabled, sleep};
pub use value::{CelDuration, CelJson, CelKind, CelResult, CelTimestamp, CelValue, MetadataValue};

/// Exports a plugin as agentgateway's plugin entrypoint.
///
/// Everything after the plugin type is an optional keyword argument, so new
/// capabilities can be added as new keywords:
///
/// ```ignore
/// // Config is deserialized from JSON into `MyPlugin`.
/// export_plugin!(MyPlugin);
/// export_plugin!(MyPlugin, name = "my-plugin", version = "1.2.3");
/// // `MyPlugin` implements `HttpPluginFactory`, for creation that needs
/// // validation, setup, or defaults that don't fit plain `serde`.
/// export_plugin!(MyPlugin, name = "my-plugin", factory);
/// ```
///
/// `name` defaults to the type name and `version` to the crate version.
#[macro_export]
macro_rules! export_plugin {
	($plugin:ty $(, $($args:tt)*)?) => {
		$crate::__export_plugin!(
			@parse $plugin;
			name = (stringify!($plugin));
			version = (env!("CARGO_PKG_VERSION"));
			create = (descriptor);
			$($($args)*)?
		);
	};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_plugin {
	(@parse $plugin:ty; name = ($n:expr); version = ($v:expr); create = ($c:ident);
		name = $name:expr $(, $($rest:tt)*)?) => {
		$crate::__export_plugin!(@parse $plugin; name = ($name); version = ($v); create = ($c);
			$($($rest)*)?);
	};
	(@parse $plugin:ty; name = ($n:expr); version = ($v:expr); create = ($c:ident);
		version = $version:expr $(, $($rest:tt)*)?) => {
		$crate::__export_plugin!(@parse $plugin; name = ($n); version = ($version); create = ($c);
			$($($rest)*)?);
	};
	(@parse $plugin:ty; name = ($n:expr); version = ($v:expr); create = ($c:ident);
		factory $(, $($rest:tt)*)?) => {
		$crate::__export_plugin!(@parse $plugin; name = ($n); version = ($v);
			create = (factory_descriptor); $($($rest)*)?);
	};
	(@parse $plugin:ty; name = ($n:expr); version = ($v:expr); create = ($c:ident);) => {
		#[unsafe(no_mangle)]
		pub unsafe extern "C" fn agentgateway_plugin_entrypoint()
		-> *const $crate::abi::AgwPluginDescriptor {
			static DESCRIPTOR: $crate::abi::AgwPluginDescriptor =
				$crate::__export::$c::<$plugin>($n, $v);
			&DESCRIPTOR
		}
	};
	(@parse $plugin:ty; name = ($n:expr); version = ($v:expr); create = ($c:ident);
		$($unknown:tt)+) => {
		compile_error!(concat!(
			"export_plugin!: unknown argument `",
			stringify!($($unknown)+),
			"`; expected `name = ...`, `version = ...`, or `factory`"
		));
	};
}

#[cfg(test)]
mod tests {
	use std::collections::VecDeque;
	use std::sync::Mutex;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::task::Waker;

	use super::*;

	static CEL_COMPILES: AtomicUsize = AtomicUsize::new(0);
	static CEL_EVALS: AtomicUsize = AtomicUsize::new(0);
	static CEL_DROPS: AtomicUsize = AtomicUsize::new(0);
	static RESPONSE_STATUS: AtomicUsize = AtomicUsize::new(0);
	static RESPONSE_BODY: Mutex<Vec<u8>> = Mutex::new(Vec::new());
	static METADATA: Mutex<Vec<(String, serde_json::Value)>> = Mutex::new(Vec::new());
	static HEADER_MUTATIONS: Mutex<Vec<(Vec<u8>, Vec<u8>)>> = Mutex::new(Vec::new());
	static DESCRIPTOR_TEST_LOCK: Mutex<()> = Mutex::new(());

	// The mock hosts never dereference the context; it only has to be non-null.
	fn test_ctx() -> *const abi::AgwContext {
		std::ptr::NonNull::dangling().as_ptr()
	}

	fn test_request(host: Host) -> RequestContext {
		RequestContext {
			handle: ContextHandle::new(host, test_ctx()),
		}
	}
	static BODY_STREAM_DROPS: AtomicUsize = AtomicUsize::new(0);

	#[derive(serde::Deserialize)]
	struct TestPlugin {
		enabled: bool,
	}

	#[async_trait]
	impl HttpPlugin for TestPlugin {
		type ResponseState = ();

		async fn on_request(
			&self,
			request: RequestContext,
		) -> Result<HttpRequestAction<Self::ResponseState>> {
			assert!(self.enabled);
			assert_eq!(request.method(), Some(Method::GET));
			assert_eq!(request.scheme(), Some(Scheme::HTTP));
			assert_eq!(
				request.authority(),
				Some(Authority::from_static("example.com"))
			);
			assert_eq!(request.path(), Some(PathAndQuery::from_static("/")));
			assert_eq!(
				request.header(&HeaderName::from_static("x-input")),
				Some(HeaderValue::from_static("from-host"))
			);
			assert_eq!(
				request.headers().collect::<Vec<_>>(),
				vec![
					(
						HeaderName::from_static("x-input"),
						HeaderValue::from_static("from-host")
					),
					(
						HeaderName::from_static("x-other"),
						HeaderValue::from_static("other")
					),
				]
			);
			request.set_header(
				HeaderName::from_static("x-test-plugin"),
				HeaderValue::from_static("ok"),
			)?;
			Ok(HttpRequestAction::Continue {
				response: ResponsePhase::Skip,
			})
		}

		async fn on_response(
			&self,
			_state: Self::ResponseState,
			_response: ResponseContext,
		) -> Result<ResponseAction> {
			Ok(ResponseAction::Continue)
		}
	}

	#[test]
	fn descriptor_create_and_apply_round_trip() {
		let _guard = DESCRIPTOR_TEST_LOCK.lock().unwrap();
		HEADER_MUTATIONS.lock().unwrap().clear();
		let descriptor = __export::descriptor::<TestPlugin>("test", "1.2.3");
		assert_eq!(descriptor.abi_version, abi::AGW_PLUGIN_ABI_VERSION);
		assert_eq!(unsafe { descriptor.name.as_str() }, "test");
		assert_eq!(unsafe { descriptor.version.as_str() }, "1.2.3");
		assert!(descriptor.create.is_some());

		let config = br#"{"enabled":true}"#;
		let mut instance = abi::AgwPluginInstance::default();
		let mut err = abi::AgwError::default();
		let create = descriptor.create.unwrap();
		let host = test_host_vtable();
		let status = unsafe {
			create(
				abi::AgwBytes::from_slice(config),
				&host,
				&mut instance,
				&mut err,
			)
		};
		assert_eq!(status, abi::AGW_STATUS_OK);
		assert!(instance.apply.is_some());

		let request = test_ctx();
		let apply = instance.apply.unwrap();
		let future = unsafe { apply(instance.state, request) };
		let poll = future.poll.unwrap();
		let waker = Waker::noop();
		let mut result = abi::AgwApplyResult::default();
		let poll_status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			poll(future.state, raw_waker, &mut result)
		});
		assert_eq!(poll_status, abi::AGW_POLL_READY);
		assert_eq!(result.status, abi::AGW_APPLY_CONTINUE);

		assert_eq!(
			&*HEADER_MUTATIONS.lock().unwrap(),
			&[(b"x-test-plugin".to_vec(), b"ok".to_vec())]
		);

		unsafe { result.free() };
		if let Some(drop) = future.drop {
			unsafe { drop(future.state) };
		}
		if let Some(drop) = instance.drop {
			unsafe { drop(instance.state) };
		}
	}

	#[test]
	fn cel_expression_compiles_once_and_reuses_handle() {
		CEL_COMPILES.store(0, Ordering::SeqCst);
		CEL_EVALS.store(0, Ordering::SeqCst);
		CEL_DROPS.store(0, Ordering::SeqCst);

		let host = Host {
			vtable: abi::AgwHostVTable {
				compile_cel: Some(test_compile_cel),
				eval_cel: Some(test_eval_cel),
				..Default::default()
			},
		};
		let request = test_request(host);
		let expression = CelExpression::new("request.path");

		assert_eq!(
			expression.evaluate(&request).unwrap().as_str(),
			Some("request.path")
		);
		assert_eq!(
			expression.evaluate(&request).unwrap().as_str(),
			Some("request.path")
		);
		assert_eq!(CEL_COMPILES.load(Ordering::SeqCst), 1);
		assert_eq!(CEL_EVALS.load(Ordering::SeqCst), 2);
		drop(expression);
		assert_eq!(CEL_DROPS.load(Ordering::SeqCst), 1);
	}

	#[test]
	fn host_get_property_returns_optional_json_value() {
		let host = Host {
			vtable: abi::AgwHostVTable {
				get_property: Some(test_get_property),
				..Default::default()
			},
		};
		let request = test_request(host);

		let group = request.property(&["jwt", "group"]).unwrap();
		assert_eq!(group.as_deref().and_then(CelValue::as_str), Some("admin"));
		assert!(request.property(&["jwt", "missing"]).unwrap().is_none());
		assert!(request.property(&[]).is_err());
	}

	#[test]
	fn host_set_metadata_takes_json_and_cel_values() {
		METADATA.lock().unwrap().clear();
		let host = Host {
			vtable: abi::AgwHostVTable {
				set_metadata: Some(test_set_metadata),
				..Default::default()
			},
		};
		let request = test_request(host);

		{
			request.set_metadata(
				"decision",
				&serde_json::json!({ "allow": true, "score": 7 }),
			)
		}
		.unwrap();
		let metadata = METADATA.lock().unwrap();
		assert_eq!(metadata.len(), 1);
		assert_eq!(metadata[0].0, "decision");
		assert_eq!(
			metadata[0].1,
			serde_json::json!({ "allow": true, "score": 7 })
		);
		drop(metadata);
		// A CEL value goes through as is, types included.
		let cel_value = unsafe { value::CelResult::from_raw(value::TestValue::UInt(7).to_abi()) }
			.unwrap()
			.unwrap();
		request.set_metadata("count", &cel_value).unwrap();
		assert_eq!(METADATA.lock().unwrap()[1].1, serde_json::json!(7));
		assert!(request.set_metadata("", &serde_json::json!(true)).is_err());
	}

	#[test]
	fn request_body_collect_reads_host_body() {
		let _guard = DESCRIPTOR_TEST_LOCK.lock().unwrap();
		RESPONSE_BODY.lock().unwrap().clear();
		let host = Host {
			vtable: abi::AgwHostVTable {
				body_stream: Some(test_body_stream),
				set_body_stream: Some(test_set_body_stream),
				..Default::default()
			},
		};
		let request = test_request(host);

		let mut future = Box::pin(request.body().collect(1024));
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		match future.as_mut().poll(&mut cx) {
			Poll::Ready(Ok(body)) => assert_eq!(body, b"hello world"),
			other => panic!("unexpected body poll result: {other:?}"),
		}
		// The collected bytes are handed back so the host still forwards them.
		assert_eq!(&*RESPONSE_BODY.lock().unwrap(), b"hello world");
	}

	#[test]
	fn request_body_stream_yields_chunks_and_eof() {
		BODY_STREAM_DROPS.store(0, Ordering::SeqCst);
		let host = Host {
			vtable: abi::AgwHostVTable {
				body_stream: Some(test_body_stream),
				..Default::default()
			},
		};
		let request = test_request(host);

		let mut stream = request.body();
		assert_eq!(
			poll_body_stream_next(&mut stream).unwrap(),
			Some(bytes::Bytes::from_static(b"hello "))
		);
		assert_eq!(
			poll_body_stream_next(&mut stream).unwrap(),
			Some(bytes::Bytes::from_static(b"world"))
		);
		assert_eq!(poll_body_stream_next(&mut stream).unwrap(), None);
		assert_eq!(poll_body_stream_next(&mut stream).unwrap(), None);

		drop(stream);
		assert!(BODY_STREAM_DROPS.load(Ordering::SeqCst) >= 1);
	}

	#[test]
	fn body_stream_implements_http_body() {
		BODY_STREAM_DROPS.store(0, Ordering::SeqCst);
		let host = Host {
			vtable: abi::AgwHostVTable {
				body_stream: Some(test_body_stream),
				..Default::default()
			},
		};
		let request = test_request(host);

		let mut stream = request.body();
		assert_eq!(
			poll_body_frame(&mut stream).unwrap(),
			Some(bytes::Bytes::from_static(b"hello "))
		);
		assert_eq!(
			poll_body_frame(&mut stream).unwrap(),
			Some(bytes::Bytes::from_static(b"world"))
		);
		assert_eq!(poll_body_frame(&mut stream).unwrap(), None);

		drop(stream);
		assert!(BODY_STREAM_DROPS.load(Ordering::SeqCst) >= 1);
	}

	#[test]
	fn body_stream_preserves_trailers() {
		let host = Host {
			vtable: abi::AgwHostVTable {
				body_stream: Some(test_trailer_body_stream),
				..Default::default()
			},
		};
		let request = test_request(host);

		let mut stream = request.body();
		assert_eq!(
			poll_body_frame(&mut stream).unwrap(),
			Some(bytes::Bytes::from_static(b"hello"))
		);
		let trailers = poll_body_trailers(&mut stream).unwrap();
		assert_eq!(
			trailers.get(HeaderName::from_static("x-trailer")),
			Some(&HeaderValue::from_static("ok"))
		);
		assert_eq!(poll_body_frame(&mut stream).unwrap(), None);
	}

	#[test]
	fn to_http_request_materializes_headers_and_stream_body() {
		BODY_STREAM_DROPS.store(0, Ordering::SeqCst);
		let host = Host {
			vtable: abi::AgwHostVTable {
				get_header: Some(test_get_header),
				headers: Some(test_headers),
				body_stream: Some(test_body_stream),
				..Default::default()
			},
		};
		let request = test_request(host);

		let mut request = request.to_http_request().unwrap();
		assert_eq!(request.method(), Method::GET);
		assert_eq!(request.uri(), "/");
		assert_eq!(
			request.headers().get(HeaderName::from_static("x-input")),
			Some(&HeaderValue::from_static("from-host"))
		);
		assert_eq!(
			poll_body_frame(request.body_mut()).unwrap(),
			Some(bytes::Bytes::from_static(b"hello "))
		);

		drop(request);
		assert!(BODY_STREAM_DROPS.load(Ordering::SeqCst) >= 1);
	}

	#[derive(serde::Deserialize)]
	struct DirectStreamPlugin {}

	#[async_trait]
	impl HttpPlugin for DirectStreamPlugin {
		type ResponseState = ();

		async fn on_request(
			&self,
			_request: RequestContext,
		) -> Result<HttpRequestAction<Self::ResponseState>> {
			Ok(HttpRequestAction::DirectResponse {
				status: 418,
				headers: vec![Header::new(
					HeaderName::from_static("x-direct"),
					HeaderValue::from_static("stream"),
				)],
				body: DirectBody::body(TestHttpBody::new([
					b"hello ".as_slice(),
					b"world".as_slice(),
				])),
			})
		}

		async fn on_response(
			&self,
			_state: Self::ResponseState,
			_response: ResponseContext,
		) -> Result<ResponseAction> {
			Ok(ResponseAction::Continue)
		}
	}

	#[test]
	fn direct_response_body_stream_exports_abi_stream() {
		BODY_STREAM_DROPS.store(0, Ordering::SeqCst);
		let descriptor = __export::descriptor::<DirectStreamPlugin>("direct-stream", "1.0.0");
		let mut instance = abi::AgwPluginInstance::default();
		let mut err = abi::AgwError::default();
		let create = descriptor.create.unwrap();
		let host = test_host_vtable();
		let status = unsafe {
			create(
				abi::AgwBytes::from_slice(b"null"),
				&host,
				&mut instance,
				&mut err,
			)
		};
		assert_eq!(status, abi::AGW_STATUS_OK);

		let request = test_ctx();
		let apply = instance.apply.unwrap();
		let future = unsafe { apply(instance.state, request) };
		let poll = future.poll.unwrap();
		let waker = Waker::noop();
		let mut result = abi::AgwApplyResult::default();
		let poll_status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			poll(future.state, raw_waker, &mut result)
		});
		assert_eq!(poll_status, abi::AGW_POLL_READY);
		assert_eq!(result.status, abi::AGW_APPLY_DIRECT_RESPONSE);
		assert_eq!(result.direct_response.present, 1);
		assert_eq!(result.direct_response.status, 418);
		assert!(result.direct_response.body_stream.poll_next.is_some());

		let mut stream = std::mem::take(&mut result.direct_response.body_stream);
		assert_eq!(
			poll_abi_body_stream(&mut stream).unwrap(),
			Some(b"hello ".to_vec())
		);
		assert_eq!(
			poll_abi_body_stream(&mut stream).unwrap(),
			Some(b"world".to_vec())
		);
		assert_eq!(poll_abi_body_stream(&mut stream).unwrap(), None);
		drop_abi_body_stream(&mut stream);

		unsafe { result.free() };
		if let Some(drop) = future.drop {
			unsafe { drop(future.state) };
		}
		if let Some(drop) = instance.drop {
			unsafe { drop(instance.state) };
		}
	}

	static PANIC_RETAINED: Mutex<Option<RequestContext>> = Mutex::new(None);

	/// Keeps its context, then panics.
	#[derive(serde::Deserialize)]
	struct RetainsAndPanics {}

	#[async_trait]
	impl HttpPlugin for RetainsAndPanics {
		type ResponseState = ();

		async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<()>> {
			*PANIC_RETAINED.lock().unwrap() = Some(request);
			panic!("plugin bug");
		}
	}

	// Regression: a panicking future reported READY, which lets the host free the
	// context, but left the context live for anything that retained it.
	#[test]
	fn panicking_future_invalidates_the_context() {
		let descriptor = __export::descriptor::<RetainsAndPanics>("panics", "1.0.0");
		let mut instance = abi::AgwPluginInstance::default();
		let mut err = abi::AgwError::default();
		let host = test_host_vtable();
		let status = unsafe {
			descriptor.create.unwrap()(
				abi::AgwBytes::from_slice(b"{}"),
				&host,
				&mut instance,
				&mut err,
			)
		};
		assert_eq!(status, abi::AGW_STATUS_OK);

		let future = unsafe { instance.apply.unwrap()(instance.state, test_ctx()) };
		let mut result = abi::AgwApplyResult::default();
		let poll_status = abi::with_borrowed_waker(Waker::noop(), |raw_waker| unsafe {
			future.poll.unwrap()(future.state, raw_waker, &mut result)
		});
		assert_eq!(poll_status, abi::AGW_POLL_READY);
		assert_eq!(result.status, abi::AGW_APPLY_ERROR);

		// The future is not dropped yet; READY alone must have ended the context.
		let retained = PANIC_RETAINED.lock().unwrap().take().unwrap();
		assert!(
			retained
				.set_header(
					HeaderName::from_static("x-late"),
					HeaderValue::from_static("1")
				)
				.is_err()
		);

		unsafe { result.free() };
		unsafe { future.drop.unwrap()(future.state) };
		unsafe { instance.drop.unwrap()(instance.state) };
	}

	#[derive(serde::Deserialize)]
	struct TestHttpPlugin {}

	#[async_trait]
	impl HttpPlugin for TestHttpPlugin {
		type ResponseState = String;

		async fn on_request(
			&self,
			request: RequestContext,
		) -> Result<HttpRequestAction<Self::ResponseState>> {
			let method = request
				.method()
				.map(|method| method.to_string())
				.unwrap_or_default();
			let path = request
				.path()
				.map(|path| path.to_string())
				.unwrap_or_default();
			Ok(HttpRequestAction::Continue {
				response: ResponsePhase::Run(format!("{method} {path}")),
			})
		}

		async fn on_response(
			&self,
			state: Self::ResponseState,
			response: ResponseContext,
		) -> Result<ResponseAction> {
			assert_eq!(state, "GET /");
			assert_eq!(response.status(), Some(StatusCode::CREATED));
			assert_eq!(
				response.header(&HeaderName::from_static("x-input")),
				Some(HeaderValue::from_static("from-host"))
			);
			let body = response.body().collect(1024).await?;
			assert_eq!(body, b"hello world");
			response.set_header(
				HeaderName::from_static("x-test-plugin"),
				HeaderValue::from_static("ok"),
			)?;
			response.set_status(202)?;
			response.set_body(b"patched body")?;
			Ok(ResponseAction::Continue)
		}
	}

	#[test]
	fn http_plugin_request_creates_response_session_and_runs_response() {
		let _guard = DESCRIPTOR_TEST_LOCK.lock().unwrap();
		RESPONSE_STATUS.store(0, Ordering::SeqCst);
		RESPONSE_BODY.lock().unwrap().clear();
		HEADER_MUTATIONS.lock().unwrap().clear();

		let descriptor = __export::descriptor::<TestHttpPlugin>("test-http", "4.5.6");
		let mut instance = abi::AgwPluginInstance::default();
		let mut err = abi::AgwError::default();
		let create = descriptor.create.unwrap();
		let host = test_host_vtable();
		let status = unsafe {
			create(
				abi::AgwBytes::from_slice(b"null"),
				&host,
				&mut instance,
				&mut err,
			)
		};
		assert_eq!(status, abi::AGW_STATUS_OK);

		let request = test_ctx();
		let apply = instance.apply.unwrap();
		let future = unsafe { apply(instance.state, request) };
		let poll = future.poll.unwrap();
		let waker = Waker::noop();
		let mut result = abi::AgwApplyResult::default();
		let poll_status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			poll(future.state, raw_waker, &mut result)
		});
		assert_eq!(poll_status, abi::AGW_POLL_READY);
		assert_eq!(result.status, abi::AGW_APPLY_CONTINUE);
		assert!(result.response_session.apply.is_some());

		let response_session = std::mem::take(&mut result.response_session);
		unsafe { result.free() };
		if let Some(drop) = future.drop {
			unsafe { drop(future.state) };
		}

		let response = test_ctx();
		let response_apply = response_session.apply.unwrap();
		let response_future = unsafe { response_apply(response_session.state, response) };
		let response_poll = response_future.poll.unwrap();
		let mut response_result = abi::AgwApplyResult::default();
		let poll_status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			response_poll(response_future.state, raw_waker, &mut response_result)
		});
		assert_eq!(poll_status, abi::AGW_POLL_READY);
		assert_eq!(
			response_result.status,
			abi::AGW_APPLY_CONTINUE,
			"{}",
			unsafe { response_result.error.message.as_str() }
		);
		assert_eq!(
			&*HEADER_MUTATIONS.lock().unwrap(),
			&[
				(b"x-test-plugin".to_vec(), b"ok".to_vec()),
				(b":status".to_vec(), b"202".to_vec())
			]
		);
		assert_eq!(RESPONSE_STATUS.load(Ordering::SeqCst), 202);
		assert_eq!(&*RESPONSE_BODY.lock().unwrap(), b"patched body");

		unsafe { response_result.free() };
		if let Some(drop) = response_future.drop {
			unsafe { drop(response_future.state) };
		}
		if let Some(drop) = response_session.drop {
			unsafe { drop(response_session.state) };
		}
		if let Some(drop) = instance.drop {
			unsafe { drop(instance.state) };
		}
	}

	struct TestHostBodyStream {
		chunks: Vec<&'static [u8]>,
		index: usize,
	}

	struct TestTrailerBodyStream {
		index: usize,
	}

	struct TestHttpBody {
		chunks: VecDeque<bytes::Bytes>,
	}

	impl TestHttpBody {
		fn new(chunks: impl IntoIterator<Item = &'static [u8]>) -> Self {
			Self {
				chunks: chunks.into_iter().map(bytes::Bytes::from_static).collect(),
			}
		}
	}

	impl Drop for TestHttpBody {
		fn drop(&mut self) {
			BODY_STREAM_DROPS.fetch_add(1, Ordering::SeqCst);
		}
	}

	impl http_body::Body for TestHttpBody {
		type Data = bytes::Bytes;
		type Error = Error;

		fn poll_frame(
			mut self: Pin<&mut Self>,
			_cx: &mut Context<'_>,
		) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
			Poll::Ready(
				self
					.chunks
					.pop_front()
					.map(|chunk| Ok(http_body::Frame::data(chunk))),
			)
		}
	}

	fn poll_body_stream_next(stream: &mut RequestBody) -> Result<Option<bytes::Bytes>> {
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		let mut next = Box::pin(stream.next_chunk());
		match next.as_mut().poll(&mut cx) {
			Poll::Ready(result) => result,
			Poll::Pending => panic!("test body stream should be ready"),
		}
	}

	fn poll_body_frame<B>(body: &mut B) -> Result<Option<bytes::Bytes>>
	where
		B: http_body::Body<Data = bytes::Bytes, Error = Error> + Unpin,
	{
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		match Pin::new(body).poll_frame(&mut cx) {
			Poll::Ready(Some(Ok(frame))) => Ok(Some(
				frame
					.into_data()
					.expect("test body should yield data frames"),
			)),
			Poll::Ready(Some(Err(error))) => Err(error),
			Poll::Ready(None) => Ok(None),
			Poll::Pending => panic!("test body stream should be ready"),
		}
	}

	fn poll_body_trailers<B>(body: &mut B) -> Result<HeaderMap>
	where
		B: http_body::Body<Data = bytes::Bytes, Error = Error> + Unpin,
	{
		let waker = Waker::noop();
		let mut cx = Context::from_waker(waker);
		match Pin::new(body).poll_frame(&mut cx) {
			Poll::Ready(Some(Ok(frame))) => frame
				.into_trailers()
				.map_err(|_| Error::msg("test body should yield trailers")),
			Poll::Ready(Some(Err(error))) => Err(error),
			Poll::Ready(None) => Err(Error::msg("test body ended before trailers")),
			Poll::Pending => panic!("test body stream should be ready"),
		}
	}

	fn poll_abi_body_stream(stream: &mut abi::AgwBodyStream) -> Result<Option<Vec<u8>>> {
		let poll_next = stream.poll_next.expect("test stream should have poll_next");
		let waker = Waker::noop();
		let mut out = abi::AgwBodyChunk::default();
		let status = abi::with_borrowed_waker(waker, |raw_waker| unsafe {
			poll_next(stream.state, raw_waker, &mut out)
		});
		assert_eq!(status, abi::AGW_POLL_READY);
		let result = match out.kind {
			abi::AGW_BODY_FRAME_END if out.error.code != 0 || out.error.message.len != 0 => {
				Err(error_from_abi(&out.error))
			},
			abi::AGW_BODY_FRAME_DATA => Ok(Some(unsafe { out.bytes.as_slice() }.to_vec())),
			abi::AGW_BODY_FRAME_TRAILERS => Ok(Some(Vec::new())),
			abi::AGW_BODY_FRAME_END => Ok(None),
			other => Err(Error::msg(format!(
				"unexpected ABI body frame kind {other}"
			))),
		};
		unsafe { out.free() };
		result
	}

	fn drop_abi_body_stream(stream: &mut abi::AgwBodyStream) {
		if let Some(drop) = stream.drop.take()
			&& !stream.state.is_null()
		{
			unsafe { drop(stream.state) };
		}
		stream.state = std::ptr::null_mut();
	}

	fn test_host_vtable() -> abi::AgwHostVTable {
		abi::AgwHostVTable {
			get_header: Some(test_get_header),
			headers: Some(test_headers),
			add_header: Some(test_set_header),
			set_header: Some(test_set_header),
			remove_header: Some(test_remove_header),
			body_stream: Some(test_body_stream),
			set_body_stream: Some(test_set_body_stream),
			..Default::default()
		}
	}

	unsafe extern "C" fn test_get_header(
		_ctx: *const abi::AgwContext,
		name: abi::AgwString,
		out: *mut abi::AgwOwnedBytes,
	) -> u32 {
		let value = match unsafe { name.as_bytes() } {
			name if name.eq_ignore_ascii_case(b"x-input") => Some(b"from-host".as_slice()),
			b":method" => Some(b"GET".as_slice()),
			b":scheme" => Some(b"http".as_slice()),
			b":authority" => Some(b"example.com".as_slice()),
			b":path" => Some(b"/".as_slice()),
			b":status" => Some(b"201".as_slice()),
			_ => None,
		};
		if let Some(value) = value {
			if !out.is_null() {
				unsafe {
					*out = __export::owned_bytes(value.to_vec());
				}
			}
			abi::AGW_STATUS_OK
		} else {
			if !out.is_null() {
				unsafe {
					*out = abi::AgwOwnedBytes::default();
				}
			}
			abi::AGW_STATUS_OK
		}
	}

	unsafe extern "C" fn test_headers(_ctx: *const abi::AgwContext) -> abi::AgwHeaderIterator {
		abi::AgwHeaderIterator {
			state: Box::into_raw(Box::new(0usize)).cast::<c_void>(),
			next: Some(test_next_header),
			drop: Some(test_drop_headers),
		}
	}

	unsafe extern "C" fn test_next_header(state: *mut c_void, out: *mut abi::AgwHeader) -> u32 {
		let index = unsafe { &mut *(state.cast::<usize>()) };
		let (name, value) = match *index {
			0 => (b"x-input".as_slice(), b"from-host".as_slice()),
			1 => (b"x-other".as_slice(), b"other".as_slice()),
			_ => {
				if !out.is_null() {
					unsafe { *out = abi::AgwHeader::default() };
				}
				return abi::AGW_STATUS_OK;
			},
		};
		*index += 1;
		unsafe {
			*out = abi::AgwHeader {
				name: abi::AgwString::from_borrowed(std::str::from_utf8(name).unwrap()),
				value: abi::AgwBytes::from_slice(value),
			};
		}
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_drop_headers(state: *mut c_void) {
		if !state.is_null() {
			unsafe {
				drop(Box::from_raw(state.cast::<usize>()));
			}
		}
	}

	unsafe extern "C" fn test_compile_cel(
		_data: *const c_void,
		expression: abi::AgwString,
		out: *mut abi::AgwCelExpression,
		_err: *mut abi::AgwError,
	) -> u32 {
		CEL_COMPILES.fetch_add(1, Ordering::SeqCst);
		if !out.is_null() {
			let expression = unsafe { expression.as_bytes() }.to_vec();
			unsafe {
				*out = abi::AgwCelExpression {
					state: Box::into_raw(Box::new(expression)).cast::<c_void>(),
					drop: Some(test_drop_cel),
				};
			}
		}
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_body_stream(_ctx: *const abi::AgwContext) -> abi::AgwBodyStream {
		abi::AgwBodyStream {
			state: Box::into_raw(Box::new(TestHostBodyStream {
				chunks: vec![b"hello ".as_slice(), b"world".as_slice()],
				index: 0,
			}))
			.cast::<c_void>(),
			poll_next: Some(test_poll_body_stream),
			drop: Some(test_drop_body_stream),
		}
	}

	unsafe extern "C" fn test_poll_body_stream(
		state: *mut c_void,
		_waker: *const abi::AgwRawWaker,
		out: *mut abi::AgwBodyChunk,
	) -> u32 {
		let stream = unsafe { &mut *(state.cast::<TestHostBodyStream>()) };
		if let Some(chunk) = stream.chunks.get(stream.index) {
			stream.index += 1;
			unsafe {
				write_owned_data_chunk(out, chunk.to_vec());
			}
		} else {
			unsafe {
				write_owned_end_chunk(out);
			}
		}
		abi::AGW_POLL_READY
	}

	unsafe extern "C" fn test_drop_body_stream(state: *mut c_void) {
		BODY_STREAM_DROPS.fetch_add(1, Ordering::SeqCst);
		if !state.is_null() {
			unsafe {
				drop(Box::from_raw(state.cast::<TestHostBodyStream>()));
			}
		}
	}

	unsafe extern "C" fn test_trailer_body_stream(
		_ctx: *const abi::AgwContext,
	) -> abi::AgwBodyStream {
		abi::AgwBodyStream {
			state: Box::into_raw(Box::new(TestTrailerBodyStream { index: 0 })).cast::<c_void>(),
			poll_next: Some(test_poll_trailer_body_stream),
			drop: Some(test_drop_trailer_body_stream),
		}
	}

	unsafe extern "C" fn test_poll_trailer_body_stream(
		state: *mut c_void,
		_waker: *const abi::AgwRawWaker,
		out: *mut abi::AgwBodyChunk,
	) -> u32 {
		let stream = unsafe { &mut *(state.cast::<TestTrailerBodyStream>()) };
		match stream.index {
			0 => unsafe {
				stream.index += 1;
				write_owned_data_chunk(out, b"hello".to_vec());
			},
			1 => unsafe {
				stream.index += 1;
				let mut trailers = HeaderMap::new();
				trailers.insert(
					HeaderName::from_static("x-trailer"),
					HeaderValue::from_static("ok"),
				);
				write_owned_trailer_chunk(out, trailers);
			},
			_ => unsafe {
				write_owned_end_chunk(out);
			},
		}
		abi::AGW_POLL_READY
	}

	unsafe extern "C" fn test_drop_trailer_body_stream(state: *mut c_void) {
		if !state.is_null() {
			unsafe {
				drop(Box::from_raw(state.cast::<TestTrailerBodyStream>()));
			}
		}
	}

	unsafe extern "C" fn test_eval_cel(
		_ctx: *const abi::AgwContext,
		expression: *const abi::AgwCelExpression,
		out: *mut abi::AgwCelResult,
		_err: *mut abi::AgwError,
	) -> u32 {
		CEL_EVALS.fetch_add(1, Ordering::SeqCst);
		let expression = unsafe { &*expression };
		let expression = unsafe { &*(expression.state.cast::<Vec<u8>>()) };
		let value = serde_json::Value::String(String::from_utf8_lossy(expression).into_owned());
		unsafe {
			*out = value::json_result(value);
		}
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_get_property(
		_ctx: *const abi::AgwContext,
		path: abi::AgwStringSlice,
		out: *mut abi::AgwCelResult,
		err: *mut abi::AgwError,
	) -> u32 {
		let path = unsafe { path.as_slice() }
			.iter()
			.map(|segment| unsafe { segment.as_str() })
			.collect::<Vec<_>>();
		match path.as_slice() {
			[] => {
				unsafe {
					write_error(err, "empty path");
				}
				abi::AGW_STATUS_ERROR
			},
			["jwt", "group"] => {
				unsafe {
					*out = value::json_result(serde_json::json!("admin"));
				}
				abi::AGW_STATUS_OK
			},
			_ => abi::AGW_STATUS_OK,
		}
	}

	unsafe extern "C" fn test_drop_cel(data: *mut c_void) {
		CEL_DROPS.fetch_add(1, Ordering::SeqCst);
		if !data.is_null() {
			unsafe {
				drop(Box::from_raw(data.cast::<Vec<u8>>()));
			}
		}
	}

	unsafe extern "C" fn test_set_metadata(
		_ctx: *const abi::AgwContext,
		key: abi::AgwString,
		value: *const abi::AgwCelValue,
		err: *mut abi::AgwError,
	) -> u32 {
		let key = unsafe { key.as_str() };
		if key.is_empty() {
			unsafe {
				write_error(err, "empty key");
			}
			return abi::AGW_STATUS_ERROR;
		}
		let value = match unsafe { value::borrowed_json(&*value) } {
			Ok(value) => value,
			Err(error) => {
				unsafe {
					write_error(err, error.to_string());
				}
				return abi::AGW_STATUS_ERROR;
			},
		};
		METADATA.lock().unwrap().push((key.to_string(), value));
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_set_header(
		_ctx: *const abi::AgwContext,
		name: abi::AgwString,
		value: abi::AgwBytes,
	) -> u32 {
		if unsafe { name.as_bytes() } == b":status" {
			let status = std::str::from_utf8(unsafe { value.as_slice() })
				.unwrap()
				.parse::<usize>()
				.unwrap();
			RESPONSE_STATUS.store(status, Ordering::SeqCst);
		}
		HEADER_MUTATIONS.lock().unwrap().push((
			unsafe { name.as_bytes() }.to_vec(),
			unsafe { value.as_slice() }.to_vec(),
		));
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_remove_header(
		_ctx: *const abi::AgwContext,
		_name: abi::AgwString,
	) -> u32 {
		abi::AGW_STATUS_OK
	}

	unsafe extern "C" fn test_set_body_stream(
		_ctx: *const abi::AgwContext,
		mut stream: abi::AgwBodyStream,
	) -> u32 {
		let Some(poll_next) = stream.poll_next else {
			return abi::AGW_STATUS_ERROR;
		};
		let waker = Waker::noop();
		let mut body = Vec::new();
		loop {
			let mut out = abi::AgwBodyChunk::default();
			match abi::with_borrowed_waker(waker, |raw_waker| unsafe {
				poll_next(stream.state, raw_waker, &mut out)
			}) {
				abi::AGW_POLL_READY => match out.kind {
					abi::AGW_BODY_FRAME_DATA => {
						body.extend_from_slice(unsafe { out.bytes.as_slice() });
						unsafe { out.free() };
					},
					abi::AGW_BODY_FRAME_END => {
						unsafe { out.free() };
						break;
					},
					_ => {
						unsafe { out.free() };
						return abi::AGW_STATUS_ERROR;
					},
				},
				_ => return abi::AGW_STATUS_ERROR,
			}
		}
		if let Some(drop) = stream.drop.take()
			&& !stream.state.is_null()
		{
			unsafe { drop(stream.state) };
		}
		*RESPONSE_BODY.lock().unwrap() = body;
		abi::AGW_STATUS_OK
	}
}
