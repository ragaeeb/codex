use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_exec_server::WriteFileOptions;
use codex_features::Feature;
use core_test_support::responses;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

struct ReadFileResponses {
    request_count: AtomicUsize,
}

fn artifact_id_from_model_input(body: &Value) -> String {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|item| {
            let output = item["output"].as_str()?;
            let output: Value = serde_json::from_str(output).ok()?;
            (output["type"] == "tool_output_artifact")
                .then(|| output["artifact_id"].as_str().map(str::to_string))
                .flatten()
        })
        .expect("resumed model input should contain a tool_output_artifact envelope")
}

fn parse_tool_output(label: &str, output: &str) -> Result<Value> {
    serde_json::from_str(output).with_context(|| format!("{label} output was not JSON: {output:?}"))
}

impl Respond for ReadFileResponses {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let request_number = self.request_count.fetch_add(1, Ordering::SeqCst);
        let body = match request_number {
            0 => responses::sse(vec![
                responses::ev_response_created("read-file-response"),
                responses::ev_function_call(
                    "read-file-call-1",
                    "read_file",
                    &json!({"path": "app-server-read.txt", "max_bytes": 32 * 1024}).to_string(),
                ),
                responses::ev_function_call(
                    "read-file-call-2",
                    "read_file",
                    &json!({"path": "app-server-read.txt", "max_bytes": 32 * 1024}).to_string(),
                ),
                responses::ev_completed("read-file-response"),
            ]),
            1 => responses::sse(vec![
                responses::ev_assistant_message("read-file-message", "done"),
                responses::ev_completed("read-file-followup"),
            ]),
            2 => responses::sse(vec![
                responses::ev_assistant_message("read-file-compaction-message", "LOCAL_SUMMARY"),
                responses::ev_completed("read-file-compaction-response"),
            ]),
            3 => responses::sse(vec![
                responses::ev_response_created("read-file-after-reopen-response"),
                responses::ev_function_call(
                    "read-file-call-after-reopen",
                    "read_file",
                    &json!({"path": "app-server-read.txt", "max_bytes": 32 * 1024}).to_string(),
                ),
                responses::ev_completed("read-file-after-reopen-response"),
            ]),
            4 => responses::sse(vec![
                responses::ev_assistant_message("read-file-after-reopen-message", "re-read"),
                responses::ev_completed("read-file-after-reopen-followup"),
            ]),
            5 => {
                let body = request
                    .body_json::<Value>()
                    .expect("Responses request should contain JSON");
                let artifact_id = artifact_id_from_model_input(&body);
                responses::sse(vec![
                    responses::ev_response_created("read-file-retrieval-response"),
                    responses::ev_function_call(
                        "read-file-retrieval-call",
                        "read_tool_output",
                        &json!({
                            "artifact_id": artifact_id,
                            "mode": "bytes",
                            "offset": 0,
                            "limit": 32 * 1024,
                        })
                        .to_string(),
                    ),
                    responses::ev_completed("read-file-retrieval-response"),
                ])
            }
            6 => responses::sse(vec![
                responses::ev_assistant_message("read-file-retrieved-message", "retrieved"),
                responses::ev_completed("read-file-retrieved-followup"),
            ]),
            7 => {
                let body = request
                    .body_json::<Value>()
                    .expect("Responses request should contain JSON");
                let artifact_id = artifact_id_from_model_input(&body);
                responses::sse(vec![
                    responses::ev_response_created("read-file-fork-retrieval-response"),
                    responses::ev_function_call(
                        "read-file-fork-retrieval-call",
                        "read_tool_output",
                        &json!({
                            "artifact_id": artifact_id,
                            "mode": "bytes",
                            "offset": 0,
                            "limit": 32 * 1024,
                        })
                        .to_string(),
                    ),
                    responses::ev_completed("read-file-fork-retrieval-response"),
                ])
            }
            8 => responses::sse(vec![
                responses::ev_assistant_message("read-file-fork-retrieved-message", "retrieved"),
                responses::ev_completed("read-file-fork-retrieved-followup"),
            ]),
            _ => panic!("unexpected extra Responses API request"),
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

fn function_call_output(requests: &[wiremock::Request], call_id: &str) -> Result<String> {
    requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(wiremock::Request::body_json::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|body| body["input"].as_array().cloned().unwrap_or_default())
        .find_map(|item| {
            (item["type"] == "function_call_output" && item["call_id"].as_str() == Some(call_id))
                .then(|| item["output"].as_str().map(str::to_string))
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("missing function output for {call_id}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_server_read_file_execution_and_resume_keep_public_history_valid() -> Result<()> {
    let responses_server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &responses_server.uri(),
        &BTreeMap::from([(Feature::NativeReadFile, true)]),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let config_path = codex_home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path)?;
    let model_provider = config
        .find("\nmodel_provider =")
        .ok_or_else(|| anyhow::anyhow!("mock config should contain a root model_provider"))?;
    config.insert_str(model_provider, "\ntool_output_token_limit = 100_000");
    std::fs::write(config_path, config)?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let auto_env = app_server.auto_env()?;
    let file_path = auto_env.selection().cwd.join("app-server-read.txt")?;
    let file_contents = format!("app-server read_file marker: {}\n", "x".repeat(72)).repeat(70);
    std::fs::write(
        codex_home.path().join("app-server-read.txt"),
        &file_contents,
    )?;
    auto_env
        .environment()
        .get_filesystem()
        .write_file(
            &file_path,
            file_contents.as_bytes().to_vec(),
            WriteFileOptions::default(),
            /*sandbox*/ None,
        )
        .await?;

    let start_id = app_server
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let start_response: JSONRPCResponse = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let start_value = serde_json::to_value(start_response.result)?;
    let thread_id = start_value["thread"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("thread/start should return a thread id"))?
        .to_string();

    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(ReadFileResponses {
            request_count: AtomicUsize::new(0),
        })
        .mount(&responses_server)
        .await;
    let turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: "read the fixture file".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(READ_TIMEOUT, app_server.read_response(turn_id)).await??;
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = responses_server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("app-server should issue a model request"))?;
    let model_request = requests
        .iter()
        .find(|request| request.url.path().ends_with("/responses"))
        .ok_or_else(|| anyhow::anyhow!("app-server should issue a Responses request"))?
        .body_json::<serde_json::Value>()?;
    assert!(serde_json::to_string(&model_request)?.contains("read_file"));
    let first_output = function_call_output(&requests, "read-file-call-1")?;
    let duplicate_output = function_call_output(&requests, "read-file-call-2")?;
    let first_value = parse_tool_output("first read_file", &first_output)?;
    let duplicate_value = parse_tool_output("duplicate read_file", &duplicate_output)?;
    let (inline_value, artifact_value) = if first_value["type"] == "file_read" {
        (&first_value, &duplicate_value)
    } else {
        (&duplicate_value, &first_value)
    };
    assert_eq!(inline_value["type"], "file_read");
    assert_eq!(artifact_value["type"], "tool_output_artifact");
    let artifact_id = artifact_value["artifact_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("duplicate read_file output should name an artifact"))?
        .to_string();
    let inline_output = if first_value["type"] == "file_read" {
        first_output.clone()
    } else {
        duplicate_output.clone()
    };

    let compact_id = app_server
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let _: ThreadCompactStartResponse =
        timeout(READ_TIMEOUT, app_server.read_response(compact_id)).await??;
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let read_id = app_server
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: true,
        })
        .await?;
    let read_response: ThreadReadResponse =
        timeout(READ_TIMEOUT, app_server.read_response(read_id)).await??;
    assert!(!read_response.thread.turns.is_empty());
    assert!(
        read_response
            .thread
            .turns
            .iter()
            .any(|turn| !turn.items.is_empty())
    );
    let public_history = serde_json::to_string(&read_response.thread)?;
    assert!(read_response.thread.turns.iter().all(|turn| {
        turn.items.iter().all(|item| {
            let serialized = serde_json::to_string(item).expect("thread item should serialize");
            !serialized.contains("tool_output_artifact")
                && !serialized.contains("app-server read_file marker")
        })
    }));
    assert!(!public_history.contains("tool_output_artifact"));

    let fork_id = app_server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let fork_response: ThreadForkResponse =
        timeout(READ_TIMEOUT, app_server.read_response(fork_id)).await??;
    let forked_thread_id = fork_response.thread.id.clone();
    assert_ne!(forked_thread_id, thread_id);
    assert!(!fork_response.thread.turns.is_empty());
    assert!(
        fork_response
            .thread
            .turns
            .iter()
            .any(|turn| !turn.items.is_empty())
    );
    let fork_public = serde_json::to_string(&fork_response.thread)?;
    assert!(!fork_public.contains("tool_output_artifact"));
    assert!(!fork_public.contains("app-server read_file marker"));

    app_server.shutdown_gracefully().await?;
    drop(app_server);

    let mut reopened_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let reopened_file_path = reopened_server
        .auto_env()?
        .selection()
        .cwd
        .join("app-server-read.txt")?;
    reopened_server
        .auto_env()?
        .environment()
        .get_filesystem()
        .write_file(
            &reopened_file_path,
            file_contents.as_bytes().to_vec(),
            WriteFileOptions::default(),
            /*sandbox*/ None,
        )
        .await?;
    let resume_id = reopened_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let resumed_response: ThreadResumeResponse =
        timeout(READ_TIMEOUT, reopened_server.read_response(resume_id)).await??;
    assert!(!resumed_response.thread.turns.is_empty());
    assert!(
        resumed_response
            .thread
            .turns
            .iter()
            .any(|turn| !turn.items.is_empty())
    );
    let resumed_public = serde_json::to_string(&resumed_response.thread)?;
    assert!(!resumed_public.contains("tool_output_artifact"));
    assert!(!resumed_public.contains("app-server read_file marker"));
    let reopened_read_id = reopened_server
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: true,
        })
        .await?;
    let reopened: ThreadReadResponse = timeout(
        READ_TIMEOUT,
        reopened_server.read_response(reopened_read_id),
    )
    .await??;
    assert_eq!(reopened.thread.id, read_response.thread.id);
    assert!(!reopened.thread.turns.is_empty());
    assert!(
        reopened
            .thread
            .turns
            .iter()
            .any(|turn| !turn.items.is_empty())
    );
    let reopened_history = serde_json::to_string(&reopened.thread)?;
    assert!(!reopened_history.contains("tool_output_artifact"));
    assert!(!reopened_history.contains("app-server read_file marker"));

