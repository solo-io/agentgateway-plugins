mod cache;
mod oauth;
use agentgateway_sdk::{
	Error, Header, HttpPlugin, HttpPluginFactory, HttpRequest, HttpRequestAction, LogLevel,
	RequestContext, ResponseAction, ResponseContext, ResponsePhase, Result, async_trait,
	export_plugin, log,
};
use http::{HeaderName, HeaderValue, Method, Uri, header};
use serde::Deserialize;
use std::{
	collections::BTreeMap,
	future::{Future, poll_fn},
	task::Poll,
	time::{Duration, Instant},
};

const ERROR_BODY: &[u8] = br#"{"error":{"type":"backend_auth_unavailable","message":"could not obtain backend credentials"}}"#;

fn error_response(status: u16) -> HttpRequestAction<u64> {
	HttpRequestAction::DirectResponse {
		status,
		headers: vec![Header::new(
			header::CONTENT_TYPE,
			HeaderValue::from_static("application/json"),
		)],
		body: ERROR_BODY.to_vec().into(),
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
	token_url: String,
	client_id_env: String,
	client_secret_env: String,
	#[serde(default)]
	client_auth: Auth,
	scope: Option<String>,
	audience: Option<String>,
	resource: Option<String>,
	#[serde(default)]
	extra_params: BTreeMap<String, String>,
	#[serde(default = "refresh_default")]
	refresh_before_seconds: u64,
	#[serde(default = "ttl_default")]
	default_ttl_seconds: u64,
	#[serde(default = "timeout_default")]
	timeout_ms: u64,
	#[serde(default = "max_default")]
	max_response_bytes: usize,
	#[serde(default = "header_default")]
	header: String,
	#[serde(default = "scheme_default")]
	scheme: String,
	#[serde(default = "yes")]
	replace_incoming: bool,
	#[serde(default = "status_default")]
	error_status: u16,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Auth {
	#[default]
	Basic,
	Post,
}

fn refresh_default() -> u64 {
	60
}

fn ttl_default() -> u64 {
	300
}

fn timeout_default() -> u64 {
	2000
}

fn max_default() -> usize {
	65536
}

fn header_default() -> String {
	"authorization".into()
}

fn scheme_default() -> String {
	"Bearer".into()
}

fn yes() -> bool {
	true
}

fn status_default() -> u16 {
	503
}

struct Secret(String);
impl std::fmt::Debug for Secret {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("<redacted>")
	}
}

struct Plugin {
	url: Uri,
	id: Secret,
	secret: Secret,
	basic: bool,
	scope: Option<String>,
	audience: Option<String>,
	resource: Option<String>,
	extra: BTreeMap<String, String>,
	refresh: u64,
	ttl: u64,
	timeout: Duration,
	max: usize,
	header: HeaderName,
	scheme: String,
	replace: bool,
	status: u16,
	cache: cache::Cache,
}

impl std::fmt::Debug for Plugin {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Plugin")
			.field("id", &self.id)
			.field("secret", &self.secret)
			.finish_non_exhaustive()
	}
}

