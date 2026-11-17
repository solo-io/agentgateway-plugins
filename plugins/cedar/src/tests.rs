use agentgateway_sdk::DirectBody;
use agentgateway_sdk::testing::{TestEnvironment, TestRequest, TestResponse};
use futures_executor::block_on;
use http::uri::Authority;
use http::{Method, StatusCode};
use serde_json::json;

use super::*;

const POLICIES: &str = r#"
@id("admins-can-do-anything")
permit (principal in Group::"admins", action, resource);

@id("anyone-can-read-public")
permit (principal, action == Action::"GET", resource)
when { context.path like "/public/*" };

@id("tenant-header-required")
permit (principal, action == Action::"GET", resource == Route::"/tenant")
when { context has headers && context.headers has "x-tenant" && context.headers["x-tenant"] == "acme" };

@id("no-deletes-on-prod")
forbid (principal, action == Action::"DELETE", resource)
when { context has host && context.host == "prod.example.com" };
"#;

fn entities() -> Value {
	json!([
		{ "uid": { "type": "User", "id": "alice" }, "attrs": {}, "parents": [{ "type": "Group", "id": "admins" }] },
		{ "uid": { "type": "Group", "id": "admins" }, "attrs": {}, "parents": [] },
	])
}

fn header_principal() -> Value {
	json!({ "id": { "header": "x-user" } })
}

fn jwt_principal() -> Value {
	json!({
		"id": { "property": "jwt.sub" },
		"attributes": { "property": "jwt" },
		"parents": { "type": "Group", "from": { "property": "jwt.groups" } },
	})
}

fn base_config() -> Value {
	json!({
		"policies": POLICIES,
		"entities": entities(),
		"principal": header_principal(),
		"decisionHeader": "x-cedar-decision",
		"context": { "headers": ["x-tenant"] },
	})
}

fn plugin(config: Value) -> CedarPlugin {
	CedarPlugin::new(config).unwrap()
}

fn load_error(config: Value) -> String {
	match CedarPlugin::new(config) {
		Ok(_) => panic!("config loaded"),
		Err(e) => e.to_string(),
	}
}

fn hv(value: &'static str) -> HeaderValue {
	HeaderValue::from_static(value)
}

