//! Cedar evaluation over `cedar-policy` types only. `lib.rs` maps a gateway
//! request into an [`AuthzInput`] and the [`Outcome`] back to an action.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use agentgateway_sdk::{Error, Result};
use cedar_policy::{
	Authorizer, Context, Decision, Entities, Entity, EntityTypeName, EntityUid, PolicyId, PolicySet,
	Request, RestrictedExpression, Schema, SchemaFragment, ValidationMode, Validator,
};
use serde_json::Value;

pub struct Engine {
	policies: PolicySet,
	schema: Option<Schema>,
	entities: Entities,
	// With no user-supplied entities, a request store is just the principal and
	// needs no copy of `entities` (which may still hold the schema's actions).
	no_static_entities: bool,
	// With a schema, the principal attributes it declares. Cedar rejects an
	// entity with undeclared attributes, and tokens carry claims (`iss`, `exp`,
	// ...) that schemas rarely list.
	principal_attrs: Option<HashSet<String>>,
	names: HashMap<PolicyId, String>,
	authorizer: Authorizer,
}

pub struct AuthzInput {
	pub principal: EntityUid,
	pub attrs: HashMap<String, RestrictedExpression>,
	pub parents: HashSet<EntityUid>,
	pub action: EntityUid,
	pub resource: EntityUid,
	pub context: Vec<(String, RestrictedExpression)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
	pub allowed: bool,
	/// Policies that determined the decision, by `@id` annotation or policy ID.
	pub policies: Vec<String>,
	/// Policies that failed to evaluate. Cedar skips them; they never allow.
	pub errors: Vec<String>,
}

impl Engine {
	/// `entity_types` are the principal, parent and resource types; with a
	/// schema they must be declared in it.
	pub fn new(
		policies: &str,
		schema: Option<&str>,
		entities: Option<Value>,
		principal_type: &EntityTypeName,
		entity_types: &[&EntityTypeName],
	) -> Result<Self> {
		let (schema, principal_attrs) = match schema {
			Some(text) => {
				let schema = Schema::from_cedarschema_str(text)
					.map(|(schema, _warnings)| schema)
					.map_err(|e| Error::msg(format!("invalid cedar schema: {e}")))?;
				for ty in entity_types {
					if !schema.entity_types().any(|t| t == *ty) {
						return Err(Error::msg(format!(
							"cedar entity type {ty} is not declared in the schema"
						)));
					}
				}
				(Some(schema), Some(declared_attrs(text, principal_type)?))
			},
			None => (None, None),
		};

		let policies = PolicySet::from_str(policies)
			.map_err(|e| Error::msg(format!("invalid cedar policies: {e}")))?;
		if policies.policies().next().is_none() {
			return Err(Error::msg(
				"cedar policies are empty; every request would be denied",
			));
		}
		if let Some(schema) = &schema {
			let result = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
			if !result.validation_passed() {
				let errors: Vec<String> = result.validation_errors().map(|e| e.to_string()).collect();
				return Err(Error::msg(format!(
					"cedar policies fail schema validation: {}",
					errors.join("; ")
				)));
			}
		}

		let entities = entities.unwrap_or(Value::Array(vec![]));
		let no_static_entities = entities.as_array().is_some_and(Vec::is_empty);
		let entities = Entities::from_json_value(entities, schema.as_ref())
			.map_err(|e| Error::msg(format!("invalid cedar entities: {e}")))?;

		let names = policies
			.policies()
			.map(|p| {
				let name = p.annotation("id").unwrap_or(p.id().as_ref()).to_string();
				(p.id().clone(), name)
			})
			.collect();

		Ok(Self {
			policies,
			schema,
			entities,
			no_static_entities,
			principal_attrs,
			names,
			authorizer: Authorizer::new(),
		})
	}

	/// Whether a principal attribute can go on the request's principal entity.
	pub fn keeps_attr(&self, name: &str) -> bool {
		self
			.principal_attrs
			.as_ref()
			.is_none_or(|attrs| attrs.contains(name))
	}

	/// Errors mean the input can't form a valid Cedar request (for example, a
	/// context that doesn't match the schema).
	pub fn authorize(&self, input: AuthzInput) -> Result<Outcome> {
		let schema = self.schema.as_ref();
		let request_entities;
		// Static entities are the source of truth for principals they define, and
		// a principal with no attributes or parents evaluates the same as a
		// missing one. Otherwise Cedar needs the principal in the store; adding it
		// copies the static store, which costs O(static entities) per request.
		let entities = if self.entities.get(&input.principal).is_some()
			|| (input.attrs.is_empty() && input.parents.is_empty())
		{
			&self.entities
		} else {
			let entity = Entity::new(input.principal.clone(), input.attrs, input.parents)
				.map_err(|e| Error::msg(format!("invalid principal entity: {e}")))?;
			request_entities = if self.no_static_entities {
				Entities::from_entities([entity], schema)
			} else {
				self.entities.clone().add_entities([entity], schema)
			}
			.map_err(|e| Error::msg(format!("invalid principal entity: {e}")))?;
			&request_entities
		};

		let context = Context::from_pairs(input.context)
			.map_err(|e| Error::msg(format!("invalid cedar context: {e}")))?;
		let request = Request::new(
			input.principal,
			input.action,
			input.resource,
			context,
			schema,
		)
		.map_err(|e| Error::msg(format!("invalid cedar request: {e}")))?;

		let response = self
			.authorizer
			.is_authorized(&request, &self.policies, entities);
		let diagnostics = response.diagnostics();
		Ok(Outcome {
			allowed: response.decision() == Decision::Allow,
			policies: diagnostics
				.reason()
				.map(|id| {
					self
						.names
						.get(id)
						.cloned()
						.unwrap_or_else(|| id.to_string())
				})
				.collect(),
			errors: diagnostics.errors().map(|e| e.to_string()).collect(),
		})
	}
}

/// Top-level attribute names the schema declares for `ty`. The Cedar schema
/// API doesn't expose entity shapes, so read them from the JSON form.
fn declared_attrs(schema: &str, ty: &EntityTypeName) -> Result<HashSet<String>> {
	let json = SchemaFragment::from_cedarschema_str(schema)
		.map_err(|e| Error::msg(format!("invalid cedar schema: {e}")))?
		.0
		.to_json_value()
		.map_err(|e| Error::msg(format!("invalid cedar schema: {e}")))?;
	let name = ty.to_string();
	let (namespace, basename) = name.rsplit_once("::").unwrap_or(("", &name));
	Ok(
		json[namespace]["entityTypes"][basename]["shape"]["attributes"]
			.as_object()
			.map(|attrs| attrs.keys().cloned().collect())
			.unwrap_or_default(),
	)
}
