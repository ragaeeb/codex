use super::*;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::ResponseItem;

fn output(body: FunctionCallOutputBody, metadata: bool) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body,
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: metadata.then(CodexHarnessMetadata::store_backed_tool_output),
    }
}

#[test]
fn fork_inheritance_reads_only_effective_trusted_controls() {
    let artifact_id = "out_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let control = format!(r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#);
    let plain_summary = output(
        FunctionCallOutputBody::Text(control.clone()),
        /*metadata*/ false,
    );
    let trusted = output(
        FunctionCallOutputBody::Text(control.clone()),
        /*metadata*/ true,
    );
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let mixed_legacy = output(
        FunctionCallOutputBody::ContentItems(vec![
            FunctionCallOutputContentItem::InputText { text: control },
            FunctionCallOutputContentItem::InputText {
                text: "untrusted sibling".to_string(),
            },
        ]),
        /*metadata*/ true,
    );

    assert_eq!(
        referenced_output_artifact_ids(&[plain_summary, call, trusted, mixed_legacy]),
        vec![OutputArtifactId::parse(artifact_id).expect("valid artifact id")]
    );
}

#[test]
fn compaction_controls_require_a_matching_call_and_merge_once_per_artifact() {
    let artifact_id = "out_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let control = format!(r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#);
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let trusted = output(
        FunctionCallOutputBody::Text(control),
        /*metadata*/ true,
    );
    let mut orphan = output(
        FunctionCallOutputBody::Text(
            r#"{"type":"tool_output_artifact","artifact_id":"out_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}"#
                .to_string(),
        ),
        /*metadata*/ true,
    );
    let ResponseItem::FunctionCallOutput { call_id, .. } = &mut orphan.item else {
        panic!("expected function output");
    };
    *call_id = Some("orphan-call".to_string());
    let effective = vec![call, trusted.clone(), orphan];

    let controls = artifact_controls_for_compaction(&effective);
    assert_eq!(controls.len(), 2);
    let ResponseItem::FunctionCall {
        name, arguments, ..
    } = &controls[0].item
    else {
        panic!("expected synthetic function call");
    };
    assert_eq!(name, "read_tool_output");
    let arguments: Value = serde_json::from_str(arguments).expect("valid retrieval arguments");
    assert_eq!(arguments["artifact_id"], artifact_id);
    assert_eq!(arguments["mode"], "bytes");
    assert!(controls[1].metadata.is_some());

    let merged = merge_artifact_controls(Vec::new(), controls.clone());
    assert_eq!(merged.len(), 2);
    assert!(matches!(merged[0].item, ResponseItem::FunctionCall { .. }));
    assert_eq!(merge_artifact_controls(vec![trusted], controls).len(), 2);
}

#[test]
fn local_shell_artifact_pairs_are_inherited_and_canonicalized() {
    let artifact_id = "out_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let call_id = "local-shell-call";
    let call = ResponseItemEnvelope::new(ResponseItem::LocalShellCall {
        id: None,
        call_id: Some(call_id.to_string()),
        status: LocalShellStatus::Completed,
        action: LocalShellAction::Exec(LocalShellExecAction {
            command: vec!["echo".to_string(), "ok".to_string()],
            timeout_ms: None,
            working_directory: None,
            env: None,
            user: None,
        }),
        internal_chat_message_metadata_passthrough: None,
    });
    let output = ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text(format!(
                r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#
            )),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
    };

    let controls = artifact_controls_for_compaction(&[call, output]);
    assert_eq!(controls.len(), 2);
    let ResponseItem::FunctionCall {
        name, arguments, ..
    } = &controls[0].item
    else {
        panic!("expected canonical retrieval call");
    };
    assert_eq!(name, "read_tool_output");
    let arguments: Value = serde_json::from_str(arguments).expect("valid retrieval arguments");
    assert_eq!(arguments["artifact_id"], artifact_id);
}

#[test]
fn a_recovery_sidecar_does_not_hide_a_missing_model_control() {
    let artifact_id = "out_dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let control = format!(r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#);
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let trusted = output(
        FunctionCallOutputBody::Text(control),
        /*metadata*/ true,
    );
    let controls = artifact_controls_for_compaction(&[call, trusted]);
    let sidecar_only = vec![ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(
            CodexHarnessMetadata::default()
                .with_store_backed_artifact_references([artifact_id.to_string()]),
        ),
    }];

    let merged = merge_artifact_controls(sidecar_only, controls);
    assert_eq!(merged.len(), 3);
    assert!(matches!(merged[0].item, ResponseItem::FunctionCall { .. }));
    assert!(matches!(
        merged[1].item,
        ResponseItem::FunctionCallOutput { .. }
    ));
}