impl HttpPluginFactory for Plugin {
	fn new(value: serde_json::Value) -> Result<Self> {
		let config: Config = serde_json::from_value(value)?;
		let url: Uri = config
			.token_url
			.parse()
			.map_err(|_| Error::msg("tokenUrl: invalid URL"))?;
		if !matches!(url.scheme_str(), Some("http" | "https"))
			|| url.authority().is_none()
			|| url
				.authority()
				.is_some_and(|authority| authority.as_str().contains('@'))
		{
			return Err(Error::msg(
				"tokenUrl: expected absolute http or https URL without userinfo",
			));
		}
		if url.scheme_str() == Some("http")
			&& !url.host().is_some_and(|host| {
				host.eq_ignore_ascii_case("localhost")
					|| host
						.trim_matches(['[', ']'])
						.parse::<std::net::IpAddr>()
						.is_ok_and(|ip| ip.is_loopback())
			}) {
			log(
				LogLevel::Warn,
				"oauth-client-credentials: tokenUrl uses cleartext HTTP to a non-loopback host",
			);
		}
		let env = |name: &str, field: &str| -> Result<Secret> {
			if name.is_empty() {
				return Err(Error::msg(format!(
					"{field}: environment variable name is empty"
				)));
			}
			std::env::var(name)
				.ok()
				.filter(|value| !value.is_empty())
				.map(Secret)
				.ok_or_else(|| {
					Error::msg(format!(
						"{field}: environment variable {name} is missing or empty"
					))
				})
		};
		let id = env(&config.client_id_env, "clientIdEnv")?;
		let secret = env(&config.client_secret_env, "clientSecretEnv")?;
		if config
			.extra_params
			.keys()
			.any(|key| matches!(key.as_str(), "grant_type" | "client_id" | "client_secret"))
		{
			return Err(Error::msg("extraParams: reserved field"));
		}
		if config.timeout_ms == 0 {
			return Err(Error::msg("timeoutMs must be greater than zero"));
		}
		if config.max_response_bytes == 0 {
			return Err(Error::msg("maxResponseBytes must be greater than zero"));
		}
		if config.default_ttl_seconds == 0 {
			return Err(Error::msg("defaultTtlSeconds must be greater than zero"));
		}
		if !(400..=599).contains(&config.error_status) {
			return Err(Error::msg("errorStatus must be 400-599"));
		}
		let header =
			HeaderName::try_from(config.header).map_err(|_| Error::msg("header: invalid header name"))?;
		if config.scheme.is_empty()
			|| !config
				.scheme
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
		{
			return Err(Error::msg("scheme: invalid authentication scheme"));
		}
		Ok(Self {
			url,
			id,
			secret,
			basic: matches!(config.client_auth, Auth::Basic),
			scope: config.scope,
			audience: config.audience,
			resource: config.resource,
			extra: config.extra_params,
			refresh: config.refresh_before_seconds,
			ttl: config.default_ttl_seconds,
			timeout: Duration::from_millis(config.timeout_ms),
			max: config.max_response_bytes,
			header,
			scheme: config.scheme,
			replace: config.replace_incoming,
			status: config.error_status,
			cache: cache::Cache::default(),
		})
	}
}

#[async_trait]
trait Transport {
	async fn send(
		&self,
		url: Uri,
		headers: http::HeaderMap,
		body: Vec<u8>,
		max: usize,
	) -> Result<(u16, Vec<u8>)>;
}

struct Host<'a>(&'a RequestContext);
#[async_trait]
impl Transport for Host<'_> {
	async fn send(
		&self,
		url: Uri,
		headers: http::HeaderMap,
		body: Vec<u8>,
		max: usize,
	) -> Result<(u16, Vec<u8>)> {
		let mut call = HttpRequest::new(Method::POST, url);
		call.headers = headers;
		call.body = body.into();
		let response = self.0.http_call(call)?.await?;
		Ok((response.status, response.body.collect(max).await?))
	}
}
async fn race<F: Future>(future: F, timeout: Duration) -> Option<F::Output> {
	let mut future = std::pin::pin!(future);
	let mut timer = std::pin::pin!(agentgateway_sdk::sleep(timeout));
	poll_fn(|cx| {
		if let Poll::Ready(value) = future.as_mut().poll(cx) {
			return Poll::Ready(Some(value));
		}
		if timer.as_mut().poll(cx).is_ready() {
			return Poll::Ready(None);
		}
		Poll::Pending
	})
	.await
}

