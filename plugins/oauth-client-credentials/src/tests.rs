use super::*;
use agentgateway_sdk::testing::{TestEnvironment, TestRequest, TestResponse};
use futures_executor::block_on;
use serde_json::json;

fn config() -> serde_json::Value {
	json!({"tokenUrl":"http://127.0.0.1:9000/token","clientIdEnv":"PATH","clientSecretEnv":"PATH"})
}
fn plugin() -> Plugin {
	Plugin::new(config()).unwrap()
}
#[test]
fn config_validation() {
	for (name, field, value) in [
		("url", "tokenUrl", json!("not-a-url")),
		("scheme", "tokenUrl", json!("ftp://example.com")),
		(
			"missing env",
			"clientIdEnv",
			json!("AGW_OAUTH_TEST_MISSING_ENV_90482"),
		),
		("status low", "errorStatus", json!(399)),
		("status high", "errorStatus", json!(600)),
		("timeout", "timeoutMs", json!(0)),
		("grant_type", "extraParams", json!({"grant_type":"x"})),
		("client_id", "extraParams", json!({"client_id":"x"})),
		("client_secret", "extraParams", json!({"client_secret":"x"})),
		(
			"missing secret",
			"clientSecretEnv",
			json!("AGW_OAUTH_TEST_SECRET_MISSING_90482"),
		),
		("unknown", "unknown", json!(true)),
		("header", "header", json!("bad header")),
	] {
		let mut c = config();
		c[field] = value;
		assert!(Plugin::new(c).is_err(), "{name}");
	}
	assert!(!format!("{:?}", plugin()).contains(&std::env::var("PATH").unwrap()));
}
#[test]
fn encodings_and_request() {
	for (name, input, want) in [
		("space", "a b", "a+b"),
		("punct", "a:/+~", "a%3A%2F%2B%7E"),
		("unicode", "é", "%C3%A9"),
	] {
		assert_eq!(oauth::form(input), want, "{name}");
	}
	assert_eq!(oauth::base64(b"hello"), "aGVsbG8=");
	let extra = BTreeMap::new();
	for basic in [true, false] {
		let (headers, body) = oauth::request(oauth::Input {
			id: "a:b",
			secret: "s +",
			basic,
			scope: Some("x y"),
			audience: None,
			resource: None,
			extra: &extra,
		})
		.unwrap();
		let body = String::from_utf8(body).unwrap();
		assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
		assert_eq!(
			headers[header::CONTENT_TYPE],
			"application/x-www-form-urlencoded"
		);
		if basic {
			assert_eq!(
				headers[header::AUTHORIZATION],
				format!("Basic {}", oauth::base64(b"a%3Ab:s+%2B"))
			);
			assert!(headers[header::AUTHORIZATION].is_sensitive());
		} else {
			assert!(body.contains("client_id=a%3Ab&client_secret=s+%2B"));
		}
	}
}
#[test]
fn parse_response_cases() {
	for (name, body, expected) in [
		(
			"number",
			r#"{"access_token":"abc","token_type":"Bearer","expires_in":42}"#,
			Some(42),
		),
		(
			"string",
			r#"{"access_token":"abc","token_type":"bearer","expires_in":"12"}"#,
			Some(12),
		),
		(
			"default",
			r#"{"access_token":"abc","token_type":"Bearer"}"#,
			Some(300),
		),
		("missing", r#"{"token_type":"Bearer"}"#, None),
		(
			"wrong type",
			r#"{"access_token":"abc","token_type":"MAC"}"#,
			None,
		),
		(
			"zero",
			r#"{"access_token":"abc","token_type":"Bearer","expires_in":0}"#,
			None,
		),
	] {
		assert_eq!(
			oauth::parse(200, body.as_bytes(), 1000, 300)
				.ok()
				.map(|x| x.1),
			expected,
			"{name}"
		);
	}
	assert!(oauth::parse(200, b"{}", 1, 300).is_err());
	assert!(oauth::parse(401, b"{}", 100, 300).is_err());
}
#[test]
fn cache_refresh_and_generation() {
	let c = cache::Cache::default();
	let now = Instant::now();
	assert!(matches!(c.lookup(now), cache::Lookup::Fetch));
	assert!(matches!(c.lookup(now), cache::Lookup::Wait));
	let first = c
		.complete(
			Some((
				"a".into(),
				Duration::from_secs(100),
				Duration::from_secs(60),
			)),
			now,
		)
		.unwrap();
	assert!(matches!(c.lookup(now), cache::Lookup::Token(_)));
	assert!(matches!(
		c.lookup(now + Duration::from_secs(50)),
		cache::Lookup::Fetch
	));
	assert!(matches!(
		c.lookup(now + Duration::from_secs(50)),
		cache::Lookup::Token(_)
	));
	let second = c
		.complete(
			Some((
				"b".into(),
				Duration::from_secs(100),
				Duration::from_secs(60),
			)),
			now,
		)
		.unwrap();
	c.invalidate(first.generation);
	assert!(matches!(c.lookup(now), cache::Lookup::Token(_)));
	c.invalidate(second.generation);
	assert!(matches!(c.lookup(now), cache::Lookup::Fetch));
}
struct Fake(bool);
#[async_trait]
impl Transport for Fake {
	async fn send(&self, _: Uri, _: http::HeaderMap, _: Vec<u8>, _: usize) -> Result<(u16, Vec<u8>)> {
		if self.0 {
			Ok((
				200,
				br#"{"access_token":"abc","token_type":"Bearer","expires_in":300}"#.to_vec(),
			))
		} else {
			Err(Error::msg("secret should never be logged"))
		}
	}
}
#[test]
fn fake_transport_fetches_and_reuses() {
	let p = plugin();
	let first = block_on(p.token(&Fake(true))).unwrap();
	assert_eq!(first.1, "fetched");
	let next = block_on(p.token(&Fake(false))).unwrap();
	assert_eq!(next.1, "cache");
}
#[test]
fn mock_request_replaces_and_invalidates() {
	let p = plugin();
	let t = p
		.cache
		.complete(
			Some((
				"abc".into(),
				Duration::from_secs(300),
				Duration::from_secs(60),
			)),
			Instant::now(),
		)
		.unwrap();
	let mut env = TestEnvironment::new();
	let req = TestRequest::get("/").header(
		header::AUTHORIZATION,
		HeaderValue::from_static("Bearer client"),
	);
	let action = block_on(env.run_request(&p, req)).unwrap();
	assert!(matches!(
		action,
		HttpRequestAction::Continue {
			response: ResponsePhase::Run(_)
		}
	));
	assert_eq!(env.request_headers()[header::AUTHORIZATION], "Bearer abc");
	assert_eq!(env.metadata()["decision"], "token");
	block_on(env.run_response(
		&p,
		t.generation,
		TestResponse::new(http::StatusCode::UNAUTHORIZED),
	))
	.unwrap();
	assert!(matches!(
		p.cache.lookup(Instant::now()),
		cache::Lookup::Fetch
	));
}
#[test]
fn failure_is_generic() {
	let mut value = config();
	value["errorStatus"] = json!(502);
	let p = Plugin::new(value).unwrap();
	let mut env = TestEnvironment::new();
	let action = block_on(env.run_request(&p, TestRequest::get("/"))).unwrap();
	match action {
		HttpRequestAction::DirectResponse {
			status,
			headers,
			body,
		} => {
			assert_eq!(status, 502);
			assert_eq!(headers.len(), 1);
			match body { agentgateway_sdk::DirectBody::Bytes(bytes) => assert_eq!(&bytes[..], br#"{"error":{"type":"backend_auth_unavailable","message":"could not obtain backend credentials"}}"#), _ => panic!("expected bytes") }
		},
		_ => panic!("expected error"),
	}
	assert_eq!(env.metadata()["decision"], "error");
	let credential = std::env::var("PATH").unwrap();
	assert!(!format!("{:?}", env.logs()).contains(&credential));
	assert!(!format!("{:?}", env.request_headers()).contains(&credential));
}

#[test]
fn concurrent_cache_misses_elect_one_fetcher() {
	use std::sync::{
		Arc, Barrier,
		atomic::{AtomicUsize, Ordering},
	};
	let cache = Arc::new(cache::Cache::default());
	let start = Arc::new(Barrier::new(9));
	let checked = Arc::new(Barrier::new(9));
	let fetches = Arc::new(AtomicUsize::new(0));
	let mut threads = Vec::new();
	for _ in 0..8 {
		let (cache, start, checked, fetches) = (
			cache.clone(),
			start.clone(),
			checked.clone(),
			fetches.clone(),
		);
		threads.push(std::thread::spawn(move || {
			start.wait();
			if matches!(cache.lookup(Instant::now()), cache::Lookup::Fetch) {
				fetches.fetch_add(1, Ordering::SeqCst);
			}
			checked.wait();
		}));
	}
	start.wait();
	checked.wait();
	for t in threads {
		t.join().unwrap();
	}
	assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

#[test]
fn short_lived_tokens_refresh_halfway_through_lifetime() {
	let cache = cache::Cache::default();
	let issued = Instant::now();
	assert!(matches!(cache.lookup(issued), cache::Lookup::Fetch));
	let token = cache
		.complete(
			Some((
				"short".into(),
				Duration::from_secs(5),
				Duration::from_secs(60),
			)),
			issued,
		)
		.unwrap();
	assert_eq!(
		token.refresh_at.duration_since(issued),
		Duration::from_millis(2500)
	);
	for millis in [0, 1, 1000, 2499] {
		assert!(
			matches!(
				cache.lookup(issued + Duration::from_millis(millis)),
				cache::Lookup::Token(_)
			),
			"{millis}ms"
		);
	}
	assert!(matches!(
		cache.lookup(issued + Duration::from_millis(2500)),
		cache::Lookup::Fetch
	));
	for _ in 0..10 {
		assert!(matches!(
			cache.lookup(issued + Duration::from_millis(2501)),
			cache::Lookup::Token(_)
		));
	}
	cache.complete(
		Some((
			"next".into(),
			Duration::from_secs(5),
			Duration::from_secs(60),
		)),
		issued + Duration::from_millis(2501),
	);
	assert!(matches!(
		cache.lookup(issued + Duration::from_millis(2502)),
		cache::Lookup::Token(_)
	));
}

#[test]
fn replace_incoming_removes_authorization_and_configured_header() {
	for (name, configured_header) in [
		("authorization", "authorization"),
		("custom", "x-backend-token"),
	] {
		let mut value = config();
		value["header"] = json!(configured_header);
		let plugin = Plugin::new(value).unwrap();
		plugin.cache.complete(
			Some((
				"backend".into(),
				Duration::from_secs(300),
				Duration::from_secs(60),
			)),
			Instant::now(),
		);
		let mut environment = TestEnvironment::new();
		let mut request = TestRequest::get("/").header(
			header::AUTHORIZATION,
			HeaderValue::from_static("Bearer incoming"),
		);
		if configured_header != "authorization" {
			request = request.header(
				HeaderName::from_static("x-backend-token"),
				HeaderValue::from_static("incoming custom"),
			);
		}
		block_on(environment.run_request(&plugin, request)).unwrap();
		let headers = environment.request_headers();
		assert_eq!(headers[configured_header], "Bearer backend", "{name}");
		if configured_header != "authorization" {
			assert!(!headers.contains_key(header::AUTHORIZATION), "{name}");
		}
	}
}

#[test]
fn response_401_invalidates_only_matching_token() {
	let plugin = plugin();
	let now = Instant::now();
	let old = plugin
		.cache
		.complete(
			Some((
				"old".into(),
				Duration::from_secs(300),
				Duration::from_secs(60),
			)),
			now,
		)
		.unwrap();
	let new = plugin
		.cache
		.complete(
			Some((
				"new".into(),
				Duration::from_secs(300),
				Duration::from_secs(60),
			)),
			now,
		)
		.unwrap();
	let mut environment = TestEnvironment::new();
	block_on(environment.run_response(
		&plugin,
		old.generation,
		TestResponse::new(http::StatusCode::UNAUTHORIZED),
	))
	.unwrap();
	assert_eq!(plugin.cache.valid(now).unwrap().generation, new.generation);
	block_on(environment.run_response(
		&plugin,
		new.generation,
		TestResponse::new(http::StatusCode::OK),
	))
	.unwrap();
	assert!(plugin.cache.valid(now).is_some());
	block_on(environment.run_response(
		&plugin,
		new.generation,
		TestResponse::new(http::StatusCode::UNAUTHORIZED),
	))
	.unwrap();
	assert!(plugin.cache.valid(now).is_none());
}

#[test]
fn oauth_error_codes_are_bounded_and_descriptions_ignored() {
	for (name, body, expected) in [
		(
			"standard",
			r#"{"error":"invalid_client","error_description":"secret"}"#,
			Some("invalid_client"),
		),
		("missing", r#"{"error_description":"secret"}"#, None),
		("unsafe", "{\"error\":\"bad\\nsecret\"}", None),
		("wrong type", r#"{"error":1}"#, None),
	] {
		assert_eq!(
			oauth::error_code(body.as_bytes()).as_deref(),
			expected,
			"{name}"
		);
	}
}

struct ErrorTransport;

#[async_trait]
impl Transport for ErrorTransport {
	async fn send(&self, _: Uri, _: http::HeaderMap, _: Vec<u8>, _: usize) -> Result<(u16, Vec<u8>)> {
		Ok((
			401,
			br#"{"error":"invalid_client","error_description":"private-id private-secret"}"#.to_vec(),
		))
	}
}

struct ErrorPlugin(Plugin);

#[async_trait]
impl HttpPlugin for ErrorPlugin {
	type ResponseState = u64;

	async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<u64>> {
		assert!(self.0.fetch(&ErrorTransport).await.is_err());
		request.set_metadata("decision", "error")?;
		Ok(error_response(self.0.status))
	}
}

#[test]
fn oauth_failure_logs_code_without_credentials_or_description() {
	let mut plugin = plugin();
	plugin.id = Secret("private-id".into());
	plugin.secret = Secret("private-secret".into());
	let mut environment = TestEnvironment::new();
	let action =
		block_on(environment.run_request(&ErrorPlugin(plugin), TestRequest::get("/"))).unwrap();
	let logs = format!("{:?}", environment.logs());
	assert!(logs.contains("invalid_client"));
	assert!(!logs.contains("private-id"));
	assert!(!logs.contains("private-secret"));
	match action {
		HttpRequestAction::DirectResponse {
			status,
			headers,
			body,
		} => {
			assert_eq!(status, 503);
			assert!(!format!("{headers:?}").contains("private-secret"));
			match body {
				agentgateway_sdk::DirectBody::Bytes(bytes) => assert_eq!(&bytes[..], ERROR_BODY),
				_ => panic!("expected buffered body"),
			}
		},
		_ => panic!("expected direct response"),
	}
}
