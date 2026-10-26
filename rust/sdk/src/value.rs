//! Typed CEL values read from the host.
//!
//! [`CelResult`] owns a value the host returned and frees it on drop. It derefs
//! to [`CelValue`], a view into the host's buffers: reading a string or walking
//! a map copies and allocates nothing.

use std::fmt::Write as _;
use std::ops::Deref;

use serde::ser::{SerializeMap, SerializeSeq};

use super::*;

/// A CEL value returned by the host, such as from [`CelExpression::evaluate`] or
/// [`RequestContext::property`]. Derefs to [`CelValue`]; borrows from it are
/// valid until the result is dropped.
pub struct CelResult {
	raw: abi::AgwCelResult,
}

// SAFETY: the result's tree is never mutated, and the ABI allows freeing it from
// any thread.
unsafe impl Send for CelResult {}
unsafe impl Sync for CelResult {}

impl CelResult {
	/// Takes ownership of a host result. Returns `None` for `AGW_CEL_UNSET`, and an
	/// error if the tree is malformed; both free the result.
	///
	/// # Safety
	///
	/// `raw` must be a result written by the host, not yet freed.
	pub(crate) unsafe fn from_raw(raw: abi::AgwCelResult) -> Result<Option<Self>> {
		let result = Self { raw };
		let value = &result.raw.value;
		if value.kind == abi::AGW_CEL_UNSET && value.flags == 0 {
			return Ok(None);
		}
		unsafe { validate(value) }?;
		Ok(Some(result))
	}
}

impl Deref for CelResult {
	type Target = CelValue;

	fn deref(&self) -> &CelValue {
		CelValue::from_raw(&self.raw.value)
	}
}

impl Drop for CelResult {
	fn drop(&mut self) {
		unsafe { self.raw.free() };
	}
}

impl std::fmt::Debug for CelResult {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		(**self).fmt(f)
	}
}

/// Checks the whole tree once, so `CelValue`'s accessors can trust it: known
/// kinds, UTF-8 strings and type names, legal map keys, and present arrays.
unsafe fn validate(value: &abi::AgwCelValue) -> Result<()> {
	if value.flags != 0 {
		return Err(Error::msg(format!(
			"host CEL value has unknown flags {:#x}",
			value.flags
		)));
	}
	match value.kind {
		abi::AGW_CEL_NULL
		| abi::AGW_CEL_INT
		| abi::AGW_CEL_UINT
		| abi::AGW_CEL_DOUBLE
		| abi::AGW_CEL_BYTES => {},
		abi::AGW_CEL_BOOL => {
			if !matches!(unsafe { value.value.int_value }, 0 | 1) {
				return Err(Error::msg("host CEL bool is not 0 or 1"));
			}
		},
		abi::AGW_CEL_TIMESTAMP => {
			let time = unsafe { value.value.time };
			if !(0..1_000_000_000).contains(&time.nanos) {
				return Err(Error::msg("host CEL timestamp nanos are out of range"));
			}
		},
		abi::AGW_CEL_DURATION => {
			let time = unsafe { value.value.time };
			let sign_ok =
				(time.seconds >= 0 && time.nanos >= 0) || (time.seconds <= 0 && time.nanos <= 0);
			if time.offset_seconds != 0 || time.nanos.unsigned_abs() >= 1_000_000_000 || !sign_ok {
				return Err(Error::msg(
					"host CEL duration must have no offset and nanos within one second, with the sign of seconds",
				));
			}
		},
		abi::AGW_CEL_STRING | abi::AGW_CEL_TYPE => {
			std::str::from_utf8(unsafe { value.value.bytes.as_slice() })
				.map_err(|e| Error::msg(format!("host CEL string is not UTF-8: {e}")))?;
		},
		abi::AGW_CEL_LIST | abi::AGW_CEL_MAP => {
			let items = unsafe { value.value.items };
			if items.len > 0 && items.ptr.is_null() {
				return Err(Error::msg("host CEL list or map has no items"));
			}
			let items = unsafe { value.items() };
			let mut keys = std::collections::HashSet::new();
			for (i, item) in items.iter().enumerate() {
				if value.kind == abi::AGW_CEL_MAP && i % 2 == 0 {
					let key = unsafe { map_key(item) }?;
					if !keys.insert(key) {
						return Err(Error::msg("host CEL map has a duplicate key"));
					}
				}
				unsafe { validate(item) }?;
			}
		},
		abi::AGW_CEL_UNSET => return Err(Error::msg("host CEL value contains an unset value")),
		other => {
			return Err(Error::msg(format!(
				"host returned unknown CEL value kind {other}"
			)));
		},
	}
	Ok(())
}

