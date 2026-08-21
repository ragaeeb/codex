use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use core_test_support::test_codex::executor_path_uri;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;

fn read_call(call_id: &str, arguments: Value) -> Value {
    ev_function_call(
        call_id,
        "read_file",
        &serde_json::to_string(&arguments).expect("read_file arguments should serialize"),
    )
}

fn tool_response(response_id: &str, calls: Vec<Value>) -> String {
    let mut events = vec![ev_response_created(response_id)];
    events.extend(calls);
    events.push(ev_completed(response_id));
    sse(events)
}

fn assistant_response(response_id: &str, message_id: &str) -> String {
    sse(vec![
        ev_assistant_message(message_id, "read_file test complete"),
        ev_completed(response_id),
    ])
}

fn output(mock: &ResponseMock, call_id: &str) -> Result<String> {
    mock.function_call_output_text(call_id)
        .with_context(|| format!("function output for {call_id}"))
}

fn response_json(mock: &ResponseMock, call_id: &str) -> Result<Value> {
    Ok(serde_json::from_str(&output(mock, call_id)?)?)
}

fn persisted_read_output_bytes(rollout: &str, call_prefix: &str) -> Result<usize> {
    let mut total = 0;
    for line in rollout.lines() {
        let line: RolloutLine = serde_json::from_str(line)?;
        let RolloutItem::ResponseItem(envelope) = line.item else {
            continue;
        };
        let ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            output,
            ..
        } = envelope.item
        else {
            continue;
        };
        if call_id.starts_with(call_prefix) {
            total += output.body.to_text().unwrap_or_default().len();
        }
    }
    Ok(total)
}

fn read_file_enabled_builder() -> core_test_support::test_codex::TestCodexBuilder {
    test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::NativeReadFile)
            .expect("test config should enable native read_file");
    })
}

#[path = "read_file_basic.rs"]
mod read_file_basic;
#[path = "read_file_dedup.rs"]
mod read_file_dedup;
#[path = "read_file_safety.rs"]
mod read_file_safety;
