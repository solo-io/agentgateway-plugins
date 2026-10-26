use agentgateway_sdk::testing::{TestEnvironment, TestRequest, TestResponse};
use futures_executor::block_on;
use http::{Method, StatusCode};
use serde_json::json;

use super::*;

fn plugin(code: &str) -> QuickJs {
	QuickJs::new(json!({ "code": code })).unwrap()
}

#[test]
fn typescript_direct_response() {
	let plugin = plugin(
		r#"
		interface Greeting { name: string }
		async function onRequest(req: GatewayRequest): Promise<DirectResponse> {
			const greeting: Greeting = { name: req.header("x-name") ?? "world" };
			return { status: 200, headers: { "content-type": "text/plain" }, body: `hello ${greeting.name}` };
		}
		"#,
	);
	let mut env = TestEnvironment::new();
	let request = TestRequest::get("/").header(
		HeaderName::from_static("x-name"),
		HeaderValue::from_static("agentgateway"),
	);
	let HttpRequestAction::DirectResponse { status, body, .. } =
		block_on(env.run_request(&plugin, request)).unwrap()
	else {
		panic!("expected a direct response");
	};
	assert_eq!(status, 200);
	let agentgateway_sdk::DirectBody::Bytes(body) = body else {
		panic!("expected a buffered body");
	};
	assert_eq!(body, "hello agentgateway");
}

#[test]
fn mutates_request_and_reads_body() {
	let plugin = plugin(
		r#"
		const onRequest = async (req) => {
			const body = await req.text();
			req.setHeader("x-body-length", String(body.length));
			req.removeHeader("x-secret");
			req.setMetadata("seen", { path: req.path, method: req.method });
		};
		"#,
	);
	let mut env = TestEnvironment::new();
	let request = TestRequest::new(Method::POST, "/upload?x=1")
		.header(
			HeaderName::from_static("x-secret"),
			HeaderValue::from_static("s"),
		)
		.body("hello");
	let action = block_on(env.run_request(&plugin, request)).unwrap();
	assert!(matches!(
		action,
		HttpRequestAction::Continue {
			response: ResponsePhase::Skip
		}
	));
	let headers = env.request_headers();
	assert_eq!(headers["x-body-length"], "5");
	assert!(!headers.contains_key("x-secret"));
	assert_eq!(
		env.metadata()["seen"],
		json!({ "path": "/upload?x=1", "method": "POST" })
	);
}

#[test]
fn passes_state_to_response_phase() {
	let plugin = plugin(
		r#"
		function onRequest(req) {
			return { state: { user: req.header("x-user") } };
		}
		async function onResponse(res, state) {
			res.setHeader("x-user", state.user);
			res.setHeader("x-upstream-status", String(res.status));
			if (res.status >= 500) res.setStatus(502);
		}
		"#,
	);
	let mut env = TestEnvironment::new();
	let request = TestRequest::get("/").header(
		HeaderName::from_static("x-user"),
		HeaderValue::from_static("alice"),
	);
	let HttpRequestAction::Continue {
		response: ResponsePhase::Run(state),
	} = block_on(env.run_request(&plugin, request)).unwrap()
	else {
		panic!("expected the response phase to run");
	};
	assert_eq!(state, json!({ "user": "alice" }));

	block_on(env.run_response(
		&plugin,
		state,
		TestResponse::new(StatusCode::SERVICE_UNAVAILABLE),
	))
	.unwrap();
	let headers = env.response_headers();
	assert_eq!(headers["x-user"], "alice");
	assert_eq!(headers["x-upstream-status"], "503");
	assert_eq!(env.response_status(), Some(StatusCode::BAD_GATEWAY));
}

#[test]
fn reads_properties() {
	let plugin = plugin(
		r#"
		function onRequest(req) {
			req.setHeader("x-ip", req.property(["source", "address"]));
		}
		"#,
	);
	let mut env = TestEnvironment::new();
	env
		.insert_property(["source", "address"], "10.0.0.1")
		.unwrap();
	block_on(env.run_request(&plugin, TestRequest::get("/"))).unwrap();
	assert_eq!(env.request_headers()["x-ip"], "10.0.0.1");
}

#[test]
fn handler_errors_fail_the_request() {
	let plugin = plugin(r#"function onRequest() { throw new Error("boom"); }"#);
	let mut env = TestEnvironment::new();
	let Err(error) = block_on(env.run_request(&plugin, TestRequest::get("/"))) else {
		panic!("expected an error");
	};
	assert!(error.to_string().contains("boom"), "{error}");
}

#[test]
fn infinite_loops_time_out() {
	let plugin = QuickJs::new(json!({
		"code": "function onRequest() { while (true) {} }",
		"timeoutMs": 50,
	}))
	.unwrap();
	let mut env = TestEnvironment::new();
	let Err(error) = block_on(env.run_request(&plugin, TestRequest::get("/"))) else {
		panic!("expected an error");
	};
	assert!(error.to_string().contains("timeout"), "{error}");
}

#[test]
fn rejects_invalid_code() {
	let err = |code: &str| {
		QuickJs::new(json!({ "code": code }))
			.err()
			.expect("expected an error")
			.to_string()
	};
	// Syntax errors are caught by the TypeScript parser.
	assert!(err("function onRequest( {").contains("invalid TypeScript"));
	assert!(err("const x: = 1;").contains("invalid TypeScript"));
	assert!(err("throw new Error('bad config')").contains("bad config"));
	assert!(err("const x = 1;").contains("must define"));
}

#[test]
fn rejects_typescript_runtime_features() {
	let err = QuickJs::new(json!({ "code": "enum Color { Red } function onRequest() {}" }))
		.err()
		.expect("expected an error")
		.to_string();
	assert!(err.contains("invalid TypeScript"), "{err}");
}