    let fresh_read_turn_id = reopened_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: "read the same file after reopening".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(
        READ_TIMEOUT,
        reopened_server.read_response(fresh_read_turn_id),
    )
    .await??;
    timeout(
        READ_TIMEOUT,
        reopened_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let requests_after_fresh_read = responses_server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("fresh read request should be captured"))?;
    let fresh_read_output =
        function_call_output(&requests_after_fresh_read, "read-file-call-after-reopen")?;
    assert_eq!(
        parse_tool_output("fresh read_file", &fresh_read_output)?["type"],
        "file_read"
    );

    let retrieval_turn_id = reopened_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::Text {
                text: "retrieve the repeated read after restart".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(
        READ_TIMEOUT,
        reopened_server.read_response(retrieval_turn_id),
    )
    .await??;
    timeout(
        READ_TIMEOUT,
        reopened_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let requests = responses_server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("artifact retrieval request should be captured"))?;
    let resumed_request = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .nth(5)
        .ok_or_else(|| anyhow::anyhow!("post-resume Responses request should be captured"))?;
    let resumed_body = resumed_request.body_json::<Value>()?;
    let resumed_outputs = resumed_body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| item["output"].as_str())
        .map(|output| parse_tool_output("resumed function output", output))
        .collect::<Result<Vec<_>, _>>()?;
    let inline_outputs: Vec<_> = resumed_outputs
        .iter()
        .filter(|output| output["type"] == "file_read")
        .collect();
    let artifact_outputs: Vec<_> = resumed_outputs
        .iter()
        .filter(|output| output["type"] == "tool_output_artifact")
        .collect();
    assert_eq!(inline_outputs.len(), 1);
    assert_eq!(artifact_outputs.len(), 1);
    let expected_fresh_inline = parse_tool_output("fresh inline read_file", &fresh_read_output)?;
    assert_eq!(*inline_outputs[0], expected_fresh_inline);
    assert_eq!(artifact_outputs[0]["artifact_id"], artifact_id);
    let artifact_output = serde_json::to_string(artifact_outputs[0])?;
    assert!(!artifact_output.contains(&serde_json::to_string(&file_contents)?));
    assert!(artifact_output.len() < inline_output.len());
    assert_eq!(artifact_id_from_model_input(&resumed_body), artifact_id);
    let retrieved = function_call_output(&requests, "read-file-retrieval-call")?;
    let retrieved_value = parse_tool_output("retrieved artifact window", &retrieved)?;
    assert_eq!(retrieved_value["type"], "tool_output_artifact_window");
    assert_eq!(
        retrieved_value["text"].as_str(),
        Some(inline_output.as_str())
    );

