use codex_history::CodexHarnessMetadata;
use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::MockServer;

const CONTINUATION: &str = "synthetic continuation after artifact controls";

fn legacy_artifact_control_pair(index: usize) -> [ResponseItemEnvelope; 2] {
    let digest = format!("{index:064x}");
    let artifact_id = format!("out_{digest}");
    let call_id = format!("artifact_ref_{digest}");
    let arguments = json!({
        "artifact_id": artifact_id,
        "mode": "bytes",
        "offset": 0,
        "limit": 1,
    })
    .to_string();
    let output = json!({
        "type": "tool_output_artifact",
        "version": 1,
        "artifact_id": artifact_id,
        "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
    })
    .to_string();
    [
        ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: call_id.clone(),
            name: "read_tool_output".to_string(),
            namespace: None,
            input: arguments,
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope {
            item: ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some("read_tool_output".to_string()),
                output: FunctionCallOutputPayload::from_text(output),
                internal_chat_message_metadata_passthrough: None,
            },
            metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
        },
    ]
}

fn legacy_compacted_fixture(pair_count: usize) -> Vec<RolloutLine> {
    let thread_id = ThreadId::default();
    let mut replacement_history = Vec::with_capacity(pair_count * 2 + 1);
    for index in 0..pair_count {
        replacement_history.extend(legacy_artifact_control_pair(index));
    }
    replacement_history.push(ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: CONTINUATION.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }));
    vec![
        RolloutLine {
            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: thread_id.into(),
                    id: thread_id,
                    parent_thread_id: None,
                    timestamp: "2024-01-01T00:00:00Z".to_string(),
                    cwd: ".".into(),
                    originator: "test_originator".to_string(),
                    cli_version: "test_version".to_string(),
                    model_provider: Some("test-provider".to_string()),
                    ..Default::default()
                },
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2024-01-01T00:00:01.000Z".to_string(),
            ordinal: None,
            item: RolloutItem::Compacted(CompactedItem {
                message: "synthetic compaction summary".to_string(),
                replacement_history: Some(replacement_history),
                mcp_resource_origins: None,
                window_number: Some(1),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            }),
        },
    ]
}

fn assert_artifact_control_request(request: &ResponsesRequest, pair_count: usize) {
    let input = request.input();
    let calls = input
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("custom_tool_call")
                && item.get("name").and_then(Value::as_str) == Some("read_tool_output")
        })
        .collect::<Vec<_>>();
    let outputs = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("custom_tool_call_output"))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), pair_count);
    assert_eq!(outputs.len(), pair_count);
    assert!(input.iter().any(|item| {
        item.get("role").and_then(Value::as_str) == Some("user")
            && item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|content| content.get("text").and_then(Value::as_str) == Some(CONTINUATION))
    }));
    for call in calls {
        let call_id = call["call_id"].as_str().expect("synthetic call ID");
        assert!(call_id.len() <= 64);
        assert_eq!(
            outputs
                .iter()
                .filter(|output| output["call_id"].as_str() == Some(call_id))
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_artifact_control_ids_are_repaired_before_resume_requests() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-legacy-22"),
                ev_completed("resp-legacy-22"),
            ]),
            sse(vec![
                ev_response_created("resp-legacy-20"),
                ev_completed("resp-legacy-20"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex();

    // Counts and the custom-tool wire shape mirror content-free audits of the two affected tasks.
    for pair_count in [22, 20] {
        let rollout = legacy_compacted_fixture(pair_count);
        let tempdir = TempDir::new()?;
        let session_path = tempdir.path().join(format!("legacy-{pair_count}.jsonl"));
        let mut file = std::fs::File::create(&session_path)?;
        for line in rollout {
            writeln!(file, "{}", serde_json::to_string(&line)?)?;
        }
        drop(file);
        let original_rollout = std::fs::read_to_string(&session_path)?;
        let home = Arc::new(TempDir::new()?);
        let resumed = builder
            .resume(&server, Arc::clone(&home), session_path.clone())
            .await?;
        resumed.submit_turn("resume synthetic").await?;
        resumed.codex.submit(Op::Shutdown).await?;
        wait_for_event(&resumed.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete)
        })
        .await;
        let persisted = std::fs::read_to_string(&session_path)?;
        assert!(persisted.starts_with(&original_rollout));
    }

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_artifact_control_request(&requests[0], /*pair_count*/ 22);
    assert_artifact_control_request(&requests[1], /*pair_count*/ 20);
    Ok(())
}
