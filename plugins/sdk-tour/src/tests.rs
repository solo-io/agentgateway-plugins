use agentgateway_sdk::testing::{TestEnvironment, TestRequest, TestResponse};
use futures_executor::block_on;
use http::StatusCode;
use serde_json::json;

use super::*;

fn plugin() -> SdkTour {
	SdkTour::new(json!({ "calloutUrl": "http://callout.example/" })).unwrap()
}

#[test]
fn direct_response() {
	let mut env = TestEnvironment::new();
	let action = block_on(env.run_request(&plugin(), TestRequest::get("/direct"))).unwrap();
	let HttpRequestAction::DirectResponse {
		status, headers, ..
	} = action
	else {
		panic!("expected a direct response");
	};
	assert_eq!(status, 200);
	assert!(
		headers
			.iter()
			.any(|h| h.name == ACTION_HEADER && h.value == "direct")
	);
}

#[test]
fn continue_records_metadata_and_runs_response_phase() {
	let plugin = plugin();
	let mut env = TestEnvironment::new();
	let request = TestRequest::get("/anything")
		.header(
			HeaderName::from_static("x-short"),
			HeaderValue::from_static("a"),
		)
		.header(
			HeaderName::from_static("x-long"),
			HeaderValue::from_static("aaaaaaaa"),
		);

	let action = block_on(env.run_request(&plugin, request)).unwrap();
	let HttpRequestAction::Continue {
		response: ResponsePhase::Run(state),
	} = action
	else {
		panic!("expected the request to continue with response state");
	};
	assert_eq!(env.request_headers()[&ACTION_HEADER], "continue");
	assert_eq!(
		env.metadata()["longestHeader"],
		json!({ "name": "x-long", "length": 8 })
	);

	block_on(env.run_response(&plugin, state, TestResponse::new(StatusCode::IM_A_TEAPOT))).unwrap();
	let headers = env.response_headers();
	assert_eq!(headers[&ACTION_HEADER], "continue");
	assert_eq!(headers["x-tour-upstream-status"], "418");
	assert!(headers.contains_key("x-tour-elapsed-ms"));
	assert!(!headers.contains_key("x-tour-cel"));
}

#[test]
fn rejects_invalid_config() {
	assert!(SdkTour::new(json!({})).is_err());
	assert!(SdkTour::new(json!({ "calloutUrl": "not a uri" })).is_err());
	assert!(SdkTour::new(json!({ "calloutUrl": "/relative" })).is_err());
}
