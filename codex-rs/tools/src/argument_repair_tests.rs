use super::*;
use crate::AdditionalProperties;
use crate::JsonSchema;
use crate::JsonSchemaPrimitiveType;
use crate::JsonSchemaType;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;

fn closed_object(properties: BTreeMap<String, JsonSchema>, required: &[&str]) -> JsonSchema {
    object_with_additional(
        properties,
        required,
        Some(AdditionalProperties::Boolean(false)),
    )
}

fn open_object(properties: BTreeMap<String, JsonSchema>, required: &[&str]) -> JsonSchema {
    object_with_additional(properties, required, /*additional_properties*/ None)
}

fn object_with_additional(
    properties: BTreeMap<String, JsonSchema>,
    required: &[&str],
    additional_properties: Option<AdditionalProperties>,
) -> JsonSchema {
    JsonSchema {
        schema_type: Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object)),
        properties: Some(properties),
        required: (!required.is_empty())
            .then(|| required.iter().map(|value| (*value).to_string()).collect()),
        additional_properties,
        ..Default::default()
    }
}

fn primitive_schema(schema_type: JsonSchemaPrimitiveType) -> JsonSchema {
    JsonSchema {
        schema_type: Some(JsonSchemaType::Single(schema_type)),
        ..Default::default()
    }
}

fn string_schema(_description: Option<String>) -> JsonSchema {
    primitive_schema(JsonSchemaPrimitiveType::String)
}

fn integer_schema(_description: Option<String>) -> JsonSchema {
    primitive_schema(JsonSchemaPrimitiveType::Integer)
}

fn number_schema(_description: Option<String>) -> JsonSchema {
    primitive_schema(JsonSchemaPrimitiveType::Number)
}

fn boolean_schema(_description: Option<String>) -> JsonSchema {
    primitive_schema(JsonSchemaPrimitiveType::Boolean)
}

fn array_schema(items: JsonSchema, _description: Option<String>) -> JsonSchema {
    JsonSchema {
        schema_type: Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Array)),
        items: Some(Box::new(items)),
        ..Default::default()
    }
}

fn any_of_schema(schemas: Vec<JsonSchema>, _description: Option<String>) -> JsonSchema {
    JsonSchema {
        any_of: Some(schemas),
        ..Default::default()
    }
}

fn one_of_schema(schemas: Vec<JsonSchema>, _description: Option<String>) -> JsonSchema {
    JsonSchema {
        one_of: Some(schemas),
        ..Default::default()
    }
}

fn all_of_schema(schemas: Vec<JsonSchema>, _description: Option<String>) -> JsonSchema {
    JsonSchema {
        all_of: Some(schemas),
        ..Default::default()
    }
}

fn string_enum_schema(values: &[&str]) -> JsonSchema {
    JsonSchema {
        schema_type: Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::String)),
        enum_values: Some(values.iter().map(|value| json!(*value)).collect()),
        ..Default::default()
    }
}

fn enum_schema(values: Vec<JsonValue>) -> JsonSchema {
    JsonSchema {
        enum_values: Some(values),
        ..Default::default()
    }
}

fn expect_repaired(
    outcome: ArgumentRepairOutcome,
    expected: JsonValue,
    expected_rules: &[ArgumentRepairRule],
) {
    match outcome {
        ArgumentRepairOutcome::Repaired { arguments, rules } => {
            assert_eq!(
                serde_json::from_str::<JsonValue>(&arguments).expect("valid repaired JSON"),
                expected,
            );
            assert_eq!(rules, expected_rules);
        }
        other => panic!("expected repaired outcome, got {other:?}"),
    }
}

fn expect_not_repairable(outcome: ArgumentRepairOutcome) -> ArgumentValidationResult {
    match outcome {
        ArgumentRepairOutcome::NotRepairable { validation } => validation,
        other => panic!("expected not-repairable outcome, got {other:?}"),
    }
}

fn default_policy() -> ArgumentRepairPolicy {
    ArgumentRepairPolicy::default()
}

#[path = "argument_repair_tests/candidate_tests.rs"]
mod candidate_tests;
#[path = "argument_repair_tests/limit_tests.rs"]
mod limit_tests;
#[path = "argument_repair_tests/policy_tests.rs"]
mod policy_tests;
#[path = "argument_repair_tests/schema_tests.rs"]
mod schema_tests;
#[path = "argument_repair_tests/search_tests.rs"]
mod search_tests;
#[path = "argument_repair_tests/transform_tests.rs"]
mod transform_tests;
#[path = "argument_repair_tests/validation_tests.rs"]
mod validation_tests;
