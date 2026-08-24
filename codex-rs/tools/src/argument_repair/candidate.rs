use super::ArgumentRepairLimit;
use super::ArgumentRepairPolicy;
use super::ArgumentRepairRule;
use super::EngineError;
use super::canonical::deduplicate;
use super::pointer::JsonPointer;
use super::schema::SchemaGraph;
use super::validation::Validator;
use crate::AdditionalProperties;
use crate::JsonSchema;
use crate::JsonSchemaType;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

mod transform;

use transform::matches_schema_type;
use transform::primitive_types;
use transform::remove_property_at_path;
use transform::replace_at_path;
use transform::type_rank;
use transform::type_replacements;
use transform::unwrap_markdown_path;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RepairStep {
    pub(crate) path: String,
    pub(crate) rule: ArgumentRepairRule,
}

#[derive(Clone)]
pub(crate) struct AtomicCandidate {
    pub(crate) value: JsonValue,
    pub(crate) step: RepairStep,
}

/// Shared attempted-work budget for one complete repair search.
///
/// The budget is consumed before cloning the root value. This makes a wide value fail closed
/// before a fan-out of full-root allocations can exceed the configured memory envelope.
pub(crate) struct CandidateBudget {
    attempted: usize,
    max: usize,
}

impl CandidateBudget {
    pub(crate) fn new(max: usize) -> Self {
        Self { attempted: 0, max }
    }

    pub(crate) fn reserve(&mut self) -> Result<(), EngineError> {
        if self.attempted >= self.max {
            return Err(EngineError::Limit(
                super::ArgumentRepairLimit::RepairCandidates,
            ));
        }
        self.attempted += 1;
        Ok(())
    }

    pub(crate) fn attempted(&self) -> usize {
        self.attempted
    }
}

pub(crate) fn generate_candidates<'graph, 'schema>(
    graph: &'graph SchemaGraph<'schema>,
    validator: &Validator<'graph, 'schema>,
    value: &JsonValue,
    policy: &ArgumentRepairPolicy,
    budget: &mut CandidateBudget,
) -> Result<Vec<AtomicCandidate>, EngineError> {
    let generator = CandidateGenerator {
        graph,
        validator,
        root_value: value,
        policy,
    };
    let mut candidates = Vec::new();
    generator.collect(
        graph.root(),
        value,
        &JsonPointer::root(),
        &mut candidates,
        budget,
    )?;
    Ok(deduplicate(candidates))
}

struct CandidateGenerator<'context, 'graph, 'schema> {
    graph: &'graph SchemaGraph<'schema>,
    validator: &'context Validator<'graph, 'schema>,
    root_value: &'context JsonValue,
    policy: &'context ArgumentRepairPolicy,
}

impl<'context, 'graph, 'schema> CandidateGenerator<'context, 'graph, 'schema> {
    fn collect(
        &self,
        schema: &JsonSchema,
        value: &JsonValue,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        let locally_valid = self.validator.matches(schema, value)?;

        if let Some(schema_ref) = schema.schema_ref.as_deref() {
            let resolved = self.graph.resolve(schema_ref)?;
            if !self.validator.matches(resolved.schema, value)? {
                self.collect(resolved.schema, value, path, candidates, budget)?;
            }
        }

        // An explicitly allowlisted markdown path is a repair constraint even when the wrapped
        // string is otherwise valid under its ordinary string schema.
        self.collect_markdown_candidate(schema, value, path, candidates, budget)?;
        self.collect_composition_candidates(schema, value, path, candidates, budget)?;

        if let Some(schema_type) = schema.schema_type.as_ref()
            && !matches_schema_type(schema_type, value)
        {
            self.collect_type_candidates(schema, schema_type, value, path, candidates, budget)?;
        }

        if let JsonValue::Object(object) = value {
            if !locally_valid {
                self.collect_object_candidates(schema, object, path, candidates, budget)?;
            }
            if let Some(properties) = schema.properties.as_ref() {
                for (name, property_schema) in properties {
                    let Some(property_value) = object.get(name) else {
                        continue;
                    };
                    self.collect(
                        property_schema,
                        property_value,
                        &path.child(name),
                        candidates,
                        budget,
                    )?;
                }
            }
            for (name, property_value) in object {
                if schema
                    .properties
                    .as_ref()
                    .is_some_and(|properties| properties.contains_key(name))
                {
                    continue;
                }
                if let Some(AdditionalProperties::Schema(additional_schema)) =
                    schema.additional_properties.as_ref()
                {
                    self.collect(
                        additional_schema,
                        property_value,
                        &path.child(name),
                        candidates,
                        budget,
                    )?;
                }
            }
        }
        if let JsonValue::Array(values) = value
            && let Some(items) = schema.items.as_deref()
        {
            for (index, item) in values.iter().enumerate() {
                self.collect(
                    items,
                    item,
                    &path.child(index.to_string()),
                    candidates,
                    budget,
                )?;
            }
        }
        Ok(())
    }