/// A map key's identity. Numerically equal `int` and `uint` keys are the same
/// key, as in CEL.
#[derive(PartialEq, Eq, Hash)]
enum MapKey<'a> {
	Bool(bool),
	Number(i128),
	String(&'a [u8]),
}

unsafe fn map_key<'a>(key: &abi::AgwCelValue) -> Result<MapKey<'a>> {
	Ok(match key.kind {
		abi::AGW_CEL_BOOL => MapKey::Bool(unsafe { key.value.int_value } != 0),
		abi::AGW_CEL_INT => MapKey::Number(i128::from(unsafe { key.value.int_value })),
		abi::AGW_CEL_UINT => MapKey::Number(i128::from(unsafe { key.value.uint_value })),
		abi::AGW_CEL_STRING => MapKey::String(unsafe { key.value.bytes.as_slice() }),
		other => {
			return Err(Error::msg(format!(
				"host CEL map key has kind {other}; keys must be bool, int, uint or string"
			)));
		},
	})
}

/// The type of a [`CelValue`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CelKind {
	Null,
	Bool,
	Int,
	UInt,
	Double,
	String,
	Bytes,
	List,
	Map,
	Timestamp,
	Duration,
	Type,
}

/// A CEL value, viewed in place: always handled as `&CelValue`, borrowed from a
/// [`CelResult`]. Values keep their CEL types: `int`, `uint` and `double` are
/// distinct, bytes are bytes, and map keys may be bools, ints, uints or strings.
///
/// Use [`CelValue::json`] to pass a value somewhere that takes JSON, such as
/// [`RequestContext::set_metadata`].
#[repr(transparent)]
pub struct CelValue(abi::AgwCelValue);

// SAFETY: a `CelValue` is only reachable through a `CelResult` (or a test value)
// whose tree is immutable.
unsafe impl Send for CelValue {}
unsafe impl Sync for CelValue {}

impl CelValue {
	fn from_raw(raw: &abi::AgwCelValue) -> &Self {
		// SAFETY: `CelValue` is `repr(transparent)` over `AgwCelValue`.
		unsafe { &*(raw as *const abi::AgwCelValue).cast::<Self>() }
	}

	fn items(&self) -> &[CelValue] {
		// SAFETY: validated when the result was created; `repr(transparent)`.
		let items = unsafe { self.0.items() };
		unsafe { std::slice::from_raw_parts(items.as_ptr().cast::<CelValue>(), items.len()) }
	}

	fn text(&self) -> &str {
		// SAFETY: STRING and TYPE bytes were checked to be UTF-8 on creation.
		unsafe { std::str::from_utf8_unchecked(self.raw_bytes()) }
	}

	// Payload reads. The caller has checked `kind`; every member is plain data,
	// so reading any of them is defined.
	fn int(&self) -> i64 {
		unsafe { self.0.value.int_value }
	}

	fn uint(&self) -> u64 {
		unsafe { self.0.value.uint_value }
	}

	fn double(&self) -> f64 {
		unsafe { self.0.value.double_value }
	}

	fn raw_bytes(&self) -> &[u8] {
		// SAFETY: STRING, BYTES and TYPE point at `len` bytes owned by the result.
		unsafe { self.0.value.bytes.as_slice() }
	}

	fn time(&self) -> abi::AgwCelTime {
		unsafe { self.0.value.time }
	}

	pub fn kind(&self) -> CelKind {
		match self.0.kind {
			abi::AGW_CEL_NULL => CelKind::Null,
			abi::AGW_CEL_BOOL => CelKind::Bool,
			abi::AGW_CEL_INT => CelKind::Int,
			abi::AGW_CEL_UINT => CelKind::UInt,
			abi::AGW_CEL_DOUBLE => CelKind::Double,
			abi::AGW_CEL_STRING => CelKind::String,
			abi::AGW_CEL_BYTES => CelKind::Bytes,
			abi::AGW_CEL_LIST => CelKind::List,
			abi::AGW_CEL_MAP => CelKind::Map,
			abi::AGW_CEL_TIMESTAMP => CelKind::Timestamp,
			abi::AGW_CEL_DURATION => CelKind::Duration,
			abi::AGW_CEL_TYPE => CelKind::Type,
			// Unknown kinds are rejected when the result is created.
			_ => unreachable!("validated CEL value kind"),
		}
	}

	pub fn is_null(&self) -> bool {
		self.0.kind == abi::AGW_CEL_NULL
	}

	pub fn as_bool(&self) -> Option<bool> {
		(self.0.kind == abi::AGW_CEL_BOOL).then(|| self.int() != 0)
	}

