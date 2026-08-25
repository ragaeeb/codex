use super::*;
use pretty_assertions::assert_eq;

#[test]
fn valid_arguments_remain_byte_for_byte_unchanged() {
    let schema = closed_object(
        BTreeMap::from([("count".to_string(), integer_schema(/*description*/ None))]),
        &["count"],
    );
    let raw = "{  \"count\" : 1 }";

    assert_eq!(
        validate_and_repair(&schema, raw, &default_policy()),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: raw.to_string(),
        },
    );
}

#[test]
fn alias_does_not_overwrite_present_destination() {
    let schema = closed_object(
        BTreeMap::from([("query".to_string(), string_schema(/*description*/ None))]),
        &["query"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("", "q", "query")
        .expect("valid alias policy");

    expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"q":"new","query":"existing"}"#,
        &policy,
    ));
}

#[test]
fn competing_aliases_are_ambiguous() {
    let schema = open_object(
        BTreeMap::from([("query".to_string(), string_schema(/*description*/ None))]),
        &["query"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("", "q", "query")
        .expect("valid alias policy");
    policy
        .insert_known_field_alias("", "search", "query")
        .expect("valid alias policy");

    expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"q":"one","search":"two"}"#,
        &policy,
    ));
}

#[test]
fn invalid_alias_and_pointer_policy_are_rejected() {
    let mut policy = default_policy();

    assert_eq!(
        policy.insert_known_field_alias("", "same", "same"),
        Err(ArgumentRepairPolicyError::AliasSourceEqualsDestination),
    );
    assert_eq!(
        policy.allow_markdown_path("not/a/pointer"),
        Err(ArgumentRepairPolicyError::InvalidJsonPointer),
    );
    assert_eq!(
        policy.insert_known_field_alias("/", &"x".repeat(1_025), "b"),
        Err(ArgumentRepairPolicyError::AliasFieldTooLong),
    );
    assert_eq!(
        policy.allow_markdown_path(&format!("/{}", "x".repeat(65 * 1024))),
        Err(ArgumentRepairPolicyError::PolicyPathTooLong),
    );
}

#[test]
fn bounded_mutation_corpus_is_deterministic() {
    let schema = closed_object(
        BTreeMap::from([
            ("count".to_string(), integer_schema(/*description*/ None)),
            ("enabled".to_string(), boolean_schema(/*description*/ None)),
            (
                "message".to_string(),
                string_enum_schema(&["fixture-secret"]),
            ),
        ]),
        &["count", "enabled", "message"],
    );
    let corpus = [
        r#"{"count":1,"enabled":true,"message":"fixture-secret"}"#,
        r#"{"count":"1","enabled":"true","message":"fixture-secret"}"#,
        r#"{"count":"+1","enabled":"TRUE","message":"fixture-secret"}"#,
        r#"{"count":"1e0","enabled":" true","message":"fixture-secret"}"#,
        r#"{"count":null,"enabled":null,"message":"fixture-secret"}"#,
        r#"{"count":[],"enabled":{},"message":"fixture-secret"}"#,
        r#"{"count":"1","enabled":"true","message":"`fixture-secret`"}"#,
        r#"{"count":"[1]","enabled":"false","message":"fixture-secret"}"#,
        r#"{"count":"{\"value\":1}","enabled":"false","message":"fixture-secret"}"#,
        r#"{"count":1,"enabled":true,"message":"fixture-secret","extra":0}"#,
        r#"{"count":1,"enabled":true,"message":"fixture-secret""#,
    ];

    let type_error = |_path: &str, expected| ArgumentValidationError {
        path: "/<property>".to_string(),
        keyword: ArgumentValidationKeyword::Type {
            expected: vec![expected],
        },
    };
    let invalid = |errors| ArgumentRepairOutcome::NotRepairable {
        validation: ArgumentValidationResult { errors },
    };
    let expected = [
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: corpus[0].to_string(),
        },
        ArgumentRepairOutcome::Repaired {
            arguments: r#"{"count":1,"enabled":true,"message":"fixture-secret"}"#.to_string(),
            rules: vec![
                ArgumentRepairRule::NumericStringTyped,
                ArgumentRepairRule::BooleanStringTyped,
            ],
        },
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
            ArgumentValidationError {
                path: "/<property>".to_string(),
                keyword: ArgumentValidationKeyword::Enum,
            },
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![
            type_error("/enabled", ArgumentValidationType::Boolean),
            type_error("/count", ArgumentValidationType::Integer),
        ]),
        invalid(vec![ArgumentValidationError {
            path: "/<property>".to_string(),
            keyword: ArgumentValidationKeyword::AdditionalProperties,
        }]),
        invalid(vec![ArgumentValidationError {
            path: String::new(),
            keyword: ArgumentValidationKeyword::MalformedJson,
        }]),
    ];
    assert_eq!(corpus.len(), expected.len());
    for (raw, expected) in corpus.into_iter().zip(expected) {
        let actual = validate_and_repair(&schema, raw, &default_policy());
        assert_eq!(actual, expected);
        assert!(!format!("{actual:?}").contains("fixture-secret"));
    }
}

#[test]
fn repair_rule_identifiers_are_stable() {
    let rules = [
        (
            ArgumentRepairRule::OptionalNullRemoved,
            "optional_null_removed",
        ),
        (
            ArgumentRepairRule::StringifiedArrayDecoded,
            "stringified_array_decoded",
        ),
        (
            ArgumentRepairRule::StringifiedObjectDecoded,
            "stringified_object_decoded",
        ),
        (
            ArgumentRepairRule::ScalarWrappedInArray,
            "scalar_wrapped_in_array",
        ),
        (
            ArgumentRepairRule::NumericStringTyped,
            "numeric_string_typed",
        ),
        (
            ArgumentRepairRule::BooleanStringTyped,
            "boolean_string_typed",
        ),
        (ArgumentRepairRule::KnownFieldAlias, "known_field_alias"),
        (
            ArgumentRepairRule::MarkdownPathUnwrapped,
            "markdown_path_unwrapped",
        ),
    ];

    for (rule, expected) in rules {
        assert_eq!(rule.as_str(), expected);
        assert_eq!(
            serde_json::to_value(rule).expect("serialize rule"),
            json!(expected),
        );
    }
}

#[test]
fn repair_provenance_retains_repeated_rule_occurrences() {
    let schema = array_schema(
        integer_schema(/*description*/ None),
        /*description*/ None,
    );
    expect_repaired(
        validate_and_repair(&schema, r#"["1","2"]"#, &default_policy()),
        json!([1, 2]),
        &[
            ArgumentRepairRule::NumericStringTyped,
            ArgumentRepairRule::NumericStringTyped,
        ],
    );
}