#[test]
fn compaction_artifact_controls_are_bounded_and_replace_large_call_arguments() {
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "x".repeat(100_000),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let mut history = vec![call];
    for index in 0..100 {
        let digest = format!("{index:064x}");
        let artifact_id = format!("out_{digest}");
        history.push(output(
            FunctionCallOutputBody::Text(format!(
                r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#
            )),
            /*metadata*/ true,
        ));
    }

    let controls = artifact_controls_for_compaction(&history);
    assert!(controls.len() <= 64);
    let serialized_bytes = controls
        .iter()
        .map(|item| {
            serde_json::to_string(&item.item)
                .expect("serialize control")
                .len()
        })
        .sum::<usize>();
    assert!(serialized_bytes <= MAX_COMPACTION_ARTIFACT_CONTROL_BYTES);
    let ResponseItem::FunctionCall { arguments, .. } = &controls[0].item else {
        panic!("expected function call control");
    };
    let arguments: Value = serde_json::from_str(arguments).expect("valid retrieval arguments");
    assert!(arguments["artifact_id"].as_str().is_some());

    let merged = merge_artifact_controls(history, Vec::new());
    assert!(merged.len() <= MAX_COMPACTION_ARTIFACT_CONTROLS * 2);
    let merged_bytes = merged
        .iter()
        .map(|item| {
            serde_json::to_string(&item.item)
                .expect("serialize merged control")
                .len()
        })
        .sum::<usize>();
    assert!(merged_bytes <= MAX_COMPACTION_ARTIFACT_CONTROL_BYTES);
    assert!(merged.iter().all(|item| match &item.item {
        ResponseItem::FunctionCall { arguments, .. } => {
            serde_json::from_str::<Value>(arguments)
                .ok()
                .is_some_and(|value| value["artifact_id"].as_str().is_some())
        }
        _ => true,
    }));
}

#[test]
fn merging_provider_controls_replaces_large_arguments_and_drops_orphan_outputs() {
    let artifact_id = "out_eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let output = output(
        FunctionCallOutputBody::Text(format!(
            r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#
        )),
        /*metadata*/ true,
    );
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_file".to_string(),
        namespace: None,
        arguments: "a".repeat(MAX_COMPACTION_ARTIFACT_CONTROL_PAIR_BYTES * 2),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });

    let merged = merge_artifact_controls(vec![output.clone()], vec![call, output]);
    assert_eq!(merged.len(), 2);
    let ResponseItem::FunctionCall { arguments, .. } = &merged[0].item else {
        panic!("expected canonical function call");
    };
    let arguments: Value = serde_json::from_str(arguments).expect("valid retrieval arguments");
    assert_eq!(arguments["artifact_id"], artifact_id);
}

#[test]
fn artifact_reference_sidecar_is_bounded_and_stays_out_of_model_items() {
    let mut history = vec![ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: Vec::new(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })];
    let ids = (0..400)
        .map(|index| {
            OutputArtifactId::parse(&format!("out_{index:064x}")).expect("generated artifact id")
        })
        .collect::<Vec<_>>();
    attach_artifact_reference_sidecar(&mut history, &ids);
    assert_eq!(referenced_output_artifact_ids(&history).len(), 256);
    assert!(matches!(history[0].item, ResponseItem::Message { .. }));
}

#[test]
fn effective_artifact_references_are_bounded_for_forks() {
    let history = (0..400)
        .flat_map(|index| {
            let call_id = format!("call-{index}");
            let artifact_id = format!("out_{index:064x}");
            vec![
                ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                    id: None,
                    name: "read_file".to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    encrypted_function_args: None,
                    call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                }),
                ResponseItemEnvelope {
                    item: ResponseItem::FunctionCallOutput {
                        id: None,
                        call_id: Some(call_id),
                        name: None,
                        namespace: None,
                        output: FunctionCallOutputPayload::from_text(format!(
                            r#"{{"type":"tool_output_artifact","artifact_id":"{artifact_id}"}}"#
                        )),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
                },
            ]
        })
        .collect::<Vec<_>>();

    assert_eq!(
        referenced_output_artifact_ids(&history).len(),
        MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES
    );
}

#[test]
fn effective_paired_artifacts_win_over_older_sidecar_references() {
    let sidecar_ids = (0..MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES)
        .map(|index| format!("out_{index:064x}"))
        .collect::<Vec<_>>();
    let sidecar = ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(
            CodexHarnessMetadata::default().with_store_backed_artifact_references(sidecar_ids),
        ),
    };
    let paired_id = "out_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_tool_output".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let paired = output(
        FunctionCallOutputBody::Text(format!(
            r#"{{"type":"tool_output_artifact","artifact_id":"{paired_id}"}}"#
        )),
        /*metadata*/ true,
    );
    let ids = referenced_output_artifact_ids(&[sidecar, call, paired]);
    assert_eq!(ids.len(), MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES);
    assert!(ids.iter().any(|id| id.as_str() == paired_id));
}
