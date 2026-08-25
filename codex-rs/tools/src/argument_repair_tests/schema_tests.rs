use super::*;
use pretty_assertions::assert_eq;

#[test]
fn local_defs_reference_is_resolved_for_validation_and_repair() {
    let mut schema = closed_object(
        BTreeMap::from([(
            "count".to_string(),
            JsonSchema {
                schema_ref: Some("#/$defs/Count".to_string()),
                ..Default::default()
            },
        )]),
        &["count"],
    );
    schema.defs = Some(BTreeMap::from([(
        "Count".to_string(),
        integer_schema(/*description*/ None),
    )]));

    expect_repaired(
        validate_and_repair(&schema, r#"{"count":"7"}"#, &default_policy()),
        json!({"count": 7}),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn nested_and_percent_encoded_local_reference_is_resolved() {
    let mut schema = closed_object(
        BTreeMap::from([(
            "count".to_string(),
            JsonSchema {
                schema_ref: Some("#/%24defs/Thing%20Type/properties/count".to_string()),
                ..Default::default()
            },
        )]),
        &["count"],
    );
    schema.defs = Some(BTreeMap::from([(
        "Thing Type".to_string(),
        open_object(
            BTreeMap::from([("count".to_string(), integer_schema(/*description*/ None))]),
            &[],
        ),
    )]));

    expect_repaired(
        validate_and_repair(&schema, r#"{"count":"8"}"#, &default_policy()),
        json!({"count": 8}),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn legacy_definitions_reference_is_resolved() {
    let mut schema = JsonSchema {
        schema_ref: Some("#/definitions/Enabled".to_string()),
        ..Default::default()
    };
    schema.definitions = Some(BTreeMap::from([(
        "Enabled".to_string(),
        boolean_schema(/*description*/ None),
    )]));

    expect_repaired(
        validate_and_repair(&schema, r#""true""#, &default_policy()),
        json!(true),
        &[ArgumentRepairRule::BooleanStringTyped],
    );
}

#[test]
fn missing_external_and_cyclic_references_are_explicitly_unsupported() {
    let missing = JsonSchema {
        schema_ref: Some("#/$defs/Missing".to_string()),
        ..Default::default()
    };
    assert_eq!(
        validate_and_repair(&missing, "null", &default_policy()),
        ArgumentRepairOutcome::UnsupportedSchema {
            reason: UnsupportedSchemaReason::MissingReference,
        },
    );

    let external = JsonSchema {
        schema_ref: Some("https://example.invalid/schema".to_string()),
        ..Default::default()
    };
    assert_eq!(
        validate_and_repair(&external, "null", &default_policy()),
        ArgumentRepairOutcome::UnsupportedSchema {
            reason: UnsupportedSchemaReason::ExternalReference,
        },
    );

    let mut cyclic = JsonSchema {
        schema_ref: Some("#/$defs/Node".to_string()),
        ..Default::default()
    };
    cyclic.defs = Some(BTreeMap::from([(
        "Node".to_string(),
        JsonSchema {
            schema_ref: Some("#/$defs/Node".to_string()),
            ..Default::default()
        },
    )]));
    assert_eq!(
        validate_and_repair(&cyclic, "null", &default_policy()),
        ArgumentRepairOutcome::UnsupportedSchema {
            reason: UnsupportedSchemaReason::ReferenceCycle,
        },
    );
}

#[test]
fn schema_value_reference_and_input_limits_are_enforced() {
    let deep_schema = array_schema(
        array_schema(
            string_schema(/*description*/ None),
            /*description*/ None,
        ),
        /*description*/ None,
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_depth = 1;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&deep_schema, "[]", &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::SchemaDepth,
        },
    );

    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_value_depth = 1;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&JsonSchema::default(), "[[0]]", &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::ValueDepth,
        },
    );

    let mut referenced = JsonSchema {
        schema_ref: Some("#/$defs/Value".to_string()),
        ..Default::default()
    };
    referenced.defs = Some(BTreeMap::from([(
        "Value".to_string(),
        string_schema(/*description*/ None),
    )]));
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_references = 0;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&referenced, r#""value""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::References,
        },
    );

    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_input_bytes = 3;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&JsonSchema::default(), "null", &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::InputBytes,
        },
    );
}

