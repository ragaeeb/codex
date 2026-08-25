use super::*;
use pretty_assertions::assert_eq;

#[test]
fn optional_null_property_is_removed() {
    let schema = closed_object(
        BTreeMap::from([("note".to_string(), string_schema(/*description*/ None))]),
        &[],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"note":null}"#, &default_policy()),
        json!({}),
        &[ArgumentRepairRule::OptionalNullRemoved],
    );
}

#[test]
fn stringified_array_is_decoded() {
    let schema = closed_object(
        BTreeMap::from([(
            "ids".to_string(),
            array_schema(
                integer_schema(/*description*/ None),
                /*description*/ None,
            ),
        )]),
        &["ids"],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"ids":"[1,2]"}"#, &default_policy()),
        json!({"ids": [1, 2]}),
        &[ArgumentRepairRule::StringifiedArrayDecoded],
    );
}

#[test]
fn stringified_object_is_decoded() {
    let metadata = closed_object(
        BTreeMap::from([("id".to_string(), integer_schema(/*description*/ None))]),
        &["id"],
    );
    let schema = closed_object(
        BTreeMap::from([("metadata".to_string(), metadata)]),
        &["metadata"],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"metadata":"{\"id\":1}"}"#, &default_policy()),
        json!({"metadata": {"id": 1}}),
        &[ArgumentRepairRule::StringifiedObjectDecoded],
    );
}

#[test]
fn scalar_is_wrapped_in_array() {
    let schema = closed_object(
        BTreeMap::from([(
            "tags".to_string(),
            array_schema(
                string_schema(/*description*/ None),
                /*description*/ None,
            ),
        )]),
        &["tags"],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"tags":"red"}"#, &default_policy()),
        json!({"tags": ["red"]}),
        &[ArgumentRepairRule::ScalarWrappedInArray],
    );
}

