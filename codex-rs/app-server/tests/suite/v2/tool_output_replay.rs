use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::to_response;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolFunctionSpec;
use codex_app_server_protocol::DynamicToolNamespaceSpec;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(any(target_os = "macos", windows))]
const READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_thread_reads_and_resumes_with_the_durable_display_projection() -> Result<()> {
    let call_id = "large-dynamic-output";
    let namespace = "test_output";
    let tool = "produce_large_output";
    let responses = vec![
        responses::sse(vec![
            responses::ev_response_created("resp-tool"),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": call_id,
                    "namespace": namespace,
                    "name": tool,
                    "arguments": "{}",
                }
            }),
            responses::ev_completed("resp-tool"),
        ]),
        create_final_assistant_message_sse_response("Done")?,
    ];
    let responses_server = create_mock_responses_server_sequence_unchecked(responses).await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&responses_server.uri())
        .with_root_config("tool_output_token_limit = 256")
        .write(codex_home.path())?;

    let dynamic_tool = DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: namespace.to_string(),
        description: "Synthetic output tools".to_string(),
        tools: vec![DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec {
                name: tool.to_string(),
                description: "Produce synthetic output".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                defer_loading: false,
            },
        )],
    });
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(READ_TIMEOUT, app.initialize()).await??;
    let start_id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            dynamic_tools: Some(vec![dynamic_tool]),
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?;
    let start_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_response)?;
    let thread_id = thread.id;

    let turn_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            client_user_message_id: None,
            input: vec![UserInput::Text {
                text: "Produce a large result".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    let _: TurnStartResponse = to_response(turn_response)?;
    let request = timeout(READ_TIMEOUT, app.read_stream_until_request_message()).await??;
    let ServerRequest::DynamicToolCall { request_id, .. } = request else {
        panic!("expected dynamic tool call request")
    };
    let original = format!(
        "readable head\n{}UNIQUE_MIDDLE_MARKER\n{}readable tail",
        "before middle\n".repeat(4_000),
        "after middle\n".repeat(4_000),
    );
    app.send_response(
        request_id,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: original.clone(),
            }],
            success: true,
        })?,
    )
    .await?;

    let completed = wait_for_dynamic_tool_completed(&mut app, call_id).await?;
    assert_eq!(
        dynamic_output(std::slice::from_ref(&completed.item), call_id),
        original
    );
    timeout(
        READ_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    app.shutdown_gracefully().await?;
    drop(app);

    let mut reopened = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let read_id = reopened
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: true,
        })
        .await?;
    let ThreadReadResponse { thread, .. } =
        timeout(READ_TIMEOUT, reopened.read_response(read_id)).await??;
    let read_output = dynamic_output_from_turns(&thread.turns, call_id);
    assert_durable_projection(read_output, original.len());

    let resume_id = reopened
        .send_thread_resume_request(ThreadResumeParams {
            thread_id,
            ..Default::default()
        })
        .await?;
    let ThreadResumeResponse { thread, .. } =
        timeout(READ_TIMEOUT, reopened.read_response(resume_id)).await??;
    let resumed_output = dynamic_output_from_turns(&thread.turns, call_id);
    assert_eq!(resumed_output, read_output);
    reopened.shutdown_gracefully().await?;
    Ok(())
}

fn assert_durable_projection(output: &str, original_bytes: usize) {
    assert!(output.contains("readable head"));
    assert!(output.contains("readable tail"));
    assert!(!output.contains("UNIQUE_MIDDLE_MARKER"));
    assert!(!output.trim_start().starts_with('{'));
    assert!(output.len() <= 1_024);
    assert!(output.len() * 20 < original_bytes);
}

fn dynamic_output_from_turns<'a>(turns: &'a [Turn], call_id: &str) -> &'a str {
    turns
        .iter()
        .flat_map(|turn| &turn.items)
        .find_map(|item| dynamic_output_from_item(item, call_id))
        .expect("persisted dynamic tool output")
}

fn dynamic_output<'a>(items: &'a [ThreadItem], call_id: &str) -> &'a str {
    items
        .iter()
        .find_map(|item| dynamic_output_from_item(item, call_id))
        .expect("persisted dynamic tool output")
}

fn dynamic_output_from_item<'a>(item: &'a ThreadItem, call_id: &str) -> Option<&'a str> {
    let ThreadItem::DynamicToolCall {
        id,
        content_items: Some(content_items),
        ..
    } = item
    else {
        return None;
    };
    (id == call_id).then(|| {
        content_items
            .iter()
            .find_map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => Some(text.as_str()),
                DynamicToolCallOutputContentItem::InputImage { .. }
                | DynamicToolCallOutputContentItem::InputAudio { .. } => None,
            })
            .expect("dynamic tool text output")
    })
}

async fn wait_for_dynamic_tool_completed(
    app: &mut TestAppServer,
    call_id: &str,
) -> Result<ItemCompletedNotification> {
    loop {
        let notification: JSONRPCNotification = timeout(
            READ_TIMEOUT,
            app.read_stream_until_notification_message("item/completed"),
        )
        .await??;
        let Some(params) = notification.params else {
            continue;
        };
        let completed: ItemCompletedNotification = serde_json::from_value(params)?;
        if matches!(&completed.item, ThreadItem::DynamicToolCall { id, .. } if id == call_id) {
            return Ok(completed);
        }
    }
}