#[test]
fn schema_node_limit_is_enforced() {
    let schema = open_object(
        BTreeMap::from([("value".to_string(), string_schema(/*description*/ None))]),
        &[],
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_nodes = 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, "{}", &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::SchemaNodes,
        },
    );
}

#[test]
fn invalid_local_reference_syntax_is_explicitly_unsupported() {
    let schema = JsonSchema {
        schema_ref: Some("#/$defs/bad~2token".to_string()),
        ..Default::default()
    };

    assert_eq!(
        validate_and_repair(&schema, "null", &default_policy()),
        ArgumentRepairOutcome::UnsupportedSchema {
            reason: UnsupportedSchemaReason::InvalidReference,
        },
    );
}

#[test]
fn decoded_candidate_value_depth_is_bounded_before_matching() {
    let schema = array_schema(JsonSchema::default(), /*description*/ None);
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_value_depth = 1;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(&schema, r#""[[0]]""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::ValueDepth,
        },
    );
}

#[test]
fn shared_reference_is_checked_again_at_deeper_reuse_site() {
    let reference = JsonSchema {
        schema_ref: Some("#/$defs/Leaf".to_string()),
        ..Default::default()
    };
    let deep = closed_object(
        BTreeMap::from([(
            "level".to_string(),
            closed_object(
                BTreeMap::from([("level".to_string(), reference.clone())]),
                &["level"],
            ),
        )]),
        &["level"],
    );
    let mut schema = open_object(
        BTreeMap::from([
            ("a_shallow".to_string(), reference),
            ("z_deep".to_string(), deep),
        ]),
        &[],
    );
    schema.defs = Some(BTreeMap::from([(
        "Leaf".to_string(),
        integer_schema(/*description*/ None),
    )]));
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_depth = 3;
    policy.set_limits(limits);

    assert_eq!(
        validate_and_repair(
            &schema,
            r#"{"a_shallow":"1","z_deep":{"level":{"level":"2"}}}"#,
            &policy
        ),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::SchemaDepth,
        },
    );
}

