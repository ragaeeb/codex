use super::ArgumentRepairLimit;
use super::ArgumentValidationError;
use super::ArgumentValidationKeyword;
use super::ArgumentValidationResult;
use super::ArgumentValidationType;
use super::EngineError;
use super::pointer::JsonPointer;
use super::schema::SchemaGraph;
use crate::AdditionalProperties;
use crate::JsonSchema;
use crate::JsonSchemaPrimitiveType;
use crate::JsonSchemaType;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;

pub(crate) struct Validator<'graph, 'schema> {
    graph: &'graph SchemaGraph<'schema>,
}

impl<'graph, 'schema> Validator<'graph, 'schema> {
    pub(crate) fn new(graph: &'graph SchemaGraph<'schema>) -> Self {
        Self { graph }
    }

    pub(crate) fn validate(
        &self,
        value: &JsonValue,
    ) -> Result<ArgumentValidationResult, EngineError> {
        check_value_depth(value, /*depth*/ 0, self.graph.limits().max_value_depth)?;
        let mut errors = ErrorCollector::new(
            self.graph.limits().max_validation_errors,
            self.graph.limits().max_validation_diagnostic_bytes,
        );
        self.collect_errors(
            self.graph.root(),
            value,
            &JsonPointer::root(),
            &JsonPointer::root(),
            &mut errors,
        )?;
        Ok(ArgumentValidationResult {
            errors: errors.finish(),
        })
    }

    pub(crate) fn matches(
        &self,
        schema: &JsonSchema,
        value: &JsonValue,
    ) -> Result<bool, EngineError> {
        check_value_depth(value, /*depth*/ 0, self.graph.limits().max_value_depth)?;
        self.matches_schema(schema, value)
    }

    fn collect_errors(
        &self,
        schema: &JsonSchema,
        value: &JsonValue,
        path: &JsonPointer,
        safe_path: &JsonPointer,
        errors: &mut ErrorCollector,
    ) -> Result<(), EngineError> {
        if let Some(schema_ref) = schema.schema_ref.as_deref() {
            let resolved = self.graph.resolve(schema_ref)?;
            self.collect_errors(resolved.schema, value, path, safe_path, errors)?;
        }

        if let Some(schema_type) = schema.schema_type.as_ref()
            && !matches_schema_type(schema_type, value)
        {
            errors.insert(
                safe_path,
                ArgumentValidationKeyword::Type {
                    expected: validation_types(schema_type),
                },
            )?;
        }

        if let Some(enum_values) = schema.enum_values.as_ref()
            && !enum_values
                .iter()
                .any(|candidate| json_schema_equal(candidate, value))
        {
            errors.insert(safe_path, ArgumentValidationKeyword::Enum)?;
        }

        if let JsonValue::Object(object) = value {
            if let Some(required) = schema.required.as_ref() {
                for required_property in required {
                    if !object.contains_key(required_property) {
                        errors.insert(
                            &safe_path.child("<property>"),
                            ArgumentValidationKeyword::Required,
                        )?;
                    }
                }
            }

            let properties = schema.properties.as_ref();
            for (name, property_value) in object {
                if let Some(property_schema) = properties.and_then(|entries| entries.get(name)) {
                    self.collect_errors(
                        property_schema,
                        property_value,
                        &path.child(name),
                        &safe_path.child("<property>"),
                        errors,
                    )?;
                    continue;
                }
                match schema.additional_properties.as_ref() {
                    None | Some(AdditionalProperties::Boolean(true)) => {}
                    Some(AdditionalProperties::Boolean(false)) => {
                        errors.insert(
                            &safe_path.child("<property>"),
                            ArgumentValidationKeyword::AdditionalProperties,
                        )?;
                    }
                    Some(AdditionalProperties::Schema(additional_schema)) => {
                        self.collect_errors(
                            additional_schema,
                            property_value,
                            &path.child(name),
                            &safe_path.child("<property>"),
                            errors,
                        )?;
                    }
                }
            }
        }

        if let JsonValue::Array(values) = value
            && let Some(items) = schema.items.as_deref()
        {
            for (index, item) in values.iter().enumerate() {
                self.collect_errors(
                    items,
                    item,
                    &path.child(index.to_string()),
                    &safe_path.child("<array-item>"),
                    errors,
                )?;
            }
        }

        if let Some(variants) = schema.any_of.as_ref()
            && !self.matches_any(variants, value)?
        {
            errors.insert(safe_path, ArgumentValidationKeyword::AnyOf)?;
        }
        if let Some(variants) = schema.one_of.as_ref()
            && self.match_count(variants, value)? != 1
        {
            errors.insert(safe_path, ArgumentValidationKeyword::OneOf)?;
        }
        if let Some(variants) = schema.all_of.as_ref()
            && !self.matches_all(variants, value)?
        {
            errors.insert(safe_path, ArgumentValidationKeyword::AllOf)?;
        }
        Ok(())
    }

