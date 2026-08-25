use super::*;
use pretty_assertions::assert_eq;

#[test]
fn every_resource_limit_has_n_minus_one_n_and_n_plus_one_boundaries() {
    let assert_unchanged = |outcome: ArgumentRepairOutcome, raw: &str| {
        assert_eq!(
            outcome,
            ArgumentRepairOutcome::ValidUnchanged {
                raw_arguments: raw.to_string(),
            },
        );
    };

    for max_input_bytes in [2, 3, 4] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_input_bytes = max_input_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&JsonSchema::default(), "123", &policy);
        if max_input_bytes == 2 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::InputBytes,
                },
            );
        } else {
            assert_unchanged(outcome, "123");
        }
    }

    let schema = array_schema(
        string_schema(/*description*/ None),
        /*description*/ None,
    );
    for max_schema_depth in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_depth = max_schema_depth;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "[]", &policy);
        if max_schema_depth == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaDepth,
                },
            );
        } else {
            assert_unchanged(outcome, "[]");
        }
    }

    let schema = open_object(
        BTreeMap::from([("x".to_string(), string_schema(/*description*/ None))]),
        &[],
    );
    for max_schema_nodes in [1, 2, 3] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_nodes = max_schema_nodes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "{}", &policy);
        if max_schema_nodes == 1 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaNodes,
                },
            );
        } else {
            assert_unchanged(outcome, "{}");
        }
    }

    for max_schema_bytes in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_bytes = max_schema_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&JsonSchema::default(), "null", &policy);
        if max_schema_bytes == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaBytes,
                },
            );
        } else {
            assert_unchanged(outcome, "null");
        }
    }

    let schema = JsonSchema {
        description: Some("x".to_string()),
        ..Default::default()
    };
    for max_schema_metadata_bytes in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_metadata_bytes = max_schema_metadata_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "null", &policy);
        if max_schema_metadata_bytes == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaMetadataBytes,
                },
            );
        } else {
            assert_unchanged(outcome, "null");
        }
    }

    let schema = open_object(
        BTreeMap::from([("x".to_string(), string_schema(/*description*/ None))]),
        &[],
    );
    for max_schema_name_bytes in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_name_bytes = max_schema_name_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "{}", &policy);
        if max_schema_name_bytes == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaNameBytes,
                },
            );
        } else {
            assert_unchanged(outcome, "{}");
        }
    }

    let mut referenced = JsonSchema {
        schema_ref: Some("#/$defs/x".to_string()),
        ..Default::default()
    };
    referenced.defs = Some(BTreeMap::from([("x".to_string(), JsonSchema::default())]));
    for max_schema_name_bytes in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_name_bytes = max_schema_name_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&referenced, "null", &policy);
        if max_schema_name_bytes == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaNameBytes,
                },
            );
        } else {
            assert_unchanged(outcome, "null");
        }
    }

    let schema = enum_schema(vec![json!("x")]);
    for max_schema_enum_values in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_enum_values = max_schema_enum_values;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, r#""x""#, &policy);
        if max_schema_enum_values == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaEnumValues,
                },
            );
        } else {
            assert_unchanged(outcome, r#""x""#);
        }
    }

    for max_schema_enum_bytes in [2, 3, 4] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_schema_enum_bytes = max_schema_enum_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, r#""x""#, &policy);
        if max_schema_enum_bytes == 2 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::SchemaEnumBytes,
                },
            );
        } else {
            assert_unchanged(outcome, r#""x""#);
        }
    }

    for max_value_depth in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_value_depth = max_value_depth;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&JsonSchema::default(), "[0]", &policy);
        if max_value_depth == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::ValueDepth,
                },
            );
        } else {
            assert_unchanged(outcome, "[0]");
        }
    }

    for max_references in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_references = max_references;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&referenced, "null", &policy);
        if max_references == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::References,
                },
            );
        } else {
            assert_unchanged(outcome, "null");
        }
    }

    let schema = closed_object(
        BTreeMap::from([("x".to_string(), string_schema(/*description*/ None))]),
        &["x"],
    );
    for max_validation_errors in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_validation_errors = max_validation_errors;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "{}", &policy);
        if max_validation_errors == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::ValidationErrors,
                },
            );
        } else {
            assert!(matches!(
                outcome,
                ArgumentRepairOutcome::NotRepairable { .. }
            ));
        }
    }

    let diagnostic_bytes = "/<property>".len() + 16;
    for max_validation_diagnostic_bytes in
        [diagnostic_bytes - 1, diagnostic_bytes, diagnostic_bytes + 1]
    {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_validation_diagnostic_bytes = max_validation_diagnostic_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&schema, "{}", &policy);
        if max_validation_diagnostic_bytes == diagnostic_bytes - 1 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::ValidationDiagnosticBytes,
                },
            );
        } else {
            assert!(matches!(
                outcome,
                ArgumentRepairOutcome::NotRepairable { .. }
            ));
        }
    }

    for max_repair_candidates in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_repair_candidates = max_repair_candidates;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&integer_schema(/*description*/ None), r#""1""#, &policy);
        if max_repair_candidates == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::RepairCandidates,
                },
            );
        } else {
            expect_repaired(outcome, json!(1), &[ArgumentRepairRule::NumericStringTyped]);
        }
    }

    let repair_schema = closed_object(
        BTreeMap::from([
            ("count".to_string(), integer_schema(/*description*/ None)),
            ("enabled".to_string(), boolean_schema(/*description*/ None)),
        ]),
        &["count", "enabled"],
    );
    for max_repairs_per_call in [1, 2, 3] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_repairs_per_call = max_repairs_per_call;
        policy.set_limits(limits);
        let outcome =
            validate_and_repair(&repair_schema, r#"{"count":"1","enabled":"true"}"#, &policy);
        if max_repairs_per_call == 1 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::RepairsPerCall,
                },
            );
        } else {
            expect_repaired(
                outcome,
                json!({"count": 1, "enabled": true}),
                &[
                    ArgumentRepairRule::NumericStringTyped,
                    ArgumentRepairRule::BooleanStringTyped,
                ],
            );
        }
    }

    for max_repaired_output_bytes in [0, 1, 2] {
        let mut policy = default_policy();
        let mut limits = policy.limits().clone();
        limits.max_repaired_output_bytes = max_repaired_output_bytes;
        policy.set_limits(limits);
        let outcome = validate_and_repair(&integer_schema(/*description*/ None), r#""1""#, &policy);
        if max_repaired_output_bytes == 0 {
            assert_eq!(
                outcome,
                ArgumentRepairOutcome::LimitExceeded {
                    limit: ArgumentRepairLimit::RepairedOutputBytes,
                },
            );
        } else {
            expect_repaired(outcome, json!(1), &[ArgumentRepairRule::NumericStringTyped]);
        }
    }
}
