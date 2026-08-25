use super::*;
use pretty_assertions::assert_eq;

#[test]
fn alias_policy_rejects_an_oversized_object_path() {
    let mut policy = default_policy();
    let path = format!("/{}", "x".repeat(policy.limits().max_schema_metadata_bytes));

    assert_eq!(
        policy.insert_known_field_alias(&path, "source", "destination"),
        Err(ArgumentRepairPolicyError::PolicyPathTooLong),
    );
}

#[test]
fn policy_entry_counts_are_hard_capped_without_charging_duplicates() {
    let mut aliases = default_policy();
    for index in 0..MAX_ARGUMENT_REPAIR_POLICY_ALIASES {
        aliases
            .insert_known_field_alias(
                "",
                &format!("source_{index}"),
                &format!("destination_{index}"),
            )
            .expect("alias within the hard count cap");
    }
    aliases
        .insert_known_field_alias("", "source_0", "destination_0")
        .expect("an existing alias does not consume another entry");
    assert_eq!(
        aliases.insert_known_field_alias("", "overflow", "destination_overflow"),
        Err(ArgumentRepairPolicyError::PolicyEntryLimitExceeded),
    );

    let mut markdown_paths = default_policy();
    for index in 0..MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATHS {
        markdown_paths
            .allow_markdown_path(&format!("/path_{index}"))
            .expect("markdown path within the hard count cap");
    }
    markdown_paths
        .allow_markdown_path("/path_0")
        .expect("an existing markdown path does not consume another entry");
    assert_eq!(
        markdown_paths.allow_markdown_path("/overflow"),
        Err(ArgumentRepairPolicyError::PolicyEntryLimitExceeded),
    );
}

#[test]
fn policy_collections_enforce_cumulative_byte_caps() {
    let mut aliases = default_policy();
    let alias_path = format!(
        "/{}",
        "a".repeat(MAX_ARGUMENT_REPAIR_POLICY_ALIAS_BYTES - 4)
    );
    aliases
        .insert_known_field_alias(&alias_path, "s", "d")
        .expect("alias metadata just below the cumulative byte cap");
    assert_eq!(
        aliases.insert_known_field_alias("", "x", "y"),
        Err(ArgumentRepairPolicyError::PolicyBytesLimitExceeded),
    );

    let mut markdown_paths = default_policy();
    let markdown_path = format!(
        "/{}",
        "m".repeat(MAX_ARGUMENT_REPAIR_POLICY_MARKDOWN_PATH_BYTES - 2)
    );
    markdown_paths
        .allow_markdown_path(&markdown_path)
        .expect("markdown metadata just below the cumulative byte cap");
    assert_eq!(
        markdown_paths.allow_markdown_path("/x"),
        Err(ArgumentRepairPolicyError::PolicyBytesLimitExceeded),
    );
}