    fn matches_schema(&self, schema: &JsonSchema, value: &JsonValue) -> Result<bool, EngineError> {
        if let Some(schema_ref) = schema.schema_ref.as_deref() {
            let resolved = self.graph.resolve(schema_ref)?;
            if !self.matches_schema(resolved.schema, value)? {
                return Ok(false);
            }
        }
        if schema
            .schema_type
            .as_ref()
            .is_some_and(|schema_type| !matches_schema_type(schema_type, value))
        {
            return Ok(false);
        }
        if schema.enum_values.as_ref().is_some_and(|values| {
            !values
                .iter()
                .any(|candidate| json_schema_equal(candidate, value))
        }) {
            return Ok(false);
        }

        if let JsonValue::Object(object) = value {
            if schema.required.as_ref().is_some_and(|required| {
                required
                    .iter()
                    .any(|required_property| !object.contains_key(required_property))
            }) {
                return Ok(false);
            }
            let properties = schema.properties.as_ref();
            for (name, property_value) in object {
                if let Some(property_schema) = properties.and_then(|entries| entries.get(name)) {
                    if !self.matches_schema(property_schema, property_value)? {
                        return Ok(false);
                    }
                    continue;
                }
                match schema.additional_properties.as_ref() {
                    None | Some(AdditionalProperties::Boolean(true)) => {}
                    Some(AdditionalProperties::Boolean(false)) => return Ok(false),
                    Some(AdditionalProperties::Schema(additional_schema)) => {
                        if !self.matches_schema(additional_schema, property_value)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }

        if let JsonValue::Array(values) = value
            && let Some(items) = schema.items.as_deref()
        {
            for item in values {
                if !self.matches_schema(items, item)? {
                    return Ok(false);
                }
            }
        }

        if let Some(variants) = schema.any_of.as_ref()
            && !self.matches_any(variants, value)?
        {
            return Ok(false);
        }
        if let Some(variants) = schema.one_of.as_ref()
            && self.match_count(variants, value)? != 1
        {
            return Ok(false);
        }
        if let Some(variants) = schema.all_of.as_ref()
            && !self.matches_all(variants, value)?
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn matches_any(&self, schemas: &[JsonSchema], value: &JsonValue) -> Result<bool, EngineError> {
        for schema in schemas {
            if self.matches_schema(schema, value)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn matches_all(&self, schemas: &[JsonSchema], value: &JsonValue) -> Result<bool, EngineError> {
        for schema in schemas {
            if !self.matches_schema(schema, value)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn match_count(&self, schemas: &[JsonSchema], value: &JsonValue) -> Result<usize, EngineError> {
        let mut count = 0;
        for schema in schemas {
            if self.matches_schema(schema, value)? {
                count += 1;
            }
        }
        Ok(count)
    }
}

struct ErrorCollector {
    errors: BTreeSet<ValidationFailure>,
    max_errors: usize,
    max_diagnostic_bytes: usize,
    diagnostic_bytes: usize,
}

impl ErrorCollector {
    fn new(max_errors: usize, max_diagnostic_bytes: usize) -> Self {
        Self {
            errors: BTreeSet::new(),
            max_errors,
            max_diagnostic_bytes,
            diagnostic_bytes: 0,
        }
    }

    fn insert(
        &mut self,
        path: &JsonPointer,
        keyword: ArgumentValidationKeyword,
    ) -> Result<(), EngineError> {
        let error = ValidationFailure {
            path: path.clone(),
            keyword,
        };
        if self.errors.contains(&error) {
            return Ok(());
        }
        let diagnostic_bytes = path.as_string().len().saturating_add(16);
        self.diagnostic_bytes = self.diagnostic_bytes.saturating_add(diagnostic_bytes);
        if self.diagnostic_bytes > self.max_diagnostic_bytes {
            return Err(EngineError::Limit(
                ArgumentRepairLimit::ValidationDiagnosticBytes,
            ));
        }
        self.errors.insert(error);
        if self.errors.len() > self.max_errors {
            return Err(EngineError::Limit(ArgumentRepairLimit::ValidationErrors));
        }
        Ok(())
    }

    fn finish(self) -> Vec<ArgumentValidationError> {
        self.errors
            .into_iter()
            .map(|error| ArgumentValidationError {
                path: error.path.as_string(),
                keyword: error.keyword,
            })
            .collect()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ValidationFailure {
    path: JsonPointer,
    keyword: ArgumentValidationKeyword,
}

fn check_value_depth(value: &JsonValue, depth: usize, max_depth: usize) -> Result<(), EngineError> {
    if depth > max_depth {
        return Err(EngineError::Limit(ArgumentRepairLimit::ValueDepth));
    }
    match value {
        JsonValue::Array(values) => {
            for value in values {
                check_value_depth(value, depth + 1, max_depth)?;
            }
        }
        JsonValue::Object(values) => {
            for value in values.values() {
                check_value_depth(value, depth + 1, max_depth)?;
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
    Ok(())
}

fn matches_schema_type(schema_type: &JsonSchemaType, value: &JsonValue) -> bool {
    match schema_type {
        JsonSchemaType::Single(schema_type) => matches_primitive_type(*schema_type, value),
        JsonSchemaType::Multiple(schema_types) => schema_types
            .iter()
            .any(|schema_type| matches_primitive_type(*schema_type, value)),
    }
}

fn matches_primitive_type(schema_type: JsonSchemaPrimitiveType, value: &JsonValue) -> bool {
    match schema_type {
        JsonSchemaPrimitiveType::String => value.is_string(),
        JsonSchemaPrimitiveType::Number => value.is_number(),
        JsonSchemaPrimitiveType::Boolean => value.is_boolean(),
        JsonSchemaPrimitiveType::Integer => value.as_number().is_some_and(is_integer),
        JsonSchemaPrimitiveType::Object => value.is_object(),
        JsonSchemaPrimitiveType::Array => value.is_array(),
        JsonSchemaPrimitiveType::Null => value.is_null(),
    }
}

fn is_integer(number: &serde_json::Number) -> bool {
    decimal_signature(&number.to_string()).is_some_and(|(_, _, exponent)| exponent >= 0)
}

fn json_schema_equal(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Number(left), JsonValue::Number(right)) => {
            decimal_signature(&left.to_string()) == decimal_signature(&right.to_string())
        }
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_schema_equal(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_schema_equal(left, right))
                })
        }
        _ => left == right,
    }
}

fn decimal_signature(value: &str) -> Option<(bool, String, i64)> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let exponent_index = value.find('e').or_else(|| value.find('E'));
    let (mantissa, exponent) = match exponent_index {
        Some(index) => (&value[..index], value[index + 1..].parse::<i64>().ok()?),
        None => (value, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, "0".to_string(), 0));
    }
    let trailing_zeros = digits
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'0')
        .count();
    let coefficient = &digits[..digits.len() - trailing_zeros];
    let exponent = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    Some((negative, coefficient.to_string(), exponent))
}

fn validation_types(schema_type: &JsonSchemaType) -> Vec<ArgumentValidationType> {
    let mut types = match schema_type {
        JsonSchemaType::Single(schema_type) => vec![validation_type(*schema_type)],
        JsonSchemaType::Multiple(schema_types) => {
            schema_types.iter().copied().map(validation_type).collect()
        }
    };
    types.sort_unstable();
    types.dedup();
    types
}

fn validation_type(schema_type: JsonSchemaPrimitiveType) -> ArgumentValidationType {
    match schema_type {
        JsonSchemaPrimitiveType::String => ArgumentValidationType::String,
        JsonSchemaPrimitiveType::Number => ArgumentValidationType::Number,
        JsonSchemaPrimitiveType::Boolean => ArgumentValidationType::Boolean,
        JsonSchemaPrimitiveType::Integer => ArgumentValidationType::Integer,
        JsonSchemaPrimitiveType::Object => ArgumentValidationType::Object,
        JsonSchemaPrimitiveType::Array => ArgumentValidationType::Array,
        JsonSchemaPrimitiveType::Null => ArgumentValidationType::Null,
    }
}
