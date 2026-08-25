use super::*;
use pretty_assertions::assert_eq;

#[test]
fn any_of_unique_candidate_is_repaired() {
    let schema = any_of_schema(
        vec![
            integer_schema(/*description*/ None),
            boolean_schema(/*description*/ None),
        ],
        /*description*/ None,
    );

    expect_repaired(
        validate_and_repair(&schema, r#""true""#, &default_policy()),
        json!(true),
        &[ArgumentRepairRule::BooleanStringTyped],
    );
}

#[test]
fn any_of_distinct_candidates_are_ambiguous() {
    let schema = any_of_schema(
        vec![
            integer_schema(/*description*/ None),
            array_schema(
                string_schema(/*description*/ None),
                /*description*/ None,
            ),
        ],
        /*description*/ None,
    );

    expect_not_repairable(validate_and_repair(&schema, r#""1""#, &default_policy()));
}

#[test]
fn any_of_deduplicates_same_complete_repaired_value() {
    let schema = any_of_schema(
        vec![
            integer_schema(/*description*/ None),
            number_schema(/*description*/ None),
        ],
        /*description*/ None,
    );

    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repair_candidates = 2;
    policy.set_limits(limits);
    expect_repaired(
        validate_and_repair(&schema, r#""1""#, &policy),
        json!(1),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn direct_ambiguous_type_union_is_not_repaired() {
    let schema = JsonSchema {
        schema_type: Some(JsonSchemaType::Multiple(vec![
            JsonSchemaPrimitiveType::Integer,
            JsonSchemaPrimitiveType::Number,
        ])),
        ..Default::default()
    };

    expect_not_repairable(validate_and_repair(&schema, r#""1""#, &default_policy()));
}

#[test]
fn one_of_unique_candidate_is_repaired() {
    let schema = one_of_schema(
        vec![
            integer_schema(/*description*/ None),
            boolean_schema(/*description*/ None),
        ],
        /*description*/ None,
    );

    expect_repaired(
        validate_and_repair(&schema, r#""false""#, &default_policy()),
        json!(false),
        &[ArgumentRepairRule::BooleanStringTyped],
    );
}

#[test]
fn one_of_candidate_matching_multiple_branches_is_rejected() {
    let schema = one_of_schema(
        vec![
            integer_schema(/*description*/ None),
            number_schema(/*description*/ None),
        ],
        /*description*/ None,
    );

    expect_not_repairable(validate_and_repair(&schema, r#""1""#, &default_policy()));
}

#[test]
fn all_of_candidate_must_satisfy_every_constraint() {
    let schema = all_of_schema(
        vec![
            integer_schema(/*description*/ None),
            enum_schema(vec![json!(2)]),
        ],
        /*description*/ None,
    );

    expect_repaired(
        validate_and_repair(&schema, r#""2""#, &default_policy()),
        json!(2),
        &[ArgumentRepairRule::NumericStringTyped],
    );
    expect_not_repairable(validate_and_repair(&schema, r#""3""#, &default_policy()));
}

#[test]
fn repair_candidate_limit_is_enforced() {
    let schema = closed_object(
        BTreeMap::from([
            ("count".to_string(), integer_schema(/*description*/ None)),
            ("enabled".to_string(), boolean_schema(/*description*/ None)),
        ]),
        &["count", "enabled"],
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repair_candidates = 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#"{"count":"1","enabled":"true"}"#, &policy,),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairCandidates,
        },
    );
}

#[test]
fn repairs_per_call_limit_is_enforced() {
    let schema = closed_object(
        BTreeMap::from([
            ("count".to_string(), integer_schema(/*description*/ None)),
            ("enabled".to_string(), boolean_schema(/*description*/ None)),
        ]),
        &["count", "enabled"],
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repairs_per_call = 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#"{"count":"1","enabled":"true"}"#, &policy,),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairsPerCall,
        },
    );
}

#[test]
fn zero_repair_limit_stops_a_needed_repair() {
    let schema = integer_schema(/*description*/ None);
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repairs_per_call = 0;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#""1""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairsPerCall,
        },
    );
}

#[test]
fn stringified_array_and_nested_item_repair_compose() {
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
        validate_and_repair(&schema, r#"{"ids":"[\"1\"]"}"#, &default_policy()),
        json!({"ids": [1]}),
        &[
            ArgumentRepairRule::StringifiedArrayDecoded,
            ArgumentRepairRule::NumericStringTyped,
        ],
    );
}

#[test]
fn stringified_object_and_nested_boolean_repair_compose() {
    let metadata = closed_object(
        BTreeMap::from([("enabled".to_string(), boolean_schema(/*description*/ None))]),
        &["enabled"],
    );
    let schema = closed_object(
        BTreeMap::from([("metadata".to_string(), metadata)]),
        &["metadata"],
    );

    expect_repaired(
        validate_and_repair(
            &schema,
            r#"{"metadata":"{\"enabled\":\"true\"}"}"#,
            &default_policy(),
        ),
        json!({"metadata": {"enabled": true}}),
        &[
            ArgumentRepairRule::StringifiedObjectDecoded,
            ArgumentRepairRule::BooleanStringTyped,
        ],
    );
}

#[test]
fn scalar_array_wrap_and_item_typing_compose() {
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
        validate_and_repair(&schema, r#"{"ids":"1"}"#, &default_policy()),
        json!({"ids": [1]}),
        &[
            ArgumentRepairRule::ScalarWrappedInArray,
            ArgumentRepairRule::NumericStringTyped,
        ],
    );
}

#[test]
fn alias_sibling_typing_and_optional_null_compose() {
    let schema = closed_object(
        BTreeMap::from([
            ("note".to_string(), string_schema(/*description*/ None)),
            ("query".to_string(), string_schema(/*description*/ None)),
            ("retries".to_string(), integer_schema(/*description*/ None)),
        ]),
        &["query", "retries"],
    );
    let mut policy = default_policy();
    policy
        .insert_known_field_alias("", "q", "query")
        .expect("valid alias policy");

    expect_repaired(
        validate_and_repair(
            &schema,
            r#"{"q":"status","retries":"2","note":null}"#,
            &policy,
        ),
        json!({"query": "status", "retries": 2}),
        &[
            ArgumentRepairRule::OptionalNullRemoved,
            ArgumentRepairRule::KnownFieldAlias,
            ArgumentRepairRule::NumericStringTyped,
        ],
    );
}

#[test]
fn all_of_branches_can_be_repaired_independently() {
    let schema = all_of_schema(
        vec![
            open_object(
                BTreeMap::from([("count".to_string(), integer_schema(/*description*/ None))]),
                &["count"],
            ),
            open_object(
                BTreeMap::from([("enabled".to_string(), boolean_schema(/*description*/ None))]),
                &["enabled"],
            ),
        ],
        /*description*/ None,
    );

    expect_repaired(
        validate_and_repair(
            &schema,
            r#"{"count":"1","enabled":"true"}"#,
            &default_policy(),
        ),
        json!({"count": 1, "enabled": true}),
        &[
            ArgumentRepairRule::NumericStringTyped,
            ArgumentRepairRule::BooleanStringTyped,
        ],
    );
}

#[test]
fn wide_values_consume_the_candidate_cap_before_root_clone_fanout() {
    let properties = (0..1_200)
        .map(|index| {
            (
                format!("field_{index}"),
                integer_schema(/*description*/ None),
            )
        })
        .collect();
    let schema = open_object(properties, &[]);
    let raw = serde_json::to_string(
        &(0..1_200)
            .map(|index| (format!("field_{index}"), "1"))
            .collect::<BTreeMap<_, _>>(),
    )
    .expect("wide fixture should serialize");
    assert!(raw.len() <= 64 * 1024);

    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repair_candidates = 32;
    limits.max_validation_errors = 2_000;
    limits.max_validation_diagnostic_bytes = 100_000;
    policy.set_limits(limits);
    let (outcome, metrics) = validate_and_repair_with_metrics(&schema, &raw, &policy);

    assert_eq!(
        outcome,
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairCandidates,
        }
    );
    assert_eq!(metrics.candidate_work, 32);
}