#[derive(Debug)]
enum Verdict {
	Continue(ResponsePhase<&'static str>),
	Respond {
		status: u16,
		body: Value,
		headers: Vec<Header>,
	},
}

impl Verdict {
	fn allowed(&self) -> bool {
		matches!(self, Verdict::Continue(_))
	}
	fn status(&self) -> u16 {
		match self {
			Verdict::Continue(_) => 200,
			Verdict::Respond { status, .. } => *status,
		}
	}
}

fn run_in(env: &mut TestEnvironment, plugin: &CedarPlugin, request: TestRequest) -> Verdict {
	match block_on(env.run_request(plugin, request)).unwrap() {
		HttpRequestAction::Continue { response } => Verdict::Continue(response),
		HttpRequestAction::DirectResponse {
			status,
			headers,
			body,
		} => {
			let DirectBody::Bytes(bytes) = body else {
				panic!("expected buffered body")
			};
			Verdict::Respond {
				status,
				body: serde_json::from_slice(&bytes).unwrap(),
				headers,
			}
		},
	}
}

fn run(plugin: &CedarPlugin, request: TestRequest) -> (Verdict, TestEnvironment) {
	let mut env = TestEnvironment::new();
	(run_in(&mut env, plugin, request), env)
}

/// Runs a request with `jwt` claims set as the host would expose them.
fn run_jwt(plugin: &CedarPlugin, claims: Value, request: TestRequest) -> Verdict {
	let mut env = TestEnvironment::new();
	for (key, value) in claims.as_object().unwrap() {
		env.insert_property(["jwt", key.as_str()], value).unwrap();
	}
	env.insert_property(["jwt"], &claims).unwrap();
	run_in(&mut env, plugin, request)
}

#[test]
fn base_policy_decisions() {
	let p = plugin(base_config());
	// Headers named `host` set the request authority.
	type Case = (
		&'static str,
		Method,
		&'static str,
		&'static [(&'static str, &'static str)],
		bool,
	);
	#[rustfmt::skip]
	let cases: &[Case] = &[
		("static group member", Method::POST, "/admin", &[("x-user", "alice")], true),
		("unknown user", Method::POST, "/admin", &[("x-user", "bob")], false),
		("anonymous public read", Method::GET, "/public/docs", &[], true),
		("anonymous private read", Method::GET, "/private/docs", &[], false),
		("anonymous public write", Method::POST, "/public/docs", &[], false),
		("tenant acme", Method::GET, "/tenant", &[("x-tenant", "acme")], true),
		("tenant other", Method::GET, "/tenant", &[("x-tenant", "globex")], false),
		("tenant missing", Method::GET, "/tenant", &[], false),
		("admin delete on dev", Method::DELETE, "/x", &[("x-user", "alice"), ("host", "dev.example.com")], true),
		("forbid wins on prod", Method::DELETE, "/x", &[("x-user", "alice"), ("host", "prod.example.com")], false),
		("client groups header ignored", Method::POST, "/admin", &[("x-user", "bob"), ("x-groups", "admins")], false),
	];
	for (name, method, path, headers, want) in cases {
		let mut request = TestRequest::new(method.clone(), *path);
		for (h, v) in *headers {
			request = match *h {
				"host" => request.authority(Authority::from_static(v)),
				_ => request.header(HeaderName::from_static(h), hv(v)),
			};
		}
		assert_eq!(run(&p, request).0.allowed(), *want, "{name}");
	}
}

#[test]
fn allow_records_metadata_and_decision_header() {
	let request =
		TestRequest::new(Method::POST, "/admin").header(HeaderName::from_static("x-user"), hv("alice"));
	let (verdict, env) = run(&plugin(base_config()), request);
	assert!(matches!(
		verdict,
		Verdict::Continue(ResponsePhase::Run("allow"))
	));
	assert_eq!(env.metadata()["decision"], "allow");
	assert_eq!(
		env.metadata()["policies"],
		json!(["admins-can-do-anything"])
	);
}

#[test]
fn deny_response_hides_reasons_unless_exposed() {
	let request = || {
		TestRequest::new(Method::DELETE, "/thing")
			.header(HeaderName::from_static("x-user"), hv("alice"))
			.authority(Authority::from_static("prod.example.com"))
	};
	for (expose, want_policies) in [(false, None), (true, Some(json!(["no-deletes-on-prod"])))] {
		let mut config = base_config();
		config["exposeReasons"] = json!(expose);
		let (verdict, env) = run(&plugin(config), request());
		let Verdict::Respond {
			status,
			body,
			headers,
		} = verdict
		else {
			panic!("expected deny (exposeReasons={expose})")
		};
		assert_eq!(status, 403);
		assert_eq!(body["error"]["type"], "authorization_denied");
		assert_eq!(body["error"].get("policies").cloned(), want_policies);
		assert!(
			headers
				.iter()
				.any(|h| h.name == "x-cedar-decision" && h.value == "deny")
		);
		assert_eq!(env.metadata()["decision"], "deny");
	}
}

#[test]
fn audit_mode_records_but_does_not_block() {
	let mut config = base_config();
	config["mode"] = json!("audit");
	let (verdict, env) = run(&plugin(config), TestRequest::new(Method::POST, "/admin"));
	assert!(matches!(
		verdict,
		Verdict::Continue(ResponsePhase::Run("deny"))
	));
	assert_eq!(env.metadata()["decision"], "deny");
	assert!(env.logs().iter().any(|l| l.message.contains("audit mode")));
}

#[test]
fn jwt_principal_attributes_and_groups() {
	let p = plugin(json!({
		"policies": r#"
			permit (principal in Group::"ml-engineers", action == Action::"POST", resource == Route::"/v1/chat/completions")
			when { principal.tier == "gold" && principal.org == "acme" };
			permit (principal in Group::"Smith, Jane", action == Action::"GET", resource);
		"#,
		"principal": jwt_principal(),
	}));
	#[rustfmt::skip]
	let cases = [
		("gold member", Method::POST, json!({ "tier": "gold", "groups": ["ml-engineers", "staff"] }), true),
		("silver member", Method::POST, json!({ "tier": "silver", "groups": ["ml-engineers"] }), false),
		("gold non-member", Method::POST, json!({ "tier": "gold", "groups": ["staff"] }), false),
		("string groups split on commas", Method::POST, json!({ "tier": "gold", "groups": "staff, ml-engineers" }), true),
		("array items not split", Method::GET, json!({ "groups": ["Smith, Jane"] }), true),
	];
	for (name, method, extra, want) in cases {
		let mut claims =
			json!({ "sub": "carol", "org": "acme", "score": 0.7, "nbf": null, "big": u64::MAX });
		claims
			.as_object_mut()
			.unwrap()
			.extend(extra.as_object().unwrap().clone());
		let verdict = run_jwt(&p, claims, TestRequest::new(method, "/v1/chat/completions"));
		assert_eq!(verdict.allowed(), want, "{name}");
	}
}

const SCHEMA: &str = r#"
entity Group;
entity User in [Group] { tier?: String };
entity Route;
type Ctx = { method: String, path: String, query?: String, host?: String };
action "GET", "POST", "DELETE" appliesTo { principal: User, resource: Route, context: Ctx };
"#;

#[test]
fn schema_validates_policies_and_requests() {
	let p = plugin(json!({
		"schema": SCHEMA,
		"policies": r#"permit (principal in Group::"admins", action == Action::"GET", resource);"#,
		"entities": entities(),
		"principal": header_principal(),
	}));
	let alice = TestRequest::get("/x").header(HeaderName::from_static("x-user"), hv("alice"));
	assert!(run(&p, alice).0.allowed());
	assert_eq!(run(&p, TestRequest::get("/x")).0.status(), 403);
}

#[test]
fn schema_drops_undeclared_claims() {
	let p = plugin(json!({
		"schema": SCHEMA,
		"policies": r#"permit (principal, action, resource) when { principal has tier && principal.tier == "gold" };"#,
		"principal": jwt_principal(),
	}));
	let claims =
		|tier| json!({ "sub": "carol", "tier": tier, "iss": "idp", "exp": 1, "groups": ["a"] });
	assert!(run_jwt(&p, claims("gold"), TestRequest::get("/x")).allowed());
	assert_eq!(
		run_jwt(&p, claims("silver"), TestRequest::get("/x")).status(),
		403
	);
}

#[test]
fn input_that_breaks_the_schema_returns_error_status() {
	// `context.headers` is configured but the schema's context type lacks it.
	let p = plugin(json!({
		"schema": SCHEMA,
		"policies": r#"permit (principal, action, resource);"#,
		"principal": header_principal(),
		"context": { "headers": ["x-tenant"] },
	}));
	let (verdict, env) = run(&p, TestRequest::get("/x"));
	let Verdict::Respond { status, body, .. } = verdict else {
		panic!("expected error response")
	};
	assert_eq!(status, 503);
	assert_eq!(body["error"]["type"], "authorization_error");
	assert_eq!(env.metadata()["decision"], "error");
}

#[test]
fn mcp_tool_calls() {
	let p = plugin(json!({
		"policies": r#"permit (principal == User::"agent-1", action == Action::"call_tool", resource == Tool::"search");"#,
		"principal": header_principal(),
		"action": { "type": "Action", "id": { "literal": "call_tool" } },
		"resource": { "type": "Tool", "id": { "property": "mcp.tool.name" } },
		"mcpToolCallsOnly": true,
	}));
	#[rustfmt::skip]
	let cases = [
		("allowed tool", Some("tools/call"), Some("search"), true),
		("other tool", Some("tools/call"), Some("delete_repo"), false),
		("tools/call without a tool name", Some("tools/call"), None, false),
		("tools/list skipped", Some("tools/list"), None, true),
		("not MCP: still authorized", None, None, false),
	];
	for (name, method, tool, want) in cases {
		let mut env = TestEnvironment::new();
		if let Some(method) = method {
			env.insert_property(["mcp", "methodName"], method).unwrap();
		}
		if let Some(tool) = tool {
			env.insert_property(["mcp", "tool", "name"], tool).unwrap();
		}
		let request = TestRequest::new(Method::POST, "/mcp")
			.header(HeaderName::from_static("x-user"), hv("agent-1"));
		assert_eq!(run_in(&mut env, &p, request).allowed(), want, "{name}");
	}
}

#[test]
fn context_properties_are_added() {
	let p = plugin(json!({
		"policies": r#"permit (principal, action, resource) when { context has model && context.model == "gpt-4o-mini" };"#,
		"principal": header_principal(),
		"context": { "properties": { "model": "llm.requestModel" } },
	}));
	for (model, want) in [("gpt-4o-mini", true), ("o3", false)] {
		let mut env = TestEnvironment::new();
		env.insert_property(["llm", "requestModel"], model).unwrap();
		assert_eq!(
			run_in(&mut env, &p, TestRequest::get("/")).allowed(),
			want,
			"{model}"
		);
	}
}

#[test]
fn rejects_invalid_config() {
	let permit = "permit (principal, action, resource);";
	let principal = header_principal();
	#[rustfmt::skip]
	let cases = [
		("bad policy", json!({ "policies": "permit (", "principal": principal }), "invalid cedar policies"),
		("empty policies", json!({ "policies": "", "principal": principal }), "empty"),
		("missing principal", json!({ "policies": permit }), "missing field `principal`"),
		("missing principal id", json!({ "policies": permit, "principal": {} }), "missing field `id`"),
		("denyStatus", json!({ "policies": permit, "principal": principal, "denyStatus": 200 }), "denyStatus"),
		("errorStatus", json!({ "policies": permit, "principal": principal, "errorStatus": 99 }), "errorStatus"),
		("unknown field", json!({ "policies": permit, "principal": principal, "bogus": 1 }), "unknown field"),
		("bad type", json!({ "policies": permit, "principal": { "type": "not a type", "id": "path" } }), "invalid cedar entity type"),
		("bad header", json!({ "policies": permit, "principal": { "id": { "header": "bad name" } } }), "header \"bad name\""),
		("bad property", json!({ "policies": permit, "principal": { "id": { "property": "jwt..sub" } } }), "invalid property path"),
		("attributes from header", json!({ "policies": permit, "principal": { "id": "path", "attributes": { "header": "x" } } }), "principal.attributes"),
		("ill-typed policy", json!({ "schema": SCHEMA, "policies": "permit (principal, action, resource) when { principal.level > 3 };", "principal": principal }), "schema validation"),
		("type not in schema", json!({ "schema": SCHEMA, "policies": permit, "principal": { "type": "Person", "id": "path" } }), "Person is not declared"),
	];
	for (name, config, want) in cases {
		let err = load_error(config);
		assert!(err.contains(want), "{name}: {err}");
	}
}

#[test]
fn writes_decision_header_on_response() {
	let p = plugin(base_config());
	let mut env = TestEnvironment::new();
	block_on(env.run_response(&p, "allow", TestResponse::new(StatusCode::OK))).unwrap();
	assert_eq!(env.response_headers()["x-cedar-decision"], "allow");
}
