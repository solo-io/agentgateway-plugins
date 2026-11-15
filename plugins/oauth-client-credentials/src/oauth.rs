use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use std::collections::BTreeMap;

/// Encode a value using application/x-www-form-urlencoded.
pub fn form(input: &str) -> String {
	let mut output = String::new();
	for byte in input.bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
				output.push(byte as char)
			},
			b' ' => output.push('+'),
			_ => {
				use std::fmt::Write;
				let _ = write!(output, "%{byte:02X}");
			},
		}
	}
	output
}

/// Encode bytes for HTTP Basic authentication.
pub fn base64(bytes: &[u8]) -> String {
	const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut output = String::new();
	for chunk in bytes.chunks(3) {
		let encoded = (u32::from(chunk[0]) << 16)
			| (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
			| u32::from(*chunk.get(2).unwrap_or(&0));
		output.push(ABC[((encoded >> 18) & 63) as usize] as char);
		output.push(ABC[((encoded >> 12) & 63) as usize] as char);
		output.push(if chunk.len() > 1 {
			ABC[((encoded >> 6) & 63) as usize] as char
		} else {
			'='
		});
		output.push(if chunk.len() > 2 {
			ABC[(encoded & 63) as usize] as char
		} else {
			'='
		});
	}
	output
}

/// Inputs for the client credentials token request.
pub struct Input<'a> {
	pub id: &'a str,
	pub secret: &'a str,
	pub basic: bool,
	pub scope: Option<&'a str>,
	pub audience: Option<&'a str>,
	pub resource: Option<&'a str>,
	pub extra: &'a BTreeMap<String, String>,
}

/// Construct the token request headers and form body.
pub fn request(input: Input<'_>) -> Result<(HeaderMap, Vec<u8>), &'static str> {
	let mut pairs = vec![("grant_type", "client_credentials")];
	for (key, value) in [
		("scope", input.scope),
		("audience", input.audience),
		("resource", input.resource),
	] {
		if let Some(value) = value {
			pairs.push((key, value));
		}
	}
	for (key, value) in input.extra {
		pairs.push((key, value));
	}
	if !input.basic {
		pairs.push(("client_id", input.id));
		pairs.push(("client_secret", input.secret));
	}
	let body = pairs
		.iter()
		.map(|(key, value)| format!("{}={}", form(key), form(value)))
		.collect::<Vec<_>>()
		.join("&")
		.into_bytes();
	let mut headers = HeaderMap::new();
	headers.insert(
		header::CONTENT_TYPE,
		HeaderValue::from_static("application/x-www-form-urlencoded"),
	);
	headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
	headers.insert(
		header::CONTENT_LENGTH,
		HeaderValue::from_str(&body.len().to_string()).map_err(|_| "content length")?,
	);
	if input.basic {
		let value = format!(
			"Basic {}",
			base64(format!("{}:{}", form(input.id), form(input.secret)).as_bytes())
		);
		let mut value = HeaderValue::from_str(&value).map_err(|_| "basic authorization")?;
		value.set_sensitive(true);
		headers.insert(header::AUTHORIZATION, value);
	}
	Ok((headers, body))
}

/// Validate a bounded bearer token response and return its lifetime.
pub fn parse(
	status: u16,
	body: &[u8],
	max: usize,
	default_ttl: u64,
) -> Result<(String, u64), &'static str> {
	if body.len() > max {
		return Err("response too large");
	}
	if !(200..300).contains(&status) {
		return Err("token endpoint rejected request");
	}
	let value: Value = serde_json::from_slice(body).map_err(|_| "invalid token response")?;
	let token = value
		.get("access_token")
		.and_then(Value::as_str)
		.filter(|input| !input.is_empty())
		.ok_or("missing access_token")?;
	if !value
		.get("token_type")
		.and_then(Value::as_str)
		.is_some_and(|input| input.eq_ignore_ascii_case("bearer"))
	{
		return Err("unsupported token_type");
	}
	let ttl = match value.get("expires_in") {
		None => default_ttl,
		Some(Value::String(input)) => input.parse().map_err(|_| "invalid expires_in")?,
		Some(value) => value.as_u64().ok_or("invalid expires_in")?,
	};
	if ttl == 0 {
		return Err("expired token");
	}
	Ok((token.to_owned(), ttl))
}

/// Extract a bounded OAuth error code without logging the error description.
pub fn error_code(body: &[u8]) -> Option<String> {
	let response: Value = serde_json::from_slice(body).ok()?;
	let code = response.get("error")?.as_str()?;
	if code.is_empty()
		|| code.len() > 128
		|| !code
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
	{
		return None;
	}
	Some(code.to_owned())
}