#[test]
fn local_reference_routes_cover_items_and_additional_properties() {
    let reference = JsonSchema {
        schema_ref: Some("#/$defs/Count".to_string()),
        ..Default::default()
    };
    let mut items_schema = array_schema(reference.clone(), /*description*/ None);
    items_schema.defs = Some(BTreeMap::from([(
        "Count".to_string(),
        integer_schema(/*description*/ None),
    )]));
    expect_repaired(
        validate_and_repair(&items_schema, r#"["1"]"#, &default_policy()),
        json!([1]),
        &[ArgumentRepairRule::NumericStringTyped],
    );

    let mut additional_schema = object_with_additional(
        BTreeMap::new(),
        &[],
        Some(AdditionalProperties::Schema(Box::new(reference))),
    );
    additional_schema.defs = Some(BTreeMap::from([(
        "Count".to_string(),
        integer_schema(/*description*/ None),
    )]));
    expect_repaired(
        validate_and_repair(
            &additional_schema,
            r#"{"secret-key":"2"}"#,
            &default_policy(),
        ),
        json!({"secret-key": 2}),
        &[ArgumentRepairRule::NumericStringTyped],
    );
}

#[test]
fn indexed_any_one_and_all_of_references_are_resolved() {
    for (keyword, variants) in [
        (
            "anyOf",
            vec![
                integer_schema(/*description*/ None),
                boolean_schema(/*description*/ None),
            ],
        ),
        (
            "oneOf",
            vec![
                integer_schema(/*description*/ None),
                boolean_schema(/*description*/ None),
            ],
        ),
        (
            "allOf",
            vec![integer_schema(/*description*/ None), JsonSchema::default()],
        ),
    ] {
        let schema_ref = format!("#/{keyword}/0");
        let mut schema = JsonSchema {
            schema_ref: Some(schema_ref),
            ..Default::default()
        };
        match keyword {
            "anyOf" => schema.any_of = Some(variants),
            "oneOf" => schema.one_of = Some(variants),
            "allOf" => schema.all_of = Some(variants),
            _ => unreachable!("test table contains only supported keywords"),
        }
        expect_repaired(
            validate_and_repair(&schema, r#""1""#, &default_policy()),
            json!(1),
            &[ArgumentRepairRule::NumericStringTyped],
        );
    }
}

#[test]
fn schema_resource_limits_have_exact_boundaries() {
    let mut schema = JsonSchema {
        description: Some("x".to_string()),
        ..Default::default()
    };
    for (limit, expected) in [
        (0, ArgumentRepairLimit::SchemaBytes),
        (1, ArgumentRepairLimit::SchemaMetadataBytes),
    ] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        if limit == 0 {
            limits.max_schema_bytes = 0;
        } else {
            limits.max_schema_metadata_bytes = 0;
        }
        policy.set_limits(limits);
        assert_eq!(
            validate_and_repair(&schema, "null", &policy),
            ArgumentRepairOutcome::LimitExceeded { limit: expected },
        );
    }
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_metadata_bytes = 1;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&schema, "null", &policy),
        ArgumentRepairOutcome::ValidUnchanged {
            raw_arguments: "null".to_string(),
        },
    );

    schema = closed_object(
        BTreeMap::from([("x".to_string(), string_schema(/*description*/ None))]),
        &[],
    );
    for (max_name_bytes, expected) in [
        (0, ArgumentRepairLimit::SchemaNameBytes),
        (1, ArgumentRepairLimit::SchemaBytes),
    ] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        if max_name_bytes == 0 {
            limits.max_schema_name_bytes = max_name_bytes;
        } else {
            limits.max_schema_bytes = 1;
        }
        policy.set_limits(limits);
        assert_eq!(
            validate_and_repair(&schema, "{}", &policy),
            ArgumentRepairOutcome::LimitExceeded { limit: expected },
        );
    }
}

#[test]
fn enum_and_repaired_output_limits_have_exact_boundaries() {
    let enum_schema = enum_schema(vec![json!("x")]);
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_enum_values = 0;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&enum_schema, r#""x""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::SchemaEnumValues,
        },
    );
    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_schema_enum_bytes = 2;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&enum_schema, r#""x""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::SchemaEnumBytes,
        },
    );
    for enum_limit in [1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_enum_values = enum_limit;
        limits.max_schema_enum_bytes = enum_limit.max(3);
        policy.set_limits(limits);
        assert_eq!(
            validate_and_repair(&enum_schema, r#""x""#, &policy),
            ArgumentRepairOutcome::ValidUnchanged {
                raw_arguments: r#""x""#.to_string(),
            },
        );
    }

    let mut policy = default_policy();
    let mut limits = policy.limits().clone();
    limits.max_repaired_output_bytes = 0;
    policy.set_limits(limits);
    assert_eq!(
        validate_and_repair(&integer_schema(/*description*/ None), r#""1""#, &policy),
        ArgumentRepairOutcome::LimitExceeded {
            limit: ArgumentRepairLimit::RepairedOutputBytes,
        },
    );
    for output_limit in [1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_repaired_output_bytes = output_limit;
        policy.set_limits(limits);
        expect_repaired(
            validate_and_repair(&integer_schema(/*description*/ None), r#""1""#, &policy),
            json!(1),
            &[ArgumentRepairRule::NumericStringTyped],
        );
    }
}
