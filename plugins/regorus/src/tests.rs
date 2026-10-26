use agentgateway_sdk::testing::{TestEnvironment, TestRequest, TestResponse};
use futures_executor::block_on;
use http::StatusCode;
use serde_json::json;

use super::*;

const POLICY: &str = r#"
package agentgateway

default result := false

result := "admin" if {
  input.headers["x-user"] == "admin"
}

result := "tenant-route" if {
  input.path == "/tenant"
  input.headers["x-tenant"] != null
}
"#;

fn plugin() -> RegoPlugin {
	RegoPlugin::new(json!({ "resultHeader": "x-rego-result", "policy": POLICY })).unwrap()
}

/// Runs the request phase and returns the policy result passed to the
/// response phase, or `None` if the request was denied.
fn evaluate(request: TestRequest) -> Option<HeaderValue> {
	let mut env = TestEnvironment::new();
	match block_on(env.run_request(&plugin(), request)).unwrap() {
		HttpRequestAction::Continue {
			response: ResponsePhase::Run(result),
		} => Some(result),
		HttpRequestAction::DirectResponse { status, .. } => {
			assert_eq!(status, 403);
			None
		},
		HttpRequestAction::Continue { .. } => panic!("expected response state"),
	}
}

#[test]
fn allows_admin() {
	let request = TestRequest::get("/").header(
		HeaderName::from_static("x-user"),
		HeaderValue::from_static("admin"),
	);
	assert_eq!(evaluate(request).unwrap(), "admin");
}

#[test]
fn allows_tenant_route_with_tenant_header() {
	let request = TestRequest::get("/tenant").header(
		HeaderName::from_static("x-tenant"),
		HeaderValue::from_static("acme"),
	);
	assert_eq!(evaluate(request).unwrap(), "tenant-route");
}

#[test]
fn denies_by_default() {
	assert_eq!(evaluate(TestRequest::get("/tenant")), None);
}

#[test]
fn writes_result_to_response_header() {
	let plugin = plugin();
	let mut env = TestEnvironment::new();
	block_on(env.run_response(
		&plugin,
		HeaderValue::from_static("admin"),
		TestResponse::new(StatusCode::OK),
	))
	.unwrap();
	assert_eq!(env.response_headers()["x-rego-result"], "admin");
}

#[test]
fn rejects_invalid_policy() {
	let config = json!({ "resultHeader": "x-rego-result", "policy": "not rego" });
	assert!(RegoPlugin::new(config).is_err());
}