	pub fn as_int(&self) -> Option<i64> {
		(self.0.kind == abi::AGW_CEL_INT).then(|| self.int())
	}

	pub fn as_uint(&self) -> Option<u64> {
		(self.0.kind == abi::AGW_CEL_UINT).then(|| self.uint())
	}

	pub fn as_double(&self) -> Option<f64> {
		(self.0.kind == abi::AGW_CEL_DOUBLE).then(|| self.double())
	}

	pub fn as_str(&self) -> Option<&str> {
		(self.0.kind == abi::AGW_CEL_STRING).then(|| self.text())
	}

	pub fn as_bytes(&self) -> Option<&[u8]> {
		(self.0.kind == abi::AGW_CEL_BYTES).then(|| self.raw_bytes())
	}

	pub fn as_timestamp(&self) -> Option<CelTimestamp> {
		(self.0.kind == abi::AGW_CEL_TIMESTAMP).then(|| {
			let time = self.time();
			CelTimestamp {
				seconds: time.seconds,
				nanos: time.nanos,
				offset_seconds: time.offset_seconds,
			}
		})
	}

	pub fn as_duration(&self) -> Option<CelDuration> {
		(self.0.kind == abi::AGW_CEL_DURATION).then(|| {
			let time = self.time();
			CelDuration {
				seconds: time.seconds,
				nanos: time.nanos,
			}
		})
	}

	/// The name of a CEL type value, such as `int` for `type(1)`.
	pub fn as_type_name(&self) -> Option<&str> {
		(self.0.kind == abi::AGW_CEL_TYPE).then(|| self.text())
	}

	pub fn as_list(&self) -> Option<&[CelValue]> {
		(self.0.kind == abi::AGW_CEL_LIST).then(|| self.items())
	}

	/// A map's entries, in the host's order. Empty if this is not a map.
	pub fn entries(&self) -> impl Iterator<Item = (&CelValue, &CelValue)> {
		let items = if self.0.kind == abi::AGW_CEL_MAP {
			self.items()
		} else {
			&[]
		};
		items
			.as_chunks::<2>()
			.0
			.iter()
			.map(|[key, value]| (key, value))
	}

	/// Looks up a string key in a map.
	pub fn get(&self, key: &str) -> Option<&CelValue> {
		self
			.entries()
			.find(|(k, _)| k.as_str() == Some(key))
			.map(|(_, v)| v)
	}

	/// A serializable view of this value using agentgateway's CEL-to-JSON
	/// mapping, e.g. for [`RequestContext::set_metadata`]. The mapping is lossy:
	/// bytes become base64, timestamps RFC 3339 strings in UTC, durations strings
	/// like `"1.5s"`, types their name, and map keys strings. Serializing fails,
	/// rather than losing data silently, on map keys that collide once turned into
	/// strings (`{1: "a", "1": "b"}`) and on non-finite doubles.
	pub fn json(&self) -> CelJson<'_> {
		CelJson(self)
	}

	/// This value as JSON; see [`CelValue::json`] for the mapping.
	pub fn to_json(&self) -> Result<serde_json::Value> {
		serde_json::to_value(self.json()).map_err(Error::msg)
	}

	fn map_key(&self) -> String {
		match self.0.kind {
			abi::AGW_CEL_STRING => self.text().to_owned(),
			abi::AGW_CEL_INT => self.int().to_string(),
			abi::AGW_CEL_UINT => self.uint().to_string(),
			_ => (self.int() != 0).to_string(),
		}
	}
}

impl std::fmt::Debug for CelValue {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self.kind() {
			CelKind::Null => f.write_str("null"),
			CelKind::Bool => write!(f, "{}", self.int() != 0),
			CelKind::Int => write!(f, "{}", self.int()),
			CelKind::UInt => write!(f, "{}u", self.uint()),
			CelKind::Double => write!(f, "{:?}", self.double()),
			CelKind::String => write!(f, "{:?}", self.text()),
			CelKind::Bytes => write!(f, "b{:?}", self.raw_bytes()),
			CelKind::List => f.debug_list().entries(self.items()).finish(),
			CelKind::Map => f.debug_map().entries(self.entries()).finish(),
			CelKind::Timestamp => write!(
				f,
				"timestamp({:?})",
				self.as_timestamp().unwrap().to_string()
			),
			CelKind::Duration => write!(f, "duration({:?})", self.as_duration().unwrap().to_string()),
			CelKind::Type => write!(f, "type({})", self.text()),
		}
	}
}

/// A [`CelValue`] serialized with agentgateway's CEL-to-JSON mapping; see
/// [`CelValue::json`].
pub struct CelJson<'a>(&'a CelValue);

