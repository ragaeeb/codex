use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feature_gate_controls_model_visible_read_file_schema() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let disabled_mock = mount_sse_once(
        &server,
        assistant_response("disabled-response", "disabled-message"),
    )
    .await;
    let mut disabled_builder = test_codex().with_config(|config| {
        config
            .features
            .disable(Feature::NativeReadFile)
            .expect("test config should disable native read_file");
    });
    let disabled = disabled_builder.build_with_auto_env(&server).await?;
    disabled.submit_turn("inspect the available tools").await?;
    let disabled_body = disabled_mock.single_request().body_json();
    assert!(!serde_json::to_string(&disabled_body)?.contains("read_file"));

    let enabled_mock = mount_sse_once(
        &server,
        assistant_response("enabled-response", "enabled-message"),
    )
    .await;
    let mut enabled_builder = read_file_enabled_builder();
    let enabled = enabled_builder.build_with_auto_env(&server).await?;
    enabled.submit_turn("inspect the available tools").await?;
    let enabled_body = enabled_mock.single_request().body_json();
    assert!(serde_json::to_string(&enabled_body)?.contains("read_file"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_code_mode_returns_a_structured_nested_result() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let code_mode_host = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
        .context("codex-code-mode-host is required for the selected Code Mode test")?;
    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder()
        .with_model("test-gpt-5.1-codex")
        .with_code_mode_host_program(code_mode_host)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("Code Mode should be enabled");
            config
                .features
                .enable(Feature::CodeModeHost)
                .expect("Code Mode host should be enabled");
        })
        .with_workspace_setup(|cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("code-mode.txt"))?,
                format!(
                    "{}\n",
                    "structured-code-mode-result-012345678901234567890123456789"
                )
                .repeat(600)
                .as_bytes()
                .to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let _first_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("code-mode-response"),
            ev_custom_tool_call(
                "code-mode-exec",
                "exec",
                r#"const first = await tools.read_file({ path: "code-mode.txt", max_bytes: 32768 }); const duplicate = await tools.read_file({ path: "code-mode.txt", max_bytes: 32768 }); text(JSON.stringify({ firstType: first.type, duplicateType: duplicate.type, sameText: first.window.text === duplicate.window.text, startsWithMarker: first.window.text.startsWith("structured-code-mode-result-"), firstEndByte: first.window.end_byte, sameNextOffset: first.next_offset === duplicate.next_offset }));"#,
            ),
            ev_completed("code-mode-response"),
        ]),
    )
    .await;
    let followup_mock = mount_sse_once(
        &server,
        assistant_response("code-mode-followup", "code-mode-message"),
    )
    .await;
    test.submit_turn("use Code Mode to read the file").await?;

    let request = followup_mock.single_request();
    let body = request.custom_tool_call_output("code-mode-exec");
    let output = if let Some(output) = body["output"].as_str() {
        output
            .split_once("Output:\n")
            .map_or(output, |(_, output)| output)
            .to_string()
    } else {
        body["output"]
            .as_array()
            .context("Code Mode output should contain text")?
            .iter()
            .filter_map(|item| item["text"].as_str())
            .skip(/*n*/ 1)
            .collect::<String>()
    };
    let value: Value = serde_json::from_str(&output)
        .with_context(|| format!("Code Mode output should be JSON: {output:?}"))?;
    assert_eq!(value["firstType"], "file_read", "{value:#}");
    assert_eq!(value["duplicateType"], "file_read");
    assert_eq!(value["sameText"], true);
    assert_eq!(value["startsWithMarker"], true);
    assert!(
        value["firstEndByte"]
            .as_u64()
            .is_some_and(|end| end <= 8 * 1024)
    );
    assert_eq!(value["sameNextOffset"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_reconstructs_utf8_and_long_line_continuations() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("headé\nmiddle🙂\n{}\n", "x".repeat(2_015));
    let first_end = "headé\n".len() as u64;
    let second_end = "headé\nmiddle🙂\n".len() as u64;
    let long_split = second_end + 2_000;
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("unicode.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;

    let args = |offset| {
        json!({
            "path": "unicode.txt",
            "offset": offset,
            "max_bytes": 4_096,
            "max_lines": 1,
        })
    };
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response("read-response-1", vec![read_call("read-1", args(0))]),
            tool_response(
                "read-response-2",
                vec![read_call("read-2", args(first_end))],
            ),
            tool_response(
                "read-response-3",
                vec![read_call("read-3", args(second_end))],
            ),
            tool_response(
                "read-response-4",
                vec![read_call("read-4", args(long_split))],
            ),
            tool_response(
                "read-response-5",
                vec![read_call("read-inside-code-point", args(5))],
            ),
            assistant_response("read-response-6", "read-message"),
        ],
    )
    .await;

    test.submit_turn("read the file in exact windows").await?;

    let values = ["read-1", "read-2", "read-3", "read-4"]
        .into_iter()
        .map(|call_id| response_json(&mock, call_id))
        .collect::<Result<Vec<_>>>()?;
    let joined = values
        .iter()
        .map(|value| value["window"]["text"].as_str().unwrap_or_default())
        .collect::<String>();
    assert_eq!(joined, source);
    assert_eq!(values[0]["window"]["start_byte"], 0);
    assert_eq!(values[0]["next_offset"], first_end);
    assert_eq!(values[1]["window"]["start_byte"], first_end);
    assert_eq!(values[1]["next_offset"], second_end);
    assert_eq!(values[2]["window"]["start_byte"], second_end);
    assert_eq!(values[2]["window"]["line_continues"], true);
    assert_eq!(values[2]["next_offset"], long_split);
    assert_eq!(values[3]["window"]["start_byte"], long_split);
    assert_eq!(values[3]["eof"], true);
    assert!(output(&mock, "read-inside-code-point")?.contains("inside a UTF-8 code point"));
    for value in values {
        let rendered = serde_json::to_string(&value)?;
        assert!(rendered.len() <= 128 * 1024);
        assert!(value["window"]["line_fragments"].as_u64().unwrap_or(0) <= 2_000);
        for fragment in value["window"]["text"]
            .as_str()
            .unwrap_or_default()
            .split('\n')
        {
            assert!(fragment.chars().count() <= 2_000);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_reconstructs_windows_at_a_small_serialized_byte_cap() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = "quote: \"slash: \\\\ café🙂\nnext\n".repeat(140);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("small-window.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let cap = 1_024usize;
    let mut offset = 0u64;
    let mut reconstructed = Vec::new();
    for index in 0..32 {
        let call_id = format!("small-window-read-{index}");
        let mock = mount_sse_sequence(
            &server,
            vec![
                tool_response(
                    &format!("small-window-response-{index}"),
                    vec![read_call(
                        &call_id,
                        json!({
                            "path": "small-window.txt",
                            "offset": offset,
                            "max_bytes": cap,
                            "max_lines": 2_000,
                        }),
                    )],
                ),
                assistant_response(
                    &format!("small-window-complete-{index}"),
                    &format!("small-window-message-{index}"),
                ),
            ],
        )
        .await;
        test.submit_turn("continue the exact small read window")
            .await?;
        let rendered = output(&mock, &call_id)?;
        assert!(
            rendered.len() <= cap,
            "serialized window exceeded requested cap"
        );
        let value: Value = serde_json::from_str(&rendered)?;
        let start = value["window"]["start_byte"]
            .as_u64()
            .context("window start")?;
        let end = value["window"]["end_byte"].as_u64().context("window end")?;
        assert_eq!(start, offset);
        assert_eq!(
            &source.as_bytes()[usize::try_from(start)?..usize::try_from(end)?],
            value["window"]["text"]
                .as_str()
                .unwrap_or_default()
                .as_bytes()
        );
        reconstructed.extend_from_slice(
            value["window"]["text"]
                .as_str()
                .unwrap_or_default()
                .as_bytes(),
        );
        if value["eof"].as_bool() == Some(true) {
            assert_eq!(value["next_offset"], Value::Null);
            break;
        }
        offset = value["next_offset"].as_u64().context("next offset")?;
        assert_eq!(offset, end);
    }
    assert_eq!(reconstructed, source.as_bytes());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_handles_empty_eof_and_past_eof_windows() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("empty.txt"))?,
            Vec::new(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        fs.write_file(
            &executor_path_uri(cwd.join("small.txt"))?,
            b"small\n".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let args = |path, offset| json!({"path": path, "offset": offset, "max_bytes": 2_048});
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "empty-response",
                vec![read_call("empty", args("empty.txt", 0))],
            ),
            tool_response(
                "empty-past-response",
                vec![read_call("empty-past", args("empty.txt", 1))],
            ),
            tool_response(
                "small-eof-response",
                vec![read_call("small-eof", args("small.txt", 6))],
            ),
            tool_response(
                "small-past-response",
                vec![read_call("small-past", args("small.txt", 7))],
            ),
            assistant_response("empty-complete", "empty-message"),
        ],
    )
    .await;

    test.submit_turn("check empty and eof reads").await?;

    for call_id in ["empty", "small-eof"] {
        let value = response_json(&mock, call_id)?;
        assert_eq!(value["window"]["text"], "");
        assert_eq!(value["window"]["line_fragments"], 0);
        assert_eq!(value["next_offset"], Value::Null);
        assert_eq!(value["eof"], true);
    }
    assert!(output(&mock, "empty-past")?.contains("beyond the end"));
    assert!(output(&mock, "small-past")?.contains("beyond the end"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_enforces_serialized_byte_and_line_caps() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("{}\n", "x".repeat(100)).repeat(2_000);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder()
        .with_config(|config| config.tool_output_token_limit = Some(100_000))
        .with_workspace_setup(move |cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("large.txt"))?,
                source_for_setup.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "large-response",
                vec![read_call(
                    "large-read",
                    json!({
                        "path": "large.txt",
                        "max_bytes": 128 * 1024,
                        "max_lines": 2_000,
                    }),
                )],
            ),
            assistant_response("large-complete", "large-message"),
        ],
    )
    .await;
    test.submit_turn("read a large bounded window").await?;

    let rendered = output(&mock, "large-read")?;
    assert!(rendered.len() <= 128 * 1024);
    let outbound_model_output = mock
        .function_call_output_text("large-read")
        .context("large read output should be sent back to the model")?;
    assert!(
        outbound_model_output.len() <= 8 * 1024,
        "model-facing read_file output exceeded the conservative complete-item ceiling: {}",
        outbound_model_output.len()
    );
    assert!(
        approx_token_count(&outbound_model_output) <= 10_000,
        "model-facing read_file output exceeded the independent 10K-token ceiling"
    );
    let value: Value = serde_json::from_str(&rendered)?;
    assert!(value["window"]["line_fragments"].as_u64().unwrap_or(0) <= 2_000);
    assert_eq!(value["eof"], false);
    for fragment in value["window"]["text"]
        .as_str()
        .unwrap_or_default()
        .split('\n')
    {
        assert!(fragment.chars().count() <= 2_000);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_omitted_limits_use_bounded_defaults() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("{}\n", "default-limit-line-012345678901234567890123456789").repeat(500);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("defaults.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "defaults-response",
                vec![read_call("defaults-read", json!({"path": "defaults.txt"}))],
            ),
            assistant_response("defaults-complete", "defaults-message"),
        ],
    )
    .await;
    test.submit_turn("read with omitted bounds").await?;
    let rendered = output(&mock, "defaults-read")?;
    let cap = 8 * 1024usize;
    assert!(rendered.len() <= cap);
    let value: Value = serde_json::from_str(&rendered)?;
    assert!(value["window"]["line_fragments"].as_u64().unwrap_or(0) <= 200);
    assert_eq!(value["window"]["start_byte"], 0);
    assert_eq!(
        value["window"]["text"]
            .as_str()
            .unwrap_or_default()
            .as_bytes(),
        &source.as_bytes()[..usize::try_from(value["window"]["end_byte"].as_u64().unwrap_or(0))?]
    );
    assert!(value["next_offset"].is_number());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_defaults_reach_the_line_cap_exactly() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = "default-line\n".repeat(260);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("default-lines.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "default-lines-response",
                vec![read_call(
                    "default-lines-read",
                    json!({"path": "default-lines.txt"}),
                )],
            ),
            assistant_response("default-lines-complete", "default-lines-message"),
        ],
    )
    .await;
    test.submit_turn("read exactly the default line window")
        .await?;
    let rendered = output(&mock, "default-lines-read")?;
    let value: Value = serde_json::from_str(&rendered)?;
    assert_eq!(value["window"]["line_fragments"], 200);
    assert_eq!(value["window"]["start_byte"], 0);
    assert_eq!(value["next_offset"], ("default-line\n".len() * 200) as u64);
    assert_eq!(value["eof"], false);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_exact_serialized_default_cap_stops_at_a_recoverable_byte() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("quoted: \\\"slash: \\\\\\\" café🙂 {}\n", "x".repeat(170)).repeat(400);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder()
        .with_model_info_override("gpt-5.5", |model| {
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::bytes(128 * 1024);
        })
        .with_workspace_setup(move |cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("serialized-cap.txt"))?,
                source_for_setup.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "serialized-cap-response",
                vec![read_call(
                    "serialized-cap-read",
                    json!({"path": "serialized-cap.txt"}),
                )],
            ),
            assistant_response("serialized-cap-complete", "serialized-cap-message"),
        ],
    )
    .await;
    test.submit_turn("read at the serialized default cap")
        .await?;
    let rendered = output(&mock, "serialized-cap-read")?;
    let value: Value = serde_json::from_str(&rendered)?;
    let start = value["window"]["start_byte"]
        .as_u64()
        .context("start byte")?;
    let end = value["window"]["end_byte"].as_u64().context("end byte")?;
    let cap = 8 * 1024usize;
    assert!(rendered.len() <= cap);
    assert!(
        rendered.len() + 8 >= cap,
        "fixture did not exercise the cap"
    );
    assert_eq!(start, 0);
    assert_eq!(
        &source.as_bytes()[usize::try_from(start)?..usize::try_from(end)?],
        value["window"]["text"]
            .as_str()
            .context("window text")?
            .as_bytes()
    );
    assert_eq!(value["next_offset"], end);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_honors_a_lower_configured_output_limit_with_valid_continuation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("configured-limit: \"quoted\" café🙂 {}\n", "x".repeat(120)).repeat(180);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder()
        .with_model_info_override("gpt-5.5", |model| {
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::bytes(128 * 1024);
        })
        .with_config(|config| config.tool_output_token_limit = Some(2_000))
        .with_workspace_setup(move |cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("configured-limit.txt"))?,
                source_for_setup.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "configured-limit-response",
                vec![read_call(
                    "configured-limit-read",
                    json!({"path": "configured-limit.txt", "max_bytes": 32 * 1024}),
                )],
            ),
            assistant_response("configured-limit-complete", "configured-limit-message"),
        ],
    )
    .await;
    test.submit_turn("read with a configured output policy")
        .await?;

    let rendered = output(&mock, "configured-limit-read")?;
    let value: Value = serde_json::from_str(&rendered)?;
    assert!(rendered.len() <= 8_000);
    let start = value["window"]["start_byte"]
        .as_u64()
        .context("start byte")?;
    let end = value["window"]["end_byte"].as_u64().context("end byte")?;
    assert_eq!(start, 0);
    assert_eq!(value["next_offset"], end);
    assert_eq!(
        &source.as_bytes()[usize::try_from(start)?..usize::try_from(end)?],
        value["window"]["text"]
            .as_str()
            .context("window text")?
            .as_bytes()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_rejects_values_above_hard_bounds() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder();
    let test = builder.build_with_auto_env(&server).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "invalid-bounds-response",
                vec![
                    read_call(
                        "invalid-bytes",
                        json!({"path": "unused", "max_bytes": 128 * 1024 + 1}),
                    ),
                    read_call(
                        "invalid-lines",
                        json!({"path": "unused", "max_lines": 2_001}),
                    ),
                ],
            ),
            assistant_response("invalid-bounds-complete", "invalid-bounds-message"),
        ],
    )
    .await;
    test.submit_turn("reject unbounded read requests").await?;
    assert!(output(&mock, "invalid-bytes")?.contains("max_bytes"));
    assert!(output(&mock, "invalid-lines")?.contains("max_lines"));
    Ok(())
}
