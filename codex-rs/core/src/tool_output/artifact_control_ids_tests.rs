use super::*;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

fn artifact_id(fill: char) -> OutputArtifactId {
    OutputArtifactId::parse(&format!("out_{}", fill.to_string().repeat(64)))
        .expect("synthetic artifact ID")
}

fn legacy_pair(artifact_id: &OutputArtifactId, metadata: bool) -> Vec<ResponseItemEnvelope> {
    let call_id = format!("{LEGACY_SYNTHETIC_CALL_ID_PREFIX}{}", artifact_id.digest());
    vec![
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "read_tool_output".to_string(),
            namespace: None,
            arguments: synthetic_retrieval_arguments(artifact_id),
            encrypted_function_args: None,
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope {
            item: ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some(call_id),
                name: Some("read_tool_output".to_string()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text(synthetic_artifact_control_body(
                    artifact_id,
                )),
                internal_chat_message_metadata_passthrough: None,
            },
            metadata: metadata.then(CodexHarnessMetadata::store_backed_tool_output),
        },
    ]
}

#[test]
fn synthetic_ids_are_stable_distinct_and_provider_safe() {
    let first = artifact_id('a');
    let second = artifact_id('b');
    let mut allocator = SyntheticArtifactCallIdAllocator::for_history(&[]);

    let first_id = allocator.next(&first).expect("first synthetic ID");
    let first_again = allocator.next(&first).expect("stable synthetic ID");
    let second_id = allocator.next(&second).expect("second synthetic ID");

    assert_eq!(first_id, first_again);
    assert_ne!(first_id, second_id);
    assert!(first_id.len() <= MAX_PROVIDER_IDENTIFIER_BYTES);
    assert!(second_id.len() <= MAX_PROVIDER_IDENTIFIER_BYTES);
    assert!(first_id.starts_with(LEGACY_SYNTHETIC_CALL_ID_PREFIX));
}

#[test]
fn synthetic_id_collision_resolution_is_deterministic_and_bounded() {
    let artifact = artifact_id('c');
    let first_candidate = synthetic_candidate(&artifact, /*ordinal*/ 0);
    let mut allocator = SyntheticArtifactCallIdAllocator {
        used_ids: [first_candidate].into_iter().collect(),
        assignments: HashMap::new(),
    };

    let resolved = allocator.next(&artifact).expect("collision fallback ID");
    assert_ne!(resolved, synthetic_candidate(&artifact, /*ordinal*/ 0));
    assert_eq!(resolved, synthetic_candidate(&artifact, /*ordinal*/ 1));
    assert!(resolved.len() <= MAX_PROVIDER_IDENTIFIER_BYTES);
    assert_eq!(allocator.next(&artifact), Some(resolved));
}

#[test]
fn trusted_legacy_pairs_are_repaired_but_untrusted_pairs_are_not() {
    let trusted_artifact = artifact_id('d');
    let untrusted_artifact = artifact_id('e');
    let mut trusted = legacy_pair(&trusted_artifact, /*metadata*/ true);
    let mut untrusted = legacy_pair(&untrusted_artifact, /*metadata*/ false);
    let legacy_trusted_id = response_call_id(&trusted[0].item)
        .expect("legacy call ID")
        .to_string();
    let legacy_untrusted_id = response_call_id(&untrusted[0].item)
        .expect("legacy call ID")
        .to_string();

    canonicalize_legacy_artifact_controls(&mut trusted);
    canonicalize_legacy_artifact_controls(&mut untrusted);

    let repaired_id = response_call_id(&trusted[0].item).expect("repaired call ID");
    assert_ne!(repaired_id, legacy_trusted_id);
    assert!(repaired_id.len() <= MAX_PROVIDER_IDENTIFIER_BYTES);
    assert_eq!(response_output_call_id(&trusted[1].item), Some(repaired_id));
    assert_eq!(
        response_call_id(&untrusted[0].item),
        Some(legacy_untrusted_id.as_str())
    );
    assert_eq!(
        response_output_call_id(&untrusted[1].item),
        Some(legacy_untrusted_id.as_str())
    );
}

#[test]
fn provider_call_id_validation_preserves_permissive_item_ids() {
    let oversized = "x".repeat(MAX_PROVIDER_IDENTIFIER_BYTES + 1);
    let oversized_item_id = codex_protocol::ResponseItemId::from_server(format!("fc_{oversized}"));
    let mut items = vec![ResponseItem::FunctionCall {
        id: Some(oversized_item_id),
        name: "read_tool_output".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "provider-safe-call-id".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }];
    assert!(provider_call_ids_within_limit(&items));

    let ResponseItem::FunctionCall { call_id, .. } = &mut items[0] else {
        panic!("expected function call");
    };
    *call_id = oversized;
    assert!(!provider_call_ids_within_limit(&items));
}