impl serde::Serialize for CelJson<'_> {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
		use serde::ser::Error as _;
		let value = self.0;
		match value.kind() {
			CelKind::Null => serializer.serialize_unit(),
			CelKind::Bool => serializer.serialize_bool(value.int() != 0),
			CelKind::Int => serializer.serialize_i64(value.int()),
			CelKind::UInt => serializer.serialize_u64(value.uint()),
			CelKind::Double => {
				let d = value.double();
				if !d.is_finite() {
					return Err(S::Error::custom(format!("{d} has no JSON form")));
				}
				serializer.serialize_f64(d)
			},
			CelKind::String | CelKind::Type => serializer.serialize_str(value.text()),
			CelKind::Bytes => serializer.serialize_str(&base64(value.raw_bytes())),
			CelKind::List => {
				let items = value.items();
				let mut seq = serializer.serialize_seq(Some(items.len()))?;
				for item in items {
					seq.serialize_element(&item.json())?;
				}
				seq.end()
			},
			CelKind::Map => {
				let mut seen = std::collections::HashSet::new();
				let mut map = serializer.serialize_map(Some(unsafe { value.0.value.items.len }))?;
				for (k, v) in value.entries() {
					let key = k.map_key();
					if !seen.insert(key.clone()) {
						return Err(S::Error::custom(format!(
							"map keys collide as the JSON key {key:?}"
						)));
					}
					map.serialize_entry(&key, &v.json())?;
				}
				map.end()
			},
			CelKind::Timestamp => serializer.serialize_str(&value.as_timestamp().unwrap().to_string()),
			CelKind::Duration => serializer.serialize_str(&value.as_duration().unwrap().to_string()),
		}
	}
}

/// A value [`RequestContext::set_metadata`] accepts: a [`CelValue`] or
/// [`CelResult`], kept exactly (types included), or anything `Serialize`,
/// converted from JSON (numbers become `int` if they fit, else `uint`, else
/// `double`). Sealed: it can't be implemented outside this crate.
pub trait MetadataValue: sealed::Sealed {
	#[doc(hidden)]
	fn metadata(&self) -> Result<Metadata<'_>>;
}

mod sealed {
	pub trait Sealed {}
}

/// What [`MetadataValue::metadata`] hands to `set_metadata`.
#[doc(hidden)]
pub enum Metadata<'a> {
	/// Sent as is.
	Cel(&'a CelValue),
	/// Sent as a CEL value built over this JSON.
	Json(serde_json::Value),
}

impl Metadata<'_> {
	/// Calls `f` with this value as an ABI tree, valid for the call.
	pub(crate) fn with_abi<R>(&self, f: impl FnOnce(&abi::AgwCelValue) -> R) -> R {
		match self {
			Self::Cel(value) => f(&value.0),
			Self::Json(json) => {
				let mut arrays = Vec::new();
				let value = json_to_abi(json, &mut arrays);
				f(&value)
			},
		}
	}
}

impl sealed::Sealed for CelValue {}
impl MetadataValue for CelValue {
	fn metadata(&self) -> Result<Metadata<'_>> {
		Ok(Metadata::Cel(self))
	}
}

impl sealed::Sealed for CelResult {}
impl MetadataValue for CelResult {
	fn metadata(&self) -> Result<Metadata<'_>> {
		Ok(Metadata::Cel(self))
	}
}

// This blanket impl coexists with the two above only because `CelValue` and
// `CelResult` do not implement `Serialize`; keep it that way (`CelValue::json`
// is the explicit way to serialize one).
impl<T: serde::Serialize + ?Sized> sealed::Sealed for T {}
impl<T: serde::Serialize + ?Sized> MetadataValue for T {
	fn metadata(&self) -> Result<Metadata<'_>> {
		serde_json::to_value(self)
			.map(Metadata::Json)
			.map_err(Error::msg)
	}
}

