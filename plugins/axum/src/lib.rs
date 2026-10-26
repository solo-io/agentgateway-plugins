use agentgateway_sdk::{
	DirectBody, Header, HttpPlugin, HttpPluginFactory, HttpRequestAction, RequestContext, Result,
	async_trait, export_plugin,
};
use axum::body::{Body, Bytes};
use axum::http;
use axum::routing::{get, post};
use axum::{Router, response::IntoResponse};
use tower::ServiceExt;

struct AxumPlugin {
	router: Router,
}

#[async_trait]
impl HttpPlugin for AxumPlugin {
	type ResponseState = ();

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		let request = request.to_http_request()?.map(Body::new);
		let response = self
			.router
			.clone()
			.oneshot(request)
			.await
			.map_err(agentgateway_sdk::Error::new)?;
		let (parts, body) = response.into_parts();
		let headers = parts
			.headers
			.iter()
			.map(|(name, value)| Header::new(name.clone(), value.clone()))
			.collect();
		Ok(HttpRequestAction::DirectResponse {
			status: parts.status.as_u16(),
			headers,
			body: DirectBody::body(body),
		})
	}
}

impl HttpPluginFactory for AxumPlugin {
	fn new(_config: serde_json::Value) -> Result<Self> {
		Ok(Self {
			router: Router::new()
				.route("/", get(root))
				.route("/echo", post(echo))
				.route("/headers", get(headers)),
		})
	}
}

async fn root() -> &'static str {
	"hello from axum\n"
}

async fn echo(body: Bytes) -> impl IntoResponse {
	body
}

async fn headers(headers: http::HeaderMap) -> String {
	let host = headers
		.get(http::header::HOST)
		.and_then(|value| value.to_str().ok())
		.unwrap_or("");
	format!("host={host}\nheaders={}\n", headers.len())
}

export_plugin!(AxumPlugin, name = "axum", factory);

#[cfg(test)]
mod tests;