    fn collect_composition_candidates(
        &self,
        schema: &JsonSchema,
        value: &JsonValue,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        if let Some(variants) = schema.any_of.as_ref()
            && self.match_count(variants, value)? == 0
        {
            self.collect_failing_variants(variants, value, path, candidates, budget)?;
        }
        if let Some(variants) = schema.one_of.as_ref()
            && self.match_count(variants, value)? == 0
        {
            self.collect_failing_variants(variants, value, path, candidates, budget)?;
        }
        if let Some(variants) = schema.all_of.as_ref()
            && self.match_count(variants, value)? != variants.len()
        {
            self.collect_failing_variants(variants, value, path, candidates, budget)?;
        }
        Ok(())
    }

    fn collect_failing_variants(
        &self,
        variants: &[JsonSchema],
        value: &JsonValue,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        for variant in variants {
            if !self.validator.matches(variant, value)? {
                self.collect(variant, value, path, candidates, budget)?;
            }
        }
        Ok(())
    }

    fn collect_markdown_candidate(
        &self,
        schema: &JsonSchema,
        value: &JsonValue,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        if !self.policy.markdown_paths.contains(path) {
            return Ok(());
        }
        let JsonValue::String(value) = value else {
            return Ok(());
        };
        let Some(unwrapped) = unwrap_markdown_path(value) else {
            return Ok(());
        };
        let replacement = JsonValue::String(unwrapped.to_string());
        if self.validator.matches(schema, &replacement)? {
            self.push_replacement(
                path,
                replacement,
                ArgumentRepairRule::MarkdownPathUnwrapped,
                candidates,
                budget,
            )?;
        }
        Ok(())
    }

    fn collect_type_candidates(
        &self,
        schema: &JsonSchema,
        schema_type: &JsonSchemaType,
        value: &JsonValue,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        let mut replacements_by_type = BTreeMap::<u8, Vec<(JsonValue, ArgumentRepairRule)>>::new();
        for expected_type in primitive_types(schema_type) {
            for (replacement, rule) in
                type_replacements(expected_type, schema.items.as_deref(), value)
            {
                // Shape-improving candidates intentionally need not validate nested children at
                // this point. Search revalidates the complete root after each atomic edit, which
                // lets decoded containers, scalar wrapping, and sibling repairs compose safely.
                if matches_schema_type(schema_type, &replacement) {
                    replacements_by_type
                        .entry(type_rank(expected_type))
                        .or_default()
                        .push((replacement, rule));
                }
            }
        }
        // A direct multi-type schema remains conservative: a value that could be interpreted as
        // more than one primitive type is ambiguous. Composition branches are handled separately
        // and can be disambiguated by complete root validation.
        if replacements_by_type.len() != 1 {
            return Ok(());
        }
        for (replacement, rule) in replacements_by_type.into_values().flatten() {
            self.push_replacement(path, replacement, rule, candidates, budget)?;
        }
        Ok(())
    }

