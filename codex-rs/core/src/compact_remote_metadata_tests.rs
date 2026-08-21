use super::*;
use codex_history::CodexHarnessMetadata;

#[test]
fn rewritten_output_preserves_harness_metadata() {
    let envelope = ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("large output".repeat(100)),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata::default()),
    };

    let rewritten = rewritten_output_for_context_window(&envelope)
        .expect("function output should be rewritten");

    assert_eq!(rewritten.metadata, envelope.metadata);
    assert_ne!(rewritten.item, envelope.item);
}

#[test]
fn rewriting_a_store_backed_output_drops_unrecoverable_metadata() {
    let envelope = ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text(
                r#"{"type":"tool_output_artifact","artifact_id":"out_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
                    .to_string(),
            ),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
    };

    let rewritten = rewritten_output_for_context_window(&envelope).expect("rewritten output");

    assert_eq!(rewritten.metadata, None);
}

#[test]
fn remote_compaction_reattaches_store_backed_metadata_to_exact_items() {
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("call-1".to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(
            r#"{"type":"tool_output_artifact","artifact_id":"out_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
                .to_string(),
        ),
        internal_chat_message_metadata_passthrough: None,
    };
    let source = ResponseItemEnvelope {
        item: item.clone(),
        metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
    };

    let annotated = annotate_store_backed_history(vec![item.clone()], &[source]);
    assert!(
        annotated[0]
            .metadata
            .as_ref()
            .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
    );

    let mut changed = item;
    let ResponseItem::FunctionCallOutput { output, .. } = &mut changed else {
        panic!("expected function output")
    };
    output.body = FunctionCallOutputBody::Text("different".to_string());
    let unannotated = annotate_store_backed_history(vec![changed], &annotated);
    assert_eq!(unannotated[0].metadata, None);
}

#[test]
fn store_backed_tool_outputs_are_not_filtered_from_compaction_groups() {
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let group = HistoryItemGroup {
        source: ResponseItemEnvelope {
            item: ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call-1".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text(
                    r#"{"type":"tool_output_artifact","artifact_id":"out_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
                        .to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
        },
        attached_notice: None,
    };
    let paired = paired_store_backed_call_ids(&[call, group.source.clone()]);
    assert!(should_keep_compacted_history_group(&group, &paired));
}

#[test]
fn orphan_store_backed_tool_outputs_are_filtered_from_compaction_groups() {
    let group = HistoryItemGroup {
        source: ResponseItemEnvelope {
            item: ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("orphan-call".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text(
                    r#"{"type":"tool_output_artifact","artifact_id":"out_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
                        .to_string(),
                ),
                internal_chat_message_metadata_passthrough: None,
            },
            metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
        },
        attached_notice: None,
    };
    let paired = paired_store_backed_call_ids(std::slice::from_ref(&group.source));
    assert!(!should_keep_compacted_history_group(&group, &paired));
}