#[test]
fn numeric_string_is_typed() {
    let schema = closed_object(
        BTreeMap::from([("retries".to_string(), integer_schema(/*description*/ None))]),
        &["retries"],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"retries":"42"}"#, &default_policy()),
        json!({"retries": 42}),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn boolean_string_is_typed() {
    let schema = closed_object(
        BTreeMap::from([("enabled".to_string(), boolean_schema(/*description*/ None))]),
        &["enabled"],
    );

    expect_repaired(
        validate_and_repair(&schema, r#"{"enabled":"false"}"#, &default_policy()),
        json!({"enabled": false}),
        &[ArgumentRepairRule::BooleanStringTyped],
    );
}

#[test]
fn explicitly_known_field_alias_is_renamed() {
    let schema = closed_object(
        BTreeMap::from([("query".to_string(), string_schema(/*description*/ None))]),
        &["query"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("", "q", "query")
        .expect("valid alias policy");

    expect_repaired(
        validate_and_repair(&schema, r#"{"q":"status"}"#, &policy),
        json!({"query": "status"}),
        &[ArgumentRepairRule::KnownFieldAlias],
    );
}

#[test]
fn explicitly_allowlisted_markdown_path_is_unwrapped() {
    let schema = closed_object(
        BTreeMap::from([("path".to_string(), string_enum_schema(&["/tmp/report.txt"]))]),
        &["path"],
    );
    let mut policy = default_policy();
    policy
        .allow_markdown_path("/path")
        .expect("valid markdown path policy");

    expect_repaired(
        validate_and_repair(&schema, r#"{"path":"`/tmp/report.txt`"}"#, &policy),
        json!({"path": "/tmp/report.txt"}),
        &[ArgumentRepairRule::MarkdownPathUnwrapped],
    );
}

#[test]
fn combined_independent_repairs_have_deterministic_rule_order() {
    let schema = closed_object(
        BTreeMap::from([
            ("enabled".to_string(), boolean_schema(/*description*/ None)),
            ("note".to_string(), string_schema(/*description*/ None)),
            ("retries".to_string(), integer_schema(/*description*/ None)),
            (
                "tags".to_string(),
                array_schema(
                    string_schema(/*description*/ None),
                    /*description*/ None,
                ),
            ),
        ]),
        &["enabled", "retries", "tags"],
    );

    expect_repaired(
        validate_and_repair(
            &schema,
            r#"{"tags":"blue","retries":"3","note":null,"enabled":"true"}"#,
            &default_policy(),
        ),
        json!({
            "enabled": true,
            "retries": 3,
            "tags": ["blue"],
        }),
        &[
            ArgumentRepairRule::BooleanStringTyped,
            ArgumentRepairRule::OptionalNullRemoved,
            ArgumentRepairRule::NumericStringTyped,
            ArgumentRepairRule::ScalarWrappedInArray,
        ],
    );
}

#[test]
fn markdown_wrapper_requires_exact_allowlisted_pointer() {
    let schema = closed_object(
        BTreeMap::from([("a/b".to_string(), string_enum_schema(&["/tmp/a"]))]),
        &["a/b"],
    );
    let mut policy = default_policy();
    policy
        .allow_markdown_path("/a~1b")
        .expect("valid escaped pointer");

    expect_repaired(
        validate_and_repair(&schema, r#"{"a/b":"`/tmp/a`"}"#, &policy),
        json!({"a/b": "/tmp/a"}),
        &[ArgumentRepairRule::MarkdownPathUnwrapped],
    );

    expect_not_repairable(validate_and_repair(
        &schema,
        r#"{"a/b":"`/tmp/a`"}"#,
        &default_policy(),
    ));
}

#[test]
fn fenced_markdown_path_wrapper_is_exact_and_single_line() {
    let schema = closed_object(
        BTreeMap::from([("path".to_string(), string_enum_schema(&["/tmp/report.txt"]))]),
        &["path"],
    );
    let mut policy = default_policy();
    policy
        .allow_markdown_path("/path")
        .expect("valid markdown path policy");

    expect_repaired(
        validate_and_repair(
            &schema,
            "{\"path\":\"```\\n/tmp/report.txt\\n```\"}",
            &policy,
        ),
        json!({"path": "/tmp/report.txt"}),
        &[ArgumentRepairRule::MarkdownPathUnwrapped],
    );

    for raw in [
        "{\"path\":\"```text\\n/tmp/report.txt\\n```\"}",
        "{\"path\":\"```\\n/tmp/report.txt\\nsecond\\n```\"}",
        "{\"path\":\"``/tmp/report.txt``\"}",
    ] {
        expect_not_repairable(validate_and_repair(&schema, raw, &policy));
    }
}

#[test]
fn alias_policy_is_exact_and_does_not_fuzzy_match() {
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
        r#"{"qurey":"status"}"#,
        &policy,
    ));
}

#[test]
fn nested_object_alias_uses_exact_object_pointer() {
    let options_schema = closed_object(
        BTreeMap::from([("query".to_string(), string_schema(/*description*/ None))]),
        &["query"],
    );
    let schema = closed_object(
        BTreeMap::from([("options".to_string(), options_schema)]),
        &["options"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("/options", "q", "query")
        .expect("valid nested alias policy");

    expect_repaired(
        validate_and_repair(&schema, r#"{"options":{"q":"status"}}"#, &policy),
        json!({"options": {"query": "status"}}),
        &[ArgumentRepairRule::KnownFieldAlias],
    );
}

#[test]
fn integer_conversion_accepts_exact_signed_and_unsigned_boundaries() {
    let schema = integer_schema(/*description*/ None);
    for (raw, expected) in [
        (r#""-9223372036854775808""#, json!(i64::MIN)),
        (r#""9223372036854775807""#, json!(i64::MAX)),
        (r#""18446744073709551615""#, json!(u64::MAX)),
    ] {
        expect_repaired(
            validate_and_repair(&schema, raw, &default_policy()),
            expected,
            &[ArgumentRepairRule::NumericStringTyped],
        );
    }
    for raw in [r#""-9223372036854775809""#, r#""18446744073709551616""#] {
        expect_not_repairable(validate_and_repair(&schema, raw, &default_policy()));
    }
}

#[test]
fn allowlisted_markdown_path_works_under_a_plain_string_schema() {
    let schema = closed_object(
        BTreeMap::from([("path".to_string(), string_schema(/*description*/ None))]),
        &["path"],
    );
    let mut policy = default_policy();
    policy
        .allow_markdown_path("/path")
        .expect("valid markdown path policy");

    expect_repaired(
        validate_and_repair(&schema, r#"{"path":"`/tmp/report.txt`"}"#, &policy),
        json!({"path": "/tmp/report.txt"}),
        &[ArgumentRepairRule::MarkdownPathUnwrapped],
    );
}

#[test]
fn alias_output_is_still_bounded_by_repaired_output_limit() {
    let schema = closed_object(
        BTreeMap::from([("query".to_string(), string_schema(/*description*/ None))]),
        &["query"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("", "q", "query")
        .expect("valid alias policy");
    let mut limits = policy.limits().clone();
    limits.max_repaired_output_bytes = r#"{"query":"x"}"#.len() - 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#"{"q":"x"}"#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairedOutputBytes,
        },
    );
}
