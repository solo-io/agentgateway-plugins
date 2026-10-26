//! A tour of the agentgateway plugin SDK.
//!
//! Every request gets metadata attached and an optional CEL expression
//! evaluated, then is dispatched on its path:
//!
//! | Path        | Shows                                                    |
//! | ----------- | -------------------------------------------------------- |
//! | `/direct`   | Returning a buffered direct response.                    |
//! | `/echo`     | Streaming the request body back as a direct response.    |
//! | `/sleep`    | Sleeping on the host runtime, then continuing upstream.  |
//! | `/callout`  | Making an HTTP call through the host, then continuing.   |
//! | anything    | Continuing upstream.                                     |
//!
//! Requests that continue upstream also run the response phase, which reads the
//! upstream status and reports how long the request took.

use std::time::{Duration, Instant};

use agentgateway_sdk::{
	CelExpression, DirectBody, Header, HttpPlugin, HttpPluginFactory, HttpRequest, HttpRequestAction,
	LogLevel, RequestContext, ResponseAction, ResponseContext, ResponsePhase, Result, async_trait,
	export_plugin, http_call, log, sleep,
};
use bytes::Bytes;
use futures_util::stream;
use http::{HeaderName, HeaderValue, Uri};
use http_body::Frame;
use http_body_util::StreamBody;
use serde::{Deserialize, Serialize};

const ACTION_HEADER: HeaderName = HeaderName::from_static("x-tour-action");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
	/// Absolute URL called by `/callout`, e.g. `http://127.0.0.1:8080/`.
	callout_url: String,
	/// Optional CEL expression evaluated on every request. The result is
	/// returned in the `x-tour-cel` response header.
	cel: Option<String>,
}

struct SdkTour {
	callout_url: Uri,
	// Compiled lazily on first use, then reused across requests.
	cel: Option<CelExpression>,
}

impl HttpPluginFactory for SdkTour {
	fn new(config: serde_json::Value) -> Result<Self> {
		let config: Config = serde_json::from_value(config)?;
		let callout_url: Uri = config.callout_url.parse()?;
		if callout_url.scheme().is_none() || callout_url.authority().is_none() {
			return Err(agentgateway_sdk::Error::msg(format!(
				"calloutUrl must be an absolute URL, got {callout_url}"
			)));
		}
		Ok(Self {
			callout_url,
			cel: config.cel.map(CelExpression::owned),
		})
	}
}

/// State carried from the request phase into the response phase.
struct ResponseState {
	action: &'static str,
	started: Instant,
	cel_result: Option<serde_json::Value>,
}

#[async_trait]
impl HttpPlugin for SdkTour {
	type ResponseState = ResponseState;

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		let started = Instant::now();

		// Metadata is visible to the rest of the gateway via CEL, e.g. in access
		// logs, under the configured plugin name.
		request.set_metadata("longestHeader", &longest_header(&request))?;
		let cel_result = self
			.cel
			.as_ref()
			.map(|cel| cel.evaluate(&request)?.to_json())
			.transpose()?;

		let path = request.path();
		let action = match path.as_ref().map(|p| p.path()).unwrap_or("/") {
			"/direct" => return Ok(text_response("direct", "hello from the sdk-tour plugin\n")),
			"/echo" => {
				return Ok(text_response("echo", echo_body(&request)));
			},
			"/sleep" => {
				sleep(Duration::from_millis(100)).await?;
				"sleep"
			},
			"/callout" => {
				log(LogLevel::Info, format!("calling {}", self.callout_url));
				let response = http_call(HttpRequest::get(self.callout_url.clone()))?.await?;
				// Tell the upstream what the callout returned.
				request.set_header(
					HeaderName::from_static("x-tour-callout-status"),
					HeaderValue::from(response.status),
				)?;
				"callout"
			},
			_ => "continue",
		};

		request.set_header(ACTION_HEADER, HeaderValue::from_static(action))?;
		Ok(HttpRequestAction::Continue {
			response: ResponsePhase::Run(ResponseState {
				action,
				started,
				cel_result,
			}),
		})
	}

	async fn on_response(
		&self,
		state: Self::ResponseState,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		let upstream_status = response.status().map(|s| s.as_u16()).unwrap_or_default();
		response.set_header(ACTION_HEADER, HeaderValue::from_static(state.action))?;
		response.set_header(
			HeaderName::from_static("x-tour-upstream-status"),
			HeaderValue::from(upstream_status),
		)?;
		response.set_header(
			HeaderName::from_static("x-tour-elapsed-ms"),
			HeaderValue::from(state.started.elapsed().as_millis() as u64),
		)?;
		if let Some(cel_result) = state.cel_result {
			response.set_header(
				HeaderName::from_static("x-tour-cel"),
				HeaderValue::from_str(&cel_result.to_string())?,
			)?;
		}
		Ok(ResponseAction::Continue)
	}
}

/// Builds a `200 text/plain` direct response. The upstream is never called.
fn text_response(
	action: &'static str,
	body: impl Into<DirectBody>,
) -> HttpRequestAction<ResponseState> {
	HttpRequestAction::DirectResponse {
		status: 200,
		headers: vec![
			Header::new(
				http::header::CONTENT_TYPE,
				HeaderValue::from_static("text/plain"),
			),
			Header::new(ACTION_HEADER, HeaderValue::from_static(action)),
		],
		body: body.into(),
	}
}

/// Streams the request body back, prefixing each chunk with its index. Chunks
/// are forwarded as they arrive, so nothing is buffered in the plugin.
fn echo_body(request: &RequestContext) -> DirectBody {
	let chunks = stream::try_unfold((request.body(), 0usize), |(mut body, index)| async move {
		let Some(chunk) = body.next_chunk().await? else {
			return Ok::<_, agentgateway_sdk::Error>(None);
		};
		let mut framed = format!("chunk {index}: ").into_bytes();
		framed.extend_from_slice(&chunk);
		framed.push(b'\n');
		Ok(Some((Frame::data(Bytes::from(framed)), (body, index + 1))))
	});
	DirectBody::body(StreamBody::new(chunks))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LongestHeader {
	name: String,
	length: usize,
}

fn longest_header(request: &RequestContext) -> Option<LongestHeader> {
	request
		.headers()
		.map(|(name, value)| LongestHeader {
			name: name.to_string(),
			length: value.len(),
		})
		.max_by_key(|header| header.length)
}

export_plugin!(SdkTour, name = "sdk-tour", factory);

#[cfg(test)]
mod tests;
