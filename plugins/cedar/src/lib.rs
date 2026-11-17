//! Authorizes requests with [Cedar](https://www.cedarpolicy.com/) policies.
//! README.md describes how a request maps to a Cedar principal, action,
//! resource and context.

mod engine;

use std::collections::{BTreeMap, HashMap, HashSet};

use agentgateway_sdk::{
	CelKind, CelValue, Error, Header, HttpPlugin, HttpPluginFactory, HttpRequestAction, LogLevel,
	RequestContext, ResponseAction, ResponseContext, ResponsePhase, Result, async_trait,
	export_plugin, log,
};
use bytes::Bytes;
use cedar_policy::{EntityId, EntityTypeName, EntityUid, RestrictedExpression};
use engine::{AuthzInput, Engine};
use http::header::CONTENT_TYPE;
use http::{HeaderName, HeaderValue};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
	policies: String,
	schema: Option<String>,
	entities: Option<Value>,
	principal: PrincipalConfig,
	#[serde(default = "default_action")]
	action: EntityConfig,
	#[serde(default = "default_resource")]
	resource: EntityConfig,
	#[serde(default)]
	context: ContextConfig,
	#[serde(default)]
	mode: Mode,
	#[serde(default)]
	mcp_tool_calls_only: bool,
	#[serde(default = "default_deny_status")]
	deny_status: u16,
	#[serde(default = "default_error_status")]
	error_status: u16,
	decision_header: Option<String>,
	#[serde(default)]
	expose_reasons: bool,
}

#[derive(Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Mode {
	#[default]
	Enforce,
	/// Log and record the decision, but let every request through.
	Audit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum SourceConfig {
	Header(String),
	Property(String),
	Literal(String),
	Method,
	Path,
	Host,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "SourceConfig")]
enum Source {
	Header(HeaderName),
	/// A CEL property path such as `jwt.sub`, split into segments.
	Property(Vec<String>),
	Literal(String),
	Method,
	/// The request path, without the query string.
	Path,
	Host,
}

impl TryFrom<SourceConfig> for Source {
	type Error = String;

