use super::*;
use pretty_assertions::assert_eq;

#[test]
fn malformed_outer_json_is_not_repaired() {
    let validation = expect_not_repairable(validate_and_repair(
        &JsonSchema::default(),
        "{\"count\":01}",
        &default_policy(),
    ));

    assert_eq!(
        validation,
        ArgumentValidationResult {
            errors: vec![ArgumentValidationError {
                path: String::new(),
                keyword: ArgumentValidationKeyword::MalformedJson,
            }],
        },
    );
}

#[test]
fn required_null_is_not_deleted() {
    let schema = closed_object(
        BTreeMap::from([("note".to_string(), string_schema(/*description*/ None))]),
        &["note"],
    );
    let validation = expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"note":null}"#,
        &default_policy(),
    ));

    assert_eq!(
        validation.errors,
        vec![ArgumentValidationError {
            path: "/<property>".to_string(),
            keyword: ArgumentValidationKeyword::Type {
                expected: vec![ArgumentValidationType::String],
            },
        }],
    );
}

#[test]
fn nullable_optional_property_is_left_unchanged() {
    let schema = open_object(
        BTreeMap::from([(
            "note".to_string(),
            JsonSchema {
                schema_type: Some(JsonSchemaType::Multiple(vec![
                    JsonSchemaPrimitiveType::String,
                    JsonSchemaPrimitiveType::Null,
                ])),
                ..Default::default()
            },
        )]),
        &[],
    );
    let raw = r#"{"note":null}"#;

    assert_eq!(
        validate_and_repair(&schema, raw, &default_policy()),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: raw.to_string(),
        },
    );
}

#[test]
fn stringified_json_must_satisfy_nested_schema() {
    let schema = array_schema(
        integer_schema(/*description*/ None),
        /*description*/ None,
    );

    expect_not_repairable(validate_and_repair(
        &schema,
        r#""[\"wrong\"]""#,
        &default_policy(),
    ));
}

#[test]
fn scalar_must_satisfy_array_item_schema() {
    let schema = array_schema(
        integer_schema(/*description*/ None),
        /*description*/ None,
    );

    expect_not_repairable(validate_and_repair(
        &schema,
        r#""wrong""#,
        &default_policy(),
    ));
}

#[test]
fn integer_conversion_rejects_fraction_exponent_overflow_and_noncanonical_forms() {
    let schema = integer_schema(/*description*/ None);
    for raw in [
        r#""1.5""#,
        r#""1e3""#,
        r#""18446744073709551616""#,
        r#"" 1""#,
        r#""1 ""#,
        r#""+1""#,
        r#""01""#,
        r#""1_0""#,
        r#""NaN""#,
        r#""Infinity""#,
    ] {
        expect_not_repairable(validate_and_repair(&schema, raw, &default_policy()));
    }
}

#[test]
fn number_conversion_accepts_lossless_exponents_and_rejects_lossy_or_nonfinite_values() {
    let schema = number_schema(/*description*/ None);

    expect_repaired(
        validate_and_repair(&schema, r#""1e3""#, &default_policy()),
        json!(1000.0),
        &[ArgumentRepairRule::NumericStringTyped],
    );
    expect_repaired(
        validate_and_repair(&schema, r#""1.25""#, &default_policy()),
        json!(1.25),
        &[ArgumentRepairRule::NumericStringTyped],
    );
    for raw in [
        r#""9007199254740993.0""#,
        r#""1e400""#,
        r#""-1e400""#,
        r#""NaN""#,
        r#""Infinity""#,
    ] {
        expect_not_repairable(validate_and_repair(&schema, raw, &default_policy()));
    }
}

#[test]
fn boolean_conversion_is_exact_and_case_sensitive() {
    let schema = boolean_schema(/*description*/ None);
    for raw in [
        r#""True""#,
        r#""FALSE""#,
        r#"" true""#,
        r#""false ""#,
        r#""yes""#,
        r#""1""#,
    ] {
        expect_not_repairable(validate_and_repair(&schema, raw, &default_policy()));
    }
}

#[test]
fn ordinary_content_fields_are_never_markdown_unwrapped() {
    for field in ["contents", "message", "patch", "prompt", "shell_command"] {
        let schema = closed_object(
            BTreeMap::from([(
                field.to_string(),
                string_enum_schema(&["fixture-secret-content"]),
            )]),
            &[field],
        );
        let raw = format!(r#"{{"{field}":"`fixture-secret-content`"}}"#);
        expect_not_repairable(validate_and_repair(&schema, &raw, &default_policy()));
    }
}

#[test]
fn ordinary_valid_content_strings_remain_byte_for_byte_unchanged() {
    let schema = closed_object(
        BTreeMap::from([
            ("contents".to_string(), string_schema(/*description*/ None)),
            ("message".to_string(), string_schema(/*description*/ None)),
            ("patch".to_string(), string_schema(/*description*/ None)),
            ("prompt".to_string(), string_schema(/*description*/ None)),
            (
                "shell_command".to_string(),
                string_schema(/*description*/ None),
            ),
        ]),
        &["contents", "message", "patch", "prompt", "shell_command"],
    );
    let raw = r#"{"contents":"{\"x\":1}","message":"`hello`","patch":"*** Begin Patch","prompt":"[1,2]","shell_command":"echo true"}"#;

    assert_eq!(
        validate_and_repair(&schema, raw, &default_policy()),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: raw.to_string(),
        },
    );
}

#[test]
fn validation_error_limit_is_enforced() {
    let schema = closed_object(
        BTreeMap::from([
            ("a".to_string(), integer_schema(/*description*/ None)),
            ("b".to_string(), boolean_schema(/*description*/ None)),
        ]),
        &["a", "b"],
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_validation_errors = 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#"{"a":"x","b":"y"}"#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::ValidationErrors,
        },
    );
}

#[test]
fn additional_properties_false_reports_local_path() {
    let schema = closed_object(BTreeMap::new(), &[]);
    let validation = expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"extra":1}"#,
        &default_policy(),
    ));

    assert_eq!(
        validation.errors,
        vec![ArgumentValidationError {
            path: "/<property>".to_string(),
            keyword: ArgumentValidationKeyword::AdditionalProperties,
        }],
    );
}