/// Builds an ABI tree over a JSON value: strings point into `json`, and list or
/// map nodes are kept in `arrays`.
fn json_to_abi(
	json: &serde_json::Value,
	arrays: &mut Vec<Box<[abi::AgwCelValue]>>,
) -> abi::AgwCelValue {
	use abi::{AgwCelPayload as P, AgwCelValue as V};
	let string = |s: &str| {
		V::new(
			abi::AGW_CEL_STRING,
			P {
				bytes: abi::AgwBytes::from_slice(s.as_bytes()),
			},
		)
	};
	let array = |items: Vec<V>, arrays: &mut Vec<Box<[V]>>| {
		arrays.push(items.into_boxed_slice());
		arrays.last().expect("just pushed").as_ptr()
	};
	match json {
		serde_json::Value::Null => V::new(abi::AGW_CEL_NULL, P { int_value: 0 }),
		serde_json::Value::Bool(b) => V::new(
			abi::AGW_CEL_BOOL,
			P {
				int_value: i64::from(*b),
			},
		),
		serde_json::Value::Number(n) => {
			if let Some(i) = n.as_i64() {
				V::new(abi::AGW_CEL_INT, P { int_value: i })
			} else if let Some(u) = n.as_u64() {
				V::new(abi::AGW_CEL_UINT, P { uint_value: u })
			} else {
				V::new(
					abi::AGW_CEL_DOUBLE,
					P {
						double_value: n.as_f64().unwrap_or_default(),
					},
				)
			}
		},
		serde_json::Value::String(s) => string(s),
		serde_json::Value::Array(items) => {
			let items = items
				.iter()
				.map(|v| json_to_abi(v, arrays))
				.collect::<Vec<_>>();
			let len = items.len();
			V::new(
				abi::AGW_CEL_LIST,
				P {
					items: abi::AgwCelItems {
						ptr: array(items, arrays),
						len,
					},
				},
			)
		},
		serde_json::Value::Object(map) => {
			let mut items = Vec::with_capacity(map.len() * 2);
			for (k, v) in map {
				items.push(string(k));
				items.push(json_to_abi(v, arrays));
			}
			V::new(
				abi::AGW_CEL_MAP,
				P {
					items: abi::AgwCelItems {
						ptr: array(items, arrays),
						len: map.len(),
					},
				},
			)
		},
	}
}

/// A point in time: `seconds` since the Unix epoch (UTC) plus `nanos` (0..1e9),
/// and the UTC offset it was expressed in, in seconds east of UTC.
///
/// Equality, ordering and hashing use the instant only: the same moment
/// expressed in two offsets compares equal.
#[derive(Clone, Copy, Debug)]
pub struct CelTimestamp {
	pub seconds: i64,
	pub nanos: i32,
	pub offset_seconds: i32,
}

impl CelTimestamp {
	fn instant(&self) -> (i64, i32) {
		(self.seconds, self.nanos)
	}
}

impl PartialEq for CelTimestamp {
	fn eq(&self, other: &Self) -> bool {
		self.instant() == other.instant()
	}
}

impl Eq for CelTimestamp {}

impl PartialOrd for CelTimestamp {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for CelTimestamp {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.instant().cmp(&other.instant())
	}
}

impl std::hash::Hash for CelTimestamp {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.instant().hash(state);
	}
}

/// A signed span of time. `seconds` and `nanos` (-1e9..1e9) both carry the
/// duration's sign: `-0.5s` is `seconds: 0, nanos: -500_000_000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CelDuration {
	pub seconds: i64,
	pub nanos: i32,
}

/// RFC 3339 in UTC, e.g. `2024-01-02T03:04:05.5Z`, matching agentgateway's own
/// formatting (years outside 0..=9999 are signed: `-0001`, `+10000`). The offset
/// is not shown, as agentgateway formats in UTC too.
impl std::fmt::Display for CelTimestamp {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let days = self.seconds.div_euclid(86_400);
		let secs = self.seconds.rem_euclid(86_400);
		let (year, month, day) = civil_from_days(days);
		match year {
			0..=9999 => write!(f, "{year:04}")?,
			..0 => write!(f, "-{:04}", year.unsigned_abs())?,
			_ => write!(f, "+{year}")?,
		}
		write!(
			f,
			"-{month:02}-{day:02}T{:02}:{:02}:{:02}",
			secs / 3600,
			secs % 3600 / 60,
			secs % 60
		)?;
		write_fraction(f, self.nanos.unsigned_abs())?;
		f.write_char('Z')
	}
}

/// Seconds with a trimmed fraction, e.g. `1.5s` or `-0.5s`.
impl std::fmt::Display for CelDuration {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.seconds < 0 || self.nanos < 0 {
			f.write_char('-')?;
		}
		write!(f, "{}", self.seconds.unsigned_abs())?;
		write_fraction(f, self.nanos.unsigned_abs())?;
		f.write_char('s')
	}
}

fn write_fraction(f: &mut std::fmt::Formatter<'_>, nanos: u32) -> std::fmt::Result {
	if nanos == 0 {
		return Ok(());
	}
	let fraction = format!("{nanos:09}");
	write!(f, ".{}", fraction.trim_end_matches('0'))
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	// Howard Hinnant's algorithm, http://howardhinnant.github.io/date_algorithms.html
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	let year = yoe + era * 400 + i64::from(month <= 2);
	(year, month, day)
}

