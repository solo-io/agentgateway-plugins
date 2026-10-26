use agentgateway_sdk::testing::{TestEnvironment, TestRequest};
use futures_executor::block_on;
use serde_json::json;

use super::*;

// `/segfault` isn't tested: it would crash the test runner.

#[test]
fn other_paths_continue() {
	let plugin = Crash::new(json!({})).unwrap();
	let mut env = TestEnvironment::new();
	let action = block_on(env.run_request(&plugin, TestRequest::get("/"))).unwrap();
	assert!(matches!(
		action,
		HttpRequestAction::Continue {
			response: ResponsePhase::Skip
		}
	));
}

#[test]
#[should_panic(expected = "/panic requested")]
fn panic_path_panics() {
	// The mock host calls the plugin directly, so the panic reaches the test.
	// In the gateway, the SDK catches it at the ABI boundary instead.
	let plugin = Crash::new(json!({})).unwrap();
	let mut env = TestEnvironment::new();
	let _ = block_on(env.run_request(&plugin, TestRequest::get("/panic")));
}
