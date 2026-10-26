use agentgateway_sdk::testing::{TestEnvironment, TestRequest};
use futures_executor::block_on;
use http::Method;
use http_body_util::BodyExt;

use super::*;

/// Runs a request through the plugin and returns the direct response status
/// and body.
fn call(request: TestRequest) -> (u16, String) {
	let plugin = AxumPlugin::new(serde_json::Value::Null).unwrap();
	let mut env = TestEnvironment::new();
	let action = block_on(env.run_request(&plugin, request)).unwrap();
	let HttpRequestAction::DirectResponse { status, body, .. } = action else {
		panic!("expected a direct response");
	};
	let body = match body {
		DirectBody::Bytes(bytes) => bytes,
		DirectBody::Body(body) => block_on(body.collect()).unwrap().to_bytes(),
	};
	(status, String::from_utf8(body.to_vec()).unwrap())
}

#[test]
fn root() {
	assert_eq!(
		call(TestRequest::get("/")),
		(200, "hello from axum\n".to_string())
	);
}

#[test]
fn echo() {
	let request = TestRequest::new(Method::POST, "/echo").body("hello");
	assert_eq!(call(request), (200, "hello".to_string()));
}

#[test]
fn unknown_route() {
	assert_eq!(call(TestRequest::get("/missing")).0, 404);
}