fn base64(bytes: &[u8]) -> String {
	const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
	for chunk in bytes.chunks(3) {
		let b = [
			chunk[0],
			*chunk.get(1).unwrap_or(&0),
			*chunk.get(2).unwrap_or(&0),
		];
		let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
		for i in 0..4 {
			if i <= chunk.len() {
				out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
			} else {
				out.push('=');
			}
		}
	}
	out
}

/// An owned CEL value, for this crate's unit tests to build trees with. Plugins
/// read values as [`CelValue`].
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TestValue {
	Null,
	Bool(bool),
	Int(i64),
	UInt(u64),
	Double(f64),
	String(String),
	Bytes(Vec<u8>),
	List(Vec<TestValue>),
	/// Keys must be `Bool`, `Int`, `UInt` or `String`.
	Map(Vec<(TestValue, TestValue)>),
	Timestamp(CelTimestamp),
	Duration(CelDuration),
	Type(String),
}

#[cfg(test)]
mod test_value {
	use super::*;

	impl From<bool> for TestValue {
		fn from(v: bool) -> Self {
			Self::Bool(v)
		}
	}
	impl From<i64> for TestValue {
		fn from(v: i64) -> Self {
			Self::Int(v)
		}
	}
	impl From<u64> for TestValue {
		fn from(v: u64) -> Self {
			Self::UInt(v)
		}
	}
	impl From<f64> for TestValue {
		fn from(v: f64) -> Self {
			Self::Double(v)
		}
	}
	impl From<&str> for TestValue {
		fn from(v: &str) -> Self {
			Self::String(v.to_owned())
		}
	}
	impl From<String> for TestValue {
		fn from(v: String) -> Self {
			Self::String(v)
		}
	}
	impl From<CelTimestamp> for TestValue {
		fn from(v: CelTimestamp) -> Self {
			Self::Timestamp(v)
		}
	}
	impl From<CelDuration> for TestValue {
		fn from(v: CelDuration) -> Self {
			Self::Duration(v)
		}
	}

	/// Owns the arrays and byte buffers of an `AgwCelValue` tree.
	#[derive(Default)]
	struct Arena {
		arrays: Vec<Box<[abi::AgwCelValue]>>,
		buffers: Vec<Box<[u8]>>,
	}

	impl Arena {
		fn bytes(&mut self, bytes: &[u8]) -> abi::AgwBytes {
			// Store first, then point at the stored copy.
			self.buffers.push(bytes.into());
			abi::AgwBytes::from_slice(self.buffers.last().expect("just pushed"))
		}

		fn array(&mut self, items: Vec<abi::AgwCelValue>) -> *const abi::AgwCelValue {
			self.arrays.push(items.into_boxed_slice());
			self.arrays.last().expect("just pushed").as_ptr()
		}

		fn value(&mut self, value: &TestValue) -> abi::AgwCelValue {
			use abi::{AgwCelPayload as P, AgwCelValue as V};
			match value {
				TestValue::Null => V::new(abi::AGW_CEL_NULL, P { int_value: 0 }),
				TestValue::Bool(b) => V::new(
					abi::AGW_CEL_BOOL,
					P {
						int_value: i64::from(*b),
					},
				),
				TestValue::Int(i) => V::new(abi::AGW_CEL_INT, P { int_value: *i }),
				TestValue::UInt(u) => V::new(abi::AGW_CEL_UINT, P { uint_value: *u }),
				TestValue::Double(d) => V::new(abi::AGW_CEL_DOUBLE, P { double_value: *d }),
				TestValue::String(s) => V::new(
					abi::AGW_CEL_STRING,
					P {
						bytes: self.bytes(s.as_bytes()),
					},
				),
				TestValue::Type(s) => V::new(
					abi::AGW_CEL_TYPE,
					P {
						bytes: self.bytes(s.as_bytes()),
					},
				),
				TestValue::Bytes(b) => V::new(
					abi::AGW_CEL_BYTES,
					P {
						bytes: self.bytes(b),
					},
				),
				TestValue::List(items) => {
					let items = items.iter().map(|v| self.value(v)).collect::<Vec<_>>();
					let len = items.len();
					V::new(
						abi::AGW_CEL_LIST,
						P {
							items: abi::AgwCelItems {
								ptr: self.array(items),
								len,
							},
						},
					)
				},
				TestValue::Map(entries) => {
					let mut items = Vec::with_capacity(entries.len() * 2);
					for (k, v) in entries {
						items.push(self.value(k));
						items.push(self.value(v));
					}
					V::new(
						abi::AGW_CEL_MAP,
						P {
							items: abi::AgwCelItems {
								ptr: self.array(items),
								len: entries.len(),
							},
						},
					)
				},
				TestValue::Timestamp(ts) => V::new(
					abi::AGW_CEL_TIMESTAMP,
					P {
						time: abi::AgwCelTime {
							seconds: ts.seconds,
							nanos: ts.nanos,
							offset_seconds: ts.offset_seconds,
						},
					},
				),
				TestValue::Duration(d) => V::new(
					abi::AGW_CEL_DURATION,
					P {
						time: abi::AgwCelTime {
							seconds: d.seconds,
							nanos: d.nanos,
							offset_seconds: 0,
						},
					},
				),
			}
		}
	}