    let delete_parent_id = reopened_server
        .send_thread_delete_request(ThreadDeleteParams { thread_id })
        .await?;
    let _: ThreadDeleteResponse = timeout(
        READ_TIMEOUT,
        reopened_server.read_response(delete_parent_id),
    )
    .await??;

    let child_resume_id = reopened_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: forked_thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(READ_TIMEOUT, reopened_server.read_response(child_resume_id)).await??;
    let child_retrieval_turn_id = reopened_server
        .send_turn_start_request(TurnStartParams {
            thread_id: forked_thread_id.clone(),
            input: vec![UserInput::Text {
                text: "retrieve the forked read after restart".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(
        READ_TIMEOUT,
        reopened_server.read_response(child_retrieval_turn_id),
    )
    .await??;
    timeout(
        READ_TIMEOUT,
        reopened_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let child_requests = responses_server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("fork retrieval request should be captured"))?;
    let child_model_request = child_requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .nth(7)
        .ok_or_else(|| anyhow::anyhow!("fork retrieval Responses request should be captured"))?;
    assert_eq!(
        artifact_id_from_model_input(&child_model_request.body_json::<Value>()?),
        artifact_id
    );
    let child_retrieved = function_call_output(&child_requests, "read-file-fork-retrieval-call")?;
    assert_eq!(
        parse_tool_output("fork retrieved artifact window", &child_retrieved)?["text"],
        inline_output
    );
    Ok(())
}
