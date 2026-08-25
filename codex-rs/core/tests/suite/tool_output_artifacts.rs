use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

fn projected_row_text_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => usize::from(text.contains("row-")) * text.len(),
        Value::Array(values) => values.iter().map(projected_row_text_bytes).sum(),
        Value::Object(values) => values.values().map(projected_row_text_bytes).sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_output_is_recoverable_persisted_once_and_survives_resume() -> Result<()> {
    let server = start_mock_server().await;
    let original = serde_json::to_string_pretty(&json!({
        "rows": (0..2_000).map(|index| format!("row-{index:04}-middle-marker")).collect::<Vec<_>>()
    }))?;
    let artifact_id = codex_utils_output_truncation::OutputArtifactId::for_text(&original);
    let tool_response = |response_id, call_id, name, arguments: Value| {
        sse(vec![
            ev_response_created(response_id),
            ev_function_call(call_id, name, &arguments.to_string()),
            ev_completed(response_id),
        ])
    };
    let message = |response_id, message_id, text| {
        sse(vec![
            ev_assistant_message(message_id, text),
            ev_completed(response_id),
        ])
    };
    let initial_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "resp-1",
                "large-output",
                "test_sync_tool",
                json!({"output_json_rows": 2_000}),
            ),
            message("resp-2", "msg-1", "stored"),
            message("resp-compact", "msg-compact", "compact summary"),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| config.tool_output_token_limit = Some(256));
    let initial = builder.build_with_auto_env(&server).await?;
    let home = Arc::clone(&initial.home);
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    initial
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "produce a large result".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut raw_text = None;
    loop {
        let event = initial
            .codex
            .next_event()
            .await
            .context("event stream")?
            .msg;
        match event {
            EventMsg::RawResponseItem(raw) => {
                if let ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } = raw.item
                    && call_id.as_deref() == Some("large-output")
                    && let FunctionCallOutputBody::Text(text) = output.body
                {
                    raw_text = Some(text);
                }
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let model_output = initial_mock
        .requests()
        .get(1)
        .and_then(|request| request.function_call_output_text("large-output"))
        .context("projected model output")?;
    let envelope: Value = serde_json::from_str(&model_output)?;
    assert_eq!(envelope["artifact_id"], artifact_id.as_str());
    assert!(model_output.len() <= 1_024);
    assert!(model_output.len() * 20 < original.len());
    initial.codex.submit(Op::Compact).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let compact_request = initial_mock
        .requests()
        .get(2)
        .context("compact request")?
        .clone();
    let compact_input = serde_json::to_string(&compact_request.input())?;
    assert!(compact_input.contains(artifact_id.as_str()));
    assert!(!compact_input.contains("row-1000-middle-marker"));
    let rollout = std::fs::read_to_string(&rollout_path)?;
    let persisted_output_bytes = rollout
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(projected_row_text_bytes)
        .sum::<usize>();
    assert!(persisted_output_bytes * 10 < original.len());
    assert!(!rollout.contains("row-1000-middle-marker"));
    assert!(rollout.contains(artifact_id.as_str()));

    assert!(raw_text.is_some_and(|text| text.contains("row-1000-middle-marker")));

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let resumed_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response("resp-3", "read-missing", "read_tool_output", json!({
                "artifact_id": format!("out_{}", "0".repeat(64))
            })),
            tool_response("resp-4", "search-middle", "read_tool_output", json!({
                "artifact_id": artifact_id.as_str(), "mode": "search", "query": "row-1000-middle-marker"
            })),
            tool_response("resp-5", "read-middle", "read_tool_output", json!({
                "artifact_id": artifact_id.as_str(), "mode": "lines", "offset": 1_003, "limit": 1
            })),
            message("resp-6", "msg-2", "recovered"),
        ],
    )
    .await;
    let resumed = builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn("recover the middle").await?;
    let resumed_requests = resumed_mock.requests();
    assert!(serde_json::to_string(&resumed_requests[0].input())?.contains(artifact_id.as_str()));
    let missing_output = resumed_requests
        .get(1)
        .and_then(|request| request.function_call_output_text("read-missing"))
        .context("missing artifact error")?;
    assert_eq!(missing_output, "output artifact is unavailable or expired");
    let search_output = resumed_requests
        .get(2)
        .and_then(|request| request.function_call_output_text("search-middle"))
        .context("artifact search")?;
    assert!(serde_json::from_str::<Value>(&search_output)?["byte_offsets"][0].is_number());
    let window_output = resumed_requests
        .get(3)
        .and_then(|request| request.function_call_output_text("read-middle"))
        .context("artifact window")?;
    let window: Value = serde_json::from_str(&window_output)?;
    assert!(
        window["text"]
            .as_str()
            .is_some_and(|text| text.contains("row-1000-middle-marker"))
    );
    assert!(window["complete"].is_boolean());
    Ok(())
}