	unsafe extern "C" fn drop_arena(data: *mut std::ffi::c_void) {
		abi::catch_panic((), || {
			if !data.is_null() {
				unsafe { drop(Box::from_raw(data.cast::<Arena>())) };
			}
		})
	}

	impl TestValue {
		/// Builds an `AgwCelResult` that owns this value's tree, as a host returns it.
		pub(crate) fn to_abi(&self) -> abi::AgwCelResult {
			let mut arena = Arena::default();
			let value = arena.value(self);
			abi::AgwCelResult {
				value,
				private_data: Box::into_raw(Box::new(arena)).cast::<std::ffi::c_void>(),
				drop: Some(drop_arena),
				..Default::default()
			}
		}
	}
}

/// Owns a JSON value and the node arrays of the ABI tree built over it.
#[cfg(any(test, feature = "testing"))]
struct JsonArena {
	// From `Box::into_raw`, so it is never moved after the tree points into it.
	json: *mut serde_json::Value,
	arrays: Vec<Box<[abi::AgwCelValue]>>,
}

#[cfg(any(test, feature = "testing"))]
impl Drop for JsonArena {
	fn drop(&mut self) {
		drop(unsafe { Box::from_raw(self.json) });
	}
}

#[cfg(any(test, feature = "testing"))]
unsafe extern "C" fn drop_json_arena(data: *mut std::ffi::c_void) {
	abi::catch_panic((), || {
		if !data.is_null() {
			unsafe { drop(Box::from_raw(data.cast::<JsonArena>())) };
		}
	})
}

/// An `AgwCelResult` that owns `json`, as a test host returns it. Numbers become
/// `int` if they fit, else `uint`, else `double`.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn json_result(json: serde_json::Value) -> abi::AgwCelResult {
	let json = Box::into_raw(Box::new(json));
	let mut arena = JsonArena {
		json,
		arrays: Vec::new(),
	};
	let value = json_to_abi(unsafe { &*json }, &mut arena.arrays);
	abi::AgwCelResult {
		value,
		private_data: Box::into_raw(Box::new(arena)).cast::<std::ffi::c_void>(),
		drop: Some(drop_json_arena),
		..Default::default()
	}
}