#[test]
fn additional_properties_schema_can_drive_repair() {
    let schema = object_with_additional(
        BTreeMap::new(),
        &[],
        Some(AdditionalProperties::Schema(Box::new(integer_schema(
            /*description*/ None,
        )))),
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"extra":"9"}"#, &default_policy()),
        json!({"extra": 9}),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn additional_properties_true_leaves_valid_input_unchanged() {
    let schema = object_with_additional(
        BTreeMap::new(),
        &[],
        Some(AdditionalProperties::Boolean(true)),
    );
    let raw = r#"{"extra":{"nested":"value"}}"#;

    assert_eq!(
        validate_and_repair(&schema, raw, &default_policy()),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: raw.to_string(),
        },
    );
}

#[test]
fn validation_errors_are_deterministic_and_pointer_escaped() {
    let schema = closed_object(
        BTreeMap::from([
            ("a/b~c".to_string(), integer_schema(/*description*/ None)),
            ("z".to_string(), boolean_schema(/*description*/ None)),
        ]),
        &["a/b~c", "z"],
    );
    let validation = expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"a/b~c":"wrong","z":"wrong"}"#,
        &default_policy(),
    ));

    assert_eq!(
        validation.errors,
        vec![
            ArgumentValidationError {
                path: "/<property>".to_string(),
                keyword: ArgumentValidationKeyword::Type {
                    expected: vec![ArgumentValidationType::Boolean],
                },
            },
            ArgumentValidationError {
                path: "/<property>".to_string(),
                keyword: ArgumentValidationKeyword::Type {
                    expected: vec![ArgumentValidationType::Integer],
                },
            },
        ],
    );
}

#[test]
fn debug_output_redacts_argument_values_but_keeps_metadata() {
    let schema = closed_object(
        BTreeMap::from([("password".to_string(), integer_schema(/*description*/ None))]),
        &["password"],
    );
    let not_repairable = validate_and_repair(
        &schema,
        r#"{"password":"fixture-secret"}"#,
        &default_policy(),
    );
    let debug = format!("{not_repairable:?}");
    assert!(!debug.contains("fixture-secret"));
    assert!(debug.contains("/<property>"));
    assert!(debug.contains("Type"));

    let repaired = validate_and_repair(&schema, r#"{"password":"123456"}"#, &default_policy());
    let debug = format!("{repaired:?}");
    assert!(!debug.contains("123456"));
    assert!(debug.contains("NumericStringTyped"));
}

#[test]
fn missing_required_property_is_reported_without_synthesis() {
    let schema = closed_object(
        BTreeMap::from([(
            "required_value".to_string(),
            string_schema(/*description*/ None),
        )]),
        &["required_value"],
    );
    let validation = expect_not_repairable(validate_and_repair(&schema, "{}", &default_policy()));

    assert_eq!(
        validation.errors,
        vec![ArgumentValidationError {
            path: "/<property>".to_string(),
            keyword: ArgumentValidationKeyword::Required,
        }],
    );
}

#[test]
fn numeric_enum_matching_uses_json_schema_number_equality() {
    let schema = JsonSchema {
        schema_type: Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Number)),
        enum_values: Some(vec![json!(1.0)]),
        ..Default::default()
    };
    assert_eq!(
        validate_and_repair(&schema, "1", &default_policy()),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: "1".to_string(),
        },
    );
    expect_repaired(
        validate_and_repair(&schema, r#""1""#, &default_policy()),
        json!(1.0),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn dynamic_unknown_keys_are_redacted_from_public_diagnostics() {
    let schema = closed_object(BTreeMap::new(), &[]);
    let outcome = validate_and_repair(
        &schema,
        r#"{"/Users/secret/project-token":1}"#,
        &default_policy(),
    );
    let validation = expect_not_repairable(outcome.clone());
    assert_eq!(
        validation.errors,
        vec![ArgumentValidationError {
            path: "/<property>".to_string(),
            keyword: ArgumentValidationKeyword::AdditionalProperties,
        }],
    );
    assert!(!format!("{outcome:?}").contains("project-token"));
    assert!(!format!("{outcome:?}").contains("/Users/secret"));
}

#[test]
fn diagnostic_bytes_have_exact_n_minus_one_n_and_n_plus_one_boundaries() {
    let schema = closed_object(
        BTreeMap::from([("a".to_string(), integer_schema(/*description*/ None))]),
        &["a"],
    );
    {
        let (limit, expected_limit) = (26, ArgumentRepairLimit::ValidationDiagnosticBytes);
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_validation_diagnostic_bytes = limit;
        policy.set_limits(limits);
        assert_eq!(
            validate_and_repair(&schema, r#"{"a":"wrong"}"#, &policy),
            ArgumentRepairOutcome::LimitExceeded {
                limit: expected_limit,
            },
        );
    }
    for limit in [27, 28] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_validation_diagnostic_bytes = limit;
        policy.set_limits(limits);
        assert!(matches!(
            validate_and_repair(&schema, r#"{"a":"wrong"}"#, &policy),
            ArgumentRepairOutcome::NotRepairable { .. }
        ));
    }
}