impl Plugin {
	async fn fetch(
		&self,
		transport: &impl Transport,
	) -> std::result::Result<(String, u64), &'static str> {
		let (headers, body) = oauth::request(oauth::Input {
			id: &self.id.0,
			secret: &self.secret.0,
			basic: self.basic,
			scope: self.scope.as_deref(),
			audience: self.audience.as_deref(),
			resource: self.resource.as_deref(),
			extra: &self.extra,
		})
		.map_err(|_| "cannot construct token request")?;
		let response = race(
			transport.send(self.url.clone(), headers, body, self.max),
			self.timeout,
		)
		.await
		.ok_or("token request timed out")?
		.map_err(|_| "token request failed")?;
		if !(200..300).contains(&response.0) {
			let error_code = oauth::error_code(&response.1)
				.filter(|code| !code.contains(&self.id.0) && !code.contains(&self.secret.0));
			if let Some(code) = error_code {
				agentgateway_sdk::log!(
					LogLevel::Warn,
					"oauth-client-credentials: token endpoint returned status {} error {}",
					response.0,
					code
				);
			} else {
				agentgateway_sdk::log!(
					LogLevel::Warn,
					"oauth-client-credentials: token endpoint returned status {}",
					response.0
				);
			}
		}
		oauth::parse(response.0, &response.1, self.max, self.ttl)
	}
	async fn token(
		&self,
		transport: &impl Transport,
	) -> std::result::Result<(cache::Token, &'static str), &'static str> {
		match self.cache.lookup(Instant::now()) {
			cache::Lookup::Token(token) => Ok((token, "cache")),
			cache::Lookup::Fetch => {
				let mut flight = cache::Flight(Some(&self.cache));
				let result = self.fetch(transport).await;
				flight.0 = None;
				match result {
					Ok((value, ttl)) => self
						.cache
						.complete(
							Some((
								value,
								Duration::from_secs(ttl),
								Duration::from_secs(self.refresh),
							)),
							Instant::now(),
						)
						.map(|token| (token, "fetched"))
						.ok_or("invalid expiry"),
					Err(error) => {
						self.cache.complete(None, Instant::now());
						Err(error)
					},
				}
			},
			cache::Lookup::Wait => {
				race(poll_fn(|cx| self.cache.poll_wait(cx.waker())), self.timeout)
					.await
					.ok_or("waiting for token timed out")?;
				self
					.cache
					.valid(Instant::now())
					.map(|token| (token, "cache"))
					.ok_or("token fetch failed")
			},
		}
	}
}

#[async_trait]
impl HttpPlugin for Plugin {
	type ResponseState = u64;
	async fn on_request(&self, request: RequestContext) -> Result<HttpRequestAction<u64>> {
		let token = self.token(&Host(&request)).await;
		if let Ok((token, source)) = token {
			let value = HeaderValue::from_str(&format!("{} {}", self.scheme, token.value));
			if let Ok(mut value) = value {
				value.set_sensitive(true);
				if self.replace {
					request.remove_header(header::AUTHORIZATION)?;
					if self.header != header::AUTHORIZATION {
						request.remove_header(self.header.clone())?;
					}
				}
				request.set_header(self.header.clone(), value)?;
				request.set_metadata("decision", "token")?;
				request.set_metadata("tokenSource", source)?;
				request.set_metadata(
					"expiresInSeconds",
					&token
						.expires
						.saturating_duration_since(Instant::now())
						.as_secs(),
				)?;
				return Ok(HttpRequestAction::Continue {
					response: ResponsePhase::Run(token.generation),
				});
			}
		} else if let Err(error) = token {
			agentgateway_sdk::log!(LogLevel::Warn, "oauth-client-credentials: {}", error);
		}
		request.set_metadata("decision", "error")?;
		Ok(error_response(self.status))
	}
	async fn on_response(
		&self,
		generation: u64,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		if response.status() == Some(http::StatusCode::UNAUTHORIZED) {
			self.cache.invalidate(generation);
		}
		Ok(ResponseAction::Continue)
	}
}

export_plugin!(Plugin, name = "oauth-client-credentials", factory);

#[cfg(test)]
mod tests;