/// Validates a value a plugin passed to a test host and returns it as JSON.
///
/// # Safety
///
/// Every pointer in `value` must be valid for the lengths it advertises.
#[cfg(any(test, feature = "testing"))]
pub(crate) unsafe fn borrowed_json(value: &abi::AgwCelValue) -> Result<serde_json::Value> {
	unsafe { validate(value) }?;
	CelValue::from_raw(value).to_json()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn result(value: TestValue) -> CelResult {
		unsafe { CelResult::from_raw(value.to_abi()) }
			.unwrap()
			.unwrap()
	}

	#[test]
	fn reads_values_in_place() {
		let value = result(TestValue::Map(vec![
			(
				7u64.into(),
				TestValue::List(vec![TestValue::Null, true.into()]),
			),
			("bytes".into(), TestValue::Bytes(vec![0, 255])),
			(TestValue::Int(-3), 1.5.into()),
			("ty".into(), TestValue::Type("int".into())),
		]));
		assert_eq!(value.kind(), CelKind::Map);
		let (key, list) = value.entries().next().unwrap();
		assert_eq!(key.as_uint(), Some(7));
		let list = list.as_list().unwrap();
		assert!(list[0].is_null());
		assert_eq!(list[1].as_bool(), Some(true));
		assert_eq!(
			value.get("bytes").and_then(CelValue::as_bytes),
			Some(&[0, 255][..])
		);
		assert_eq!(
			value.get("ty").and_then(CelValue::as_type_name),
			Some("int")
		);
		assert_eq!(value.get("ty").and_then(CelValue::as_str), None);
		assert_eq!(value.get("missing").map(CelValue::kind), None);
	}

	#[test]
	fn unset_is_none() {
		let raw = abi::AgwCelResult::default();
		assert!(unsafe { CelResult::from_raw(raw) }.unwrap().is_none());
	}

	#[test]
	fn rejects_malformed_values() {
		// A map key the ABI does not allow.
		let bad_key = TestValue::Map(vec![(TestValue::Null, 1i64.into())]);
		let err = unsafe { CelResult::from_raw(bad_key.to_abi()) }.unwrap_err();
		assert!(err.to_string().contains("map key"), "{err}");
		// An unknown kind.
		let mut unknown = TestValue::Null.to_abi();
		unknown.value.kind = 99;
		assert!(unsafe { CelResult::from_raw(unknown) }.is_err());
		// Flags this SDK does not know, including on an otherwise-unset value.
		let mut flagged = TestValue::Int(1).to_abi();
		flagged.value.flags = 1;
		assert!(unsafe { CelResult::from_raw(flagged) }.is_err());
		let mut unset = abi::AgwCelResult::default();
		unset.value.flags = 1;
		assert!(unsafe { CelResult::from_raw(unset) }.is_err());
		// Duplicate keys, where 1 and 1u are the same key.
		let duplicate = TestValue::Map(vec![(1i64.into(), "a".into()), (1u64.into(), "b".into())]);
		let err = unsafe { CelResult::from_raw(duplicate.to_abi()) }.unwrap_err();
		assert!(err.to_string().contains("duplicate"), "{err}");
		// A nested unset value. The tree is borrowed from the stack, so the result
		// has no owner to free.
		let unset_item = [abi::AgwCelValue::default()];
		let nested = abi::AgwCelResult {
			value: abi::AgwCelValue::new(
				abi::AGW_CEL_LIST,
				abi::AgwCelPayload {
					items: abi::AgwCelItems {
						ptr: unset_item.as_ptr(),
						len: 1,
					},
				},
			),
			..Default::default()
		};
		assert!(unsafe { CelResult::from_raw(nested) }.is_err());
		// A duration whose nanos disagree with its sign.
		let bad_duration = TestValue::Duration(CelDuration {
			seconds: 1,
			nanos: -1,
		});
		assert!(unsafe { CelResult::from_raw(bad_duration.to_abi()) }.is_err());
	}

	#[test]
	fn serializes_like_agentgateway_cel_json() {
		let value = result(TestValue::Map(vec![
			("s".into(), "x".into()),
			(1i64.into(), u64::MAX.into()),
			("b".into(), TestValue::Bytes(b"hello".to_vec())),
			(
				"t".into(),
				CelTimestamp {
					seconds: 1_704_164_645,
					nanos: 500_000_000,
					offset_seconds: 3600,
				}
				.into(),
			),
			(
				"d".into(),
				CelDuration {
					seconds: 0,
					nanos: -500_000_000,
				}
				.into(),
			),
			("ty".into(), TestValue::Type("int".into())),
		]));
		assert_eq!(
			value.to_json().unwrap(),
			serde_json::json!({
				"s": "x",
				"1": u64::MAX,
				"b": "aGVsbG8=",
				"t": "2024-01-02T03:04:05.5Z",
				"d": "-0.5s",
				"ty": "int",
			})
		);
	}

	#[test]
	fn json_fails_instead_of_losing_data() {
		let colliding = result(TestValue::Map(vec![
			(1i64.into(), "a".into()),
			("1".into(), "b".into()),
		]));
		assert!(colliding.to_json().is_err());
		assert!(result(f64::NAN.into()).to_json().is_err());
	}

	#[test]
	fn timestamps_compare_by_instant() {
		let utc = CelTimestamp {
			seconds: 100,
			nanos: 0,
			offset_seconds: 0,
		};
		let plus_one = CelTimestamp {
			offset_seconds: 3600,
			..utc
		};
		assert_eq!(utc, plus_one);
	}

	#[test]
	fn formats_timestamps_like_agentgateway() {
		let ts = |seconds| {
			CelTimestamp {
				seconds,
				nanos: 0,
				offset_seconds: 0,
			}
			.to_string()
		};
		assert_eq!(ts(0), "1970-01-01T00:00:00Z");
		assert_eq!(ts(-1), "1969-12-31T23:59:59Z");
		assert_eq!(ts(951_782_400), "2000-02-29T00:00:00Z");
		// Year -1 and year 10000, as the engine formats them.
		assert_eq!(ts(-62_198_755_200), "-0001-01-01T00:00:00Z");
		assert_eq!(ts(253_402_300_800), "+10000-01-01T00:00:00Z");
	}
}