	fn try_from(config: SourceConfig) -> std::result::Result<Self, String> {
		Ok(match config {
			SourceConfig::Header(name) => Source::Header(
				HeaderName::try_from(name.as_str()).map_err(|e| format!("header {name:?}: {e}"))?,
			),
			SourceConfig::Property(path) => Source::Property(property_path(&path)?),
			SourceConfig::Literal(value) => Source::Literal(value),
			SourceConfig::Method => Source::Method,
			SourceConfig::Path => Source::Path,
			SourceConfig::Host => Source::Host,
		})
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrincipalConfig {
	#[serde(
		rename = "type",
		default = "default_principal_type",
		deserialize_with = "entity_type"
	)]
	entity_type: EntityTypeName,
	id: Source,
	/// Must be a property whose value is a map, such as `jwt`.
	attributes: Option<Source>,
	parents: Option<ParentsConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ParentsConfig {
	#[serde(rename = "type", deserialize_with = "entity_type")]
	entity_type: EntityTypeName,
	from: Source,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntityConfig {
	#[serde(rename = "type", deserialize_with = "entity_type")]
	entity_type: EntityTypeName,
	id: Source,
	default: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextConfig {
	#[serde(default)]
	headers: Vec<String>,
	/// Context field name to CEL property path.
	#[serde(default)]
	properties: BTreeMap<String, String>,
}

fn entity_type<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<EntityTypeName, D::Error> {
	let name = String::deserialize(d)?;
	name
		.parse()
		.map_err(|e| D::Error::custom(format!("invalid cedar entity type {name:?}: {e}")))
}

fn type_name(name: &str) -> EntityTypeName {
	name.parse().expect("valid built-in entity type")
}

fn default_principal_type() -> EntityTypeName {
	type_name("User")
}
fn default_action() -> EntityConfig {
	EntityConfig {
		entity_type: type_name("Action"),
		id: Source::Method,
		default: None,
	}
}
fn default_resource() -> EntityConfig {
	EntityConfig {
		entity_type: type_name("Route"),
		id: Source::Path,
		default: None,
	}
}
fn default_deny_status() -> u16 {
	403
}
fn default_error_status() -> u16 {
	503
}

fn property_path(path: &str) -> std::result::Result<Vec<String>, String> {
	let segments: Vec<String> = path.split('.').map(str::to_string).collect();
	if segments.iter().any(String::is_empty) {
		return Err(format!("invalid property path {path:?}"));
	}
	Ok(segments)
}

struct CedarPlugin {
	engine: Engine,
	principal: PrincipalConfig,
	action: EntityConfig,
	resource: EntityConfig,
	context_headers: Vec<HeaderName>,
	context_properties: Vec<(String, Vec<String>)>,
	mode: Mode,
	mcp_tool_calls_only: bool,
	deny_status: u16,
	error_status: u16,
	decision_header: Option<HeaderName>,
	expose_reasons: bool,
	deny_body: Bytes,
	error_body: Bytes,
}

impl HttpPluginFactory for CedarPlugin {
	fn new(config: Value) -> Result<Self> {
		let config: Config = serde_json::from_value(config)?;
		for (field, status) in [
			("denyStatus", config.deny_status),
			("errorStatus", config.error_status),
		] {
			if !(400..=599).contains(&status) {
				return Err(Error::msg(format!("{field} must be 400-599, got {status}")));
			}
		}
		if matches!(&config.principal.attributes, Some(s) if !matches!(s, Source::Property(_))) {
			return Err(Error::msg(
				"principal.attributes must be a property, such as { property: jwt }",
			));
		}
		let mut types = vec![&config.principal.entity_type, &config.resource.entity_type];
		if let Some(parents) = &config.principal.parents {
			types.push(&parents.entity_type);
		}
		let engine = Engine::new(
			&config.policies,
			config.schema.as_deref(),
			config.entities,
			&config.principal.entity_type,
			&types,
		)?;
		Ok(Self {
			engine,
			principal: config.principal,
			action: config.action,
			resource: config.resource,
			context_headers: config
				.context
				.headers
				.iter()
				.map(|h| HeaderName::try_from(h.as_str()))
				.collect::<std::result::Result<_, _>>()?,
			context_properties: config
				.context
				.properties
				.into_iter()
				.map(|(name, path)| Ok((name, property_path(&path).map_err(Error::msg)?)))
				.collect::<Result<_>>()?,
			mode: config.mode,
			mcp_tool_calls_only: config.mcp_tool_calls_only,
			deny_status: config.deny_status,
			error_status: config.error_status,
			decision_header: config
				.decision_header
				.map(HeaderName::try_from)
				.transpose()?,
			expose_reasons: config.expose_reasons,
			deny_body: error_body(
				"authorization_denied",
				"request denied by cedar policy",
				None,
			)?,
			error_body: error_body("authorization_error", "authorization failed", None)?,
		})
	}
}

fn error_body(kind: &str, message: &str, policies: Option<&[String]>) -> Result<Bytes> {
	let mut error = serde_json::json!({ "type": kind, "message": message });
	if let Some(policies) = policies {
		error["policies"] = serde_json::json!(policies);
	}
	Ok(serde_json::to_vec(&serde_json::json!({ "error": error }))?.into())
}

#[async_trait]
impl HttpPlugin for CedarPlugin {
	/// The decision, written into the decision header on the response.
	type ResponseState = &'static str;

	async fn on_request(
		&self,
		request: RequestContext,
	) -> Result<HttpRequestAction<Self::ResponseState>> {
		// Only MCP tools/call is authorized; other MCP methods (initialize,
		// tools/list, ...) pass. Requests that aren't MCP have no method.
		if self.mcp_tool_calls_only
			&& let Some(method) = request.property(&["mcp", "methodName"])?
			&& method.as_str() != Some("tools/call")
		{
			request.set_metadata("decision", "skip")?;
			return Ok(HttpRequestAction::Continue {
				response: ResponsePhase::Skip,
			});
		}

		let (decision, policies) = match self
			.input(&request)
			.and_then(|input| input.map(|i| self.engine.authorize(i)).transpose())
		{
			Ok(Some(outcome)) => {
				for error in &outcome.errors {
					log(LogLevel::Warn, format!("cedar: policy error: {error}"));
				}
				let decision = if outcome.allowed { "allow" } else { "deny" };
				(decision, outcome.policies)
			},
			Ok(None) => ("deny", vec![]),
			Err(error) => {
				log(
					LogLevel::Warn,
					format!("cedar: cannot evaluate request: {error:#}"),
				);
				("error", vec![])
			},
		};
		request.set_metadata("decision", decision)?;
		request.set_metadata("policies", &policies)?;

		if decision == "allow" || self.mode == Mode::Audit {
			if decision != "allow" {
				log(
					LogLevel::Info,
					format!("cedar: audit mode, would {decision} (policies: {policies:?})"),
				);
			}
			return Ok(HttpRequestAction::Continue {
				response: match self.decision_header {
					Some(_) => ResponsePhase::Run(decision),
					None => ResponsePhase::Skip,
				},
			});
		}

		let (status, body) = match decision {
			"error" => (self.error_status, self.error_body.clone()),
			_ if self.expose_reasons => (
				self.deny_status,
				error_body(
					"authorization_denied",
					"request denied by cedar policy",
					Some(&policies),
				)?,
			),
			_ => (self.deny_status, self.deny_body.clone()),
		};
		let mut headers = vec![Header::new(
			CONTENT_TYPE,
			HeaderValue::from_static("application/json"),
		)];
		if let Some(name) = &self.decision_header {
			headers.push(Header::new(
				name.clone(),
				HeaderValue::from_static(decision),
			));
		}
		Ok(HttpRequestAction::DirectResponse {
			status,
			headers,
			body: body.into(),
		})
	}

	async fn on_response(
		&self,
		decision: Self::ResponseState,
		response: ResponseContext,
	) -> Result<ResponseAction> {
		if let Some(name) = &self.decision_header {
			response.set_header(name.clone(), HeaderValue::from_static(decision))?;
		}
		Ok(ResponseAction::Continue)
	}
}

/// Request fields read once, since each read crosses into the host.
struct RequestInfo {
	method: String,
	path: String,
	query: Option<String>,
	host: Option<String>,
}

impl RequestInfo {
	fn read(request: &RequestContext) -> Self {
		let path = request.path();
		Self {
			method: request.method().map(|m| m.to_string()).unwrap_or_default(),
			path: path.as_ref().map_or("/", |p| p.path()).to_string(),
			query: path.as_ref().and_then(|p| p.query()).map(str::to_string),
			host: request.authority().map(|a| a.to_string()).or_else(|| {
				request
					.header(&http::header::HOST)
					.and_then(|h| h.to_str().ok().map(str::to_string))
			}),
		}
	}
}

impl CedarPlugin {
	/// `None` when the action or resource id is missing and has no default.
	fn input(&self, request: &RequestContext) -> Result<Option<AuthzInput>> {
		let info = RequestInfo::read(request);
		let (Some(action), Some(resource)) = (
			entity_uid(request, &info, &self.action, "action")?,
			entity_uid(request, &info, &self.resource, "resource")?,
		) else {
			return Ok(None);
		};

		// A request with no principal id runs as `<type>::""`, which no real
		// identity matches because empty ids are treated as missing.
		let id = self
			.principal
			.id
			.string(request, &info)?
			.unwrap_or_default();
		let mut attrs = HashMap::new();
		if let Some(Source::Property(path)) = &self.principal.attributes
			&& let Some(value) = property(request, path)?
		{
			for (key, value) in value.entries() {
				if let Some(key) = key.as_str().filter(|k| self.engine.keeps_attr(k))
					&& let Some(value) = to_cedar(value)
				{
					attrs.insert(key.to_string(), value);
				}
			}
		}
		let parents = match &self.principal.parents {
			Some(parents) => parents
				.from
				.list(request, &info)?
				.into_iter()
				.map(|id| uid(&parents.entity_type, &id))
				.collect(),
			None => HashSet::new(),
		};

		Ok(Some(AuthzInput {
			principal: uid(&self.principal.entity_type, &id),
			attrs,
			parents,
			action,
			resource,
			context: self.context(request, info)?,
		}))
	}

	fn context(
		&self,
		request: &RequestContext,
		info: RequestInfo,
	) -> Result<Vec<(String, RestrictedExpression)>> {
		let mut context = vec![
			(
				"method".into(),
				RestrictedExpression::new_string(info.method),
			),
			("path".into(), RestrictedExpression::new_string(info.path)),
		];
		if let Some(query) = info.query {
			context.push(("query".into(), RestrictedExpression::new_string(query)));
		}
		if let Some(host) = info.host {
			context.push(("host".into(), RestrictedExpression::new_string(host)));
		}
		if !self.context_headers.is_empty() {
			let headers = self.context_headers.iter().filter_map(|name| {
				let value = request.header(name)?.to_str().ok()?.to_string();
				Some((
					name.as_str().to_string(),
					RestrictedExpression::new_string(value),
				))
			});
			context.push((
				"headers".into(),
				RestrictedExpression::new_record(headers).map_err(Error::msg)?,
			));
		}
		for (name, path) in &self.context_properties {
			if let Some(value) = property(request, path)?.as_deref().and_then(to_cedar) {
				context.push((name.clone(), value));
			}
		}
		Ok(context)
	}
}

fn uid(entity_type: &EntityTypeName, id: &str) -> EntityUid {
	EntityUid::from_type_name_and_id(entity_type.clone(), EntityId::new(id))
}

fn entity_uid(
	request: &RequestContext,
	info: &RequestInfo,
	config: &EntityConfig,
	what: &str,
) -> Result<Option<EntityUid>> {
	let id = config
		.id
		.string(request, info)?
		.or_else(|| config.default.clone());
	if id.is_none() {
		log(
			LogLevel::Debug,
			format!("cedar: no {what} id ({:?}); denying", config.id),
		);
	}
	Ok(id.map(|id| uid(&config.entity_type, &id)))
}

// Host errors propagate (the request fails closed); only an unset property is
// `None`.
fn property(
	request: &RequestContext,
	path: &[String],
) -> Result<Option<agentgateway_sdk::CelResult>> {
	let path: Vec<&str> = path.iter().map(String::as_str).collect();
	request.property(&path)
}

fn scalar(value: &CelValue) -> Option<String> {
	match value.kind() {
		CelKind::String => value.as_str().map(str::to_string),
		CelKind::Int => value.as_int().map(|n| n.to_string()),
		CelKind::UInt => value.as_uint().map(|n| n.to_string()),
		CelKind::Bool => value.as_bool().map(|b| b.to_string()),
		_ => None,
	}
}

fn split_commas(value: &str) -> impl Iterator<Item = String> + '_ {
	value
		.split(',')
		.map(str::trim)
		.filter(|v| !v.is_empty())
		.map(str::to_string)
}

impl Source {
	/// A single non-empty string. Scalar properties are stringified.
	fn string(&self, request: &RequestContext, info: &RequestInfo) -> Result<Option<String>> {
		let value = match self {
			Source::Header(name) => request
				.header(name)
				.and_then(|v| v.to_str().ok().map(str::to_string)),
			Source::Property(path) => property(request, path)?.as_deref().and_then(scalar),
			Source::Literal(value) => Some(value.clone()),
			Source::Method => Some(info.method.clone()),
			Source::Path => Some(info.path.clone()),
			Source::Host => info.host.clone(),
		};
		Ok(value.filter(|v| !v.is_empty()))
	}

	/// A list of strings: list properties item by item, headers and plain
	/// strings split on commas.
	fn list(&self, request: &RequestContext, info: &RequestInfo) -> Result<Vec<String>> {
		Ok(match self {
			Source::Header(name) => request
				.header_all(name)
				.iter()
				.filter_map(|v| v.to_str().ok())
				.flat_map(split_commas)
				.collect(),
			Source::Property(path) => match property(request, path)? {
				Some(value) => match value.as_list() {
					Some(items) => items
						.iter()
						.filter_map(scalar)
						.filter(|v| !v.is_empty())
						.collect(),
					None => scalar(&value)
						.map(|v| split_commas(&v).collect())
						.unwrap_or_default(),
				},
				None => vec![],
			},
			other => other
				.string(request, info)?
				.map(|v| split_commas(&v).collect())
				.unwrap_or_default(),
		})
	}
}

/// Converts a CEL value (for example, a JWT claim) into a Cedar value. Cedar
/// has no null or floating-point type: nulls are dropped, and doubles,
/// timestamps and other kinds become strings. Values are never parsed as
/// Cedar syntax, so a claim can't produce an entity reference.
fn to_cedar(value: &CelValue) -> Option<RestrictedExpression> {
	Some(match value.kind() {
		CelKind::Null => return None,
		CelKind::Bool => RestrictedExpression::new_bool(value.as_bool()?),
		CelKind::Int => RestrictedExpression::new_long(value.as_int()?),
		CelKind::String => RestrictedExpression::new_string(value.as_str()?.to_string()),
		CelKind::List => RestrictedExpression::new_set(value.as_list()?.iter().filter_map(to_cedar)),
		CelKind::Map => RestrictedExpression::new_record(
			value
				.entries()
				.filter_map(|(k, v)| Some((k.as_str()?.to_string(), to_cedar(v)?))),
		)
		.ok()?,
		CelKind::UInt => match value.as_uint().map(i64::try_from)? {
			Ok(n) => RestrictedExpression::new_long(n),
			Err(_) => RestrictedExpression::new_string(value.as_uint()?.to_string()),
		},
		_ => RestrictedExpression::new_string(match value.to_json().ok()? {
			Value::String(s) => s,
			other => other.to_string(),
		}),
	})
}

export_plugin!(CedarPlugin, name = "cedar", factory);

#[cfg(test)]
mod tests;
