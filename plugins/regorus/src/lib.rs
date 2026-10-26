use agentgateway_sdk::{
	Header, HttpPlugin, HttpPluginFactory, HttpRequestAction, RequestContext, ResponseAction,
	ResponseContext, ResponsePhase, Result, async_trait, export_plugin,
};
use http::{HeaderName, HeaderValue};
use regorus::{Engine, Value};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
	policy: String,
	#[serde(default = "default_rule")]
	rule: String,
	result_header: String,
}

fn default_rule() -> String {
	"data.agentgateway.result".to_string()
}

#[derive(Clone)]
struct CompiledRego {
	engine: Engine,
	rule: String,
}

impl std::fmt::Debug for CompiledRego {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CompiledRego")
			.field("rule", &self.rule)
			.finish_non_exhaustive()
	}
}

impl CompiledRego {
	fn new(policy: String, rule: String) -> Result<Self> {
		let mut engine = Engine::new();
		engine.add_policy("agentgateway.rego".to_string(), policy)?;
		Ok(Self { engine, rule })
	}

	fn eval(&self, input: serde_json::Value) -> Result<serde_json::Value> {
		let mut engine = self.engine.clone();
		engine.set_input(Value::from(input));
		let value = engine.eval_rule(self.rule.clone())?;
		Ok(serde_json::to_value(value)?)
	}
}

#[derive(Debug)]
struct RegoPlugin {
	result_header: HeaderName,
	rego: CompiledRego,
}

#[async_trait]
impl HttpPlugin for RegoPlugin {
	/// The policy result, written into the result header on the response.
	type ResponseState = HeaderValue;

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		let method = request
			.method()
			.map(|method| method.to_string())
			.unwrap_or_default();
		let path = request.path();
		let input = serde_json::json!({
			"method": method,
			"path": path.as_ref().map(|path| path.path()).unwrap_or("/"),
			"query": path.as_ref().and_then(|path| path.query()),
			"headers": {
				"x-user": request
					.header(&HeaderName::from_static("x-user"))
					.and_then(|value| value.to_str().ok().map(str::to_string)),
				"x-tenant": request
					.header(&HeaderName::from_static("x-tenant"))
					.and_then(|value| value.to_str().ok().map(str::to_string)),
			},
		});
		let result = self.rego.eval(input)?;

		match result {
			serde_json::Value::Bool(false) => Ok(HttpRequestAction::DirectResponse {
				status: 403,
				headers: vec![Header::new(
					self.result_header.clone(),
					HeaderValue::from_static("deny"),
				)],
				body: "denied by rego plugin\n".into(),
			}),
			value => {
				let value = value
					.as_str()
					.map(str::to_string)
					.unwrap_or_else(|| value.to_string());
				Ok(HttpRequestAction::Continue {
					response: ResponsePhase::Run(HeaderValue::from_bytes(value.as_bytes())?),
				})
			},
		}
	}

	async fn on_response(
		&self,
		result: Self::ResponseState,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		response.set_header(self.result_header.clone(), result)?;
		Ok(ResponseAction::Continue)
	}
}

impl HttpPluginFactory for RegoPlugin {
	fn new(config: serde_json::Value) -> Result<Self> {
		let config: Config = serde_json::from_value(config)?;
		Ok(Self {
			result_header: HeaderName::try_from(config.result_header)?,
			rego: CompiledRego::new(config.policy, config.rule)?,
		})
	}
}

export_plugin!(RegoPlugin, name = "regorus", factory);

#[cfg(test)]
mod tests;