    fn collect_object_candidates(
        &self,
        schema: &JsonSchema,
        object: &JsonMap<String, JsonValue>,
        path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        if let Some(properties) = schema.properties.as_ref() {
            for (name, property_schema) in properties {
                let Some(property_value) = object.get(name) else {
                    continue;
                };
                let property_path = path.child(name);
                if property_value.is_null()
                    && !is_required(schema, name)
                    && !self.validator.matches(property_schema, property_value)?
                {
                    self.push_property_removal(&property_path, candidates, budget)?;
                }
            }
        }

        for alias in self
            .policy
            .aliases
            .iter()
            .take(super::MAX_ARGUMENT_REPAIR_POLICY_ALIASES)
            .filter(|alias| &alias.object_path == path)
        {
            if alias.source_field.len() > self.policy.limits.max_schema_name_bytes
                || alias.destination_field.len() > self.policy.limits.max_schema_name_bytes
            {
                return Err(EngineError::Limit(ArgumentRepairLimit::SchemaNameBytes));
            }
            let Some(properties) = schema.properties.as_ref() else {
                continue;
            };
            if !object.contains_key(&alias.source_field)
                || object.contains_key(&alias.destination_field)
                || !properties.contains_key(&alias.destination_field)
            {
                continue;
            }
            budget.reserve()?;
            let mut renamed = object.clone();
            let Some(source_value) = renamed.remove(&alias.source_field) else {
                continue;
            };
            renamed.insert(alias.destination_field.clone(), source_value);
            // Do not require the whole object to validate locally. A rename may be the first
            // half of a safe composition with an independent sibling repair.
            self.push_alias(path, &alias.destination_field, renamed, candidates)?;
        }
        Ok(())
    }

    fn push_replacement(
        &self,
        path: &JsonPointer,
        replacement: JsonValue,
        rule: ArgumentRepairRule,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        budget.reserve()?;
        let mut candidate = self.root_value.clone();
        if replace_at_path(&mut candidate, path, replacement) {
            candidates.push(AtomicCandidate {
                value: candidate,
                step: RepairStep {
                    path: path.as_string(),
                    rule,
                },
            });
        }
        Ok(())
    }

    fn push_property_removal(
        &self,
        property_path: &JsonPointer,
        candidates: &mut Vec<AtomicCandidate>,
        budget: &mut CandidateBudget,
    ) -> Result<(), EngineError> {
        budget.reserve()?;
        let mut candidate = self.root_value.clone();
        if remove_property_at_path(&mut candidate, property_path) {
            candidates.push(AtomicCandidate {
                value: candidate,
                step: RepairStep {
                    path: property_path.as_string(),
                    rule: ArgumentRepairRule::OptionalNullRemoved,
                },
            });
        }
        Ok(())
    }

    fn push_alias(
        &self,
        object_path: &JsonPointer,
        destination_field: &str,
        renamed: JsonMap<String, JsonValue>,
        candidates: &mut Vec<AtomicCandidate>,
    ) -> Result<(), EngineError> {
        let mut candidate = self.root_value.clone();
        if replace_at_path(&mut candidate, object_path, JsonValue::Object(renamed)) {
            candidates.push(AtomicCandidate {
                value: candidate,
                step: RepairStep {
                    path: object_path.child(destination_field).as_string(),
                    rule: ArgumentRepairRule::KnownFieldAlias,
                },
            });
        }
        Ok(())
    }

    fn match_count(&self, schemas: &[JsonSchema], value: &JsonValue) -> Result<usize, EngineError> {
        let mut count = 0;
        for schema in schemas {
            if self.validator.matches(schema, value)? {
                count += 1;
            }
        }
        Ok(count)
    }
}

fn is_required(schema: &JsonSchema, property: &str) -> bool {
    schema
        .required
        .as_ref()
        .is_some_and(|required| required.iter().any(|entry| entry.as_str() == property))
}
