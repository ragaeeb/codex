use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::OutputArtifactStore;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_output_policy_returns_a_bounded_advancing_retrieval_window() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder();
    builder = builder.with_config(|config| config.tool_output_token_limit = Some(50));
    let test = builder.build_with_auto_env(&server).await?;
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(test.codex_home_path())?
            .join("tool_outputs")
            .join(test.session_configured.thread_id.to_string()),
    );
    let artifact = store.store_text(&"secret\n".repeat(1_000)).await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "low-policy-response",
                vec![ev_function_call(
                    "low-policy-read",
                    "read_tool_output",
                    &json!({
                        "artifact_id": artifact.id.as_str(),
                        "mode": "bytes",
                        "offset": 0,
                        "limit": 512,
                    })
                    .to_string(),
                )],
            ),
            assistant_response("low-policy-complete", "low-policy-message"),
        ],
    )
    .await;
    test.submit_turn("retrieve under a deliberately tiny output policy")
        .await?;

    let rendered = output(&mock, "low-policy-read")?;
    anyhow::ensure!(
        rendered.len() <= 200,
        "bounded retrieval exceeded the active policy"
    );
    let value: Value = serde_json::from_str(&rendered)?;
    anyhow::ensure!(
        value.is_object(),
        "retrieval error must remain structured JSON"
    );
    assert_eq!(value["type"], "tool_output_artifact_window");
    let text = value["text"].as_str().context("bounded retrieval text")?;
    anyhow::ensure!(!text.is_empty(), "low-policy retrieval must advance");
    let next = value["next_offset"]
        .as_u64()
        .context("low-policy retrieval continuation")?;
    anyhow::ensure!(next > 0, "low-policy retrieval must advance");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_read_file_calls_are_safe() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        let contents = format!("{}\n", "parallel-line").repeat(300);
        fs.write_file(
            &executor_path_uri(cwd.join("parallel.txt"))?,
            contents.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder
        .with_model_info_override("gpt-5.5", |model| {
            model.truncation_policy =
                codex_protocol::openai_models::TruncationPolicyConfig::bytes(128 * 1024);
        })
        .build_with_auto_env(&server)
        .await?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "parallel-response-1",
                vec![
                    read_call(
                        "parallel-one",
                        json!({
                            "path": "parallel.txt",
                            "max_bytes": 32 * 1024,
                            "max_lines": 2_000,
                        }),
                    ),
                    read_call(
                        "parallel-two",
                        json!({
                            "path": "parallel.txt",
                            "max_bytes": 32 * 1024,
                            "max_lines": 2_000,
                        }),
                    ),
                ],
            ),
            assistant_response("parallel-response-2", "parallel-message"),
        ],
    )
    .await;

    test.submit_turn("read both files").await?;
    let first_output = output(&mock, "parallel-one")?;
    let second_output = output(&mock, "parallel-two")?;
    let first: Value = serde_json::from_str(&first_output)?;
    let second: Value = serde_json::from_str(&second_output)?;
    let inline_output = if first["type"] == "file_read" {
        first_output
    } else {
        second_output
    };
    assert_eq!(
        [first["type"].as_str(), second["type"].as_str()]
            .into_iter()
            .filter(|kind| *kind == Some("file_read"))
            .count(),
        1
    );
    assert!(
        [first["type"].as_str(), second["type"].as_str()]
            .into_iter()
            .filter(|kind| *kind == Some("tool_output_artifact"))
            .count()
            >= 1
    );
    let artifact_id = [first, second]
        .into_iter()
        .find_map(|value| value["artifact_id"].as_str().map(str::to_string));
    let artifact_id = artifact_id.context("an overlapping duplicate must create an artifact")?;
    let retrieval_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "parallel-retrieve-response",
                vec![ev_function_call(
                    "parallel-retrieve",
                    "read_tool_output",
                    &json!({
                        "artifact_id": artifact_id,
                        "mode": "bytes",
                        "offset": 0,
                        "limit": 32 * 1024,
                    })
                    .to_string(),
                )],
            ),
            assistant_response("parallel-retrieve-complete", "parallel-retrieve-message"),
        ],
    )
    .await;
    test.submit_turn("retrieve the parallel read").await?;
    let retrieved: Value = serde_json::from_str(&output(&retrieval_mock, "parallel-retrieve")?)?;
    assert_eq!(retrieved["type"], "tool_output_artifact_window");
    assert_eq!(retrieved["text"].as_str(), Some(inline_output.as_str()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_repeated_read_file_output_stays_inline_after_mutation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("changing.txt"))?,
            b"artifact-middle-marker-A\n".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let args = json!({"path": "changing.txt", "max_bytes": 4_096});
    let first_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "first-response",
                vec![read_call("first-read", args.clone())],
            ),
            assistant_response("first-complete", "first-message"),
        ],
    )
    .await;
    test.submit_turn("read the changing file").await?;
    let first_output = output(&first_mock, "first-read")?;
    let first_value: Value = serde_json::from_str(&first_output)?;
    assert_eq!(first_value["type"], "file_read");
    assert!(
        first_value["window"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("artifact-middle-marker-A"))
    );
    test.fs()
        .write_file(
            &test.workspace_path_uri("changing.txt")?,
            b"artifact-middle-marker-B\n".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    let changed_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "changed-response",
                vec![read_call("changed-read", args.clone())],
            ),
            assistant_response("changed-complete", "changed-message"),
        ],
    )
    .await;
    test.submit_turn("read it again after mutation").await?;
    let changed_output = output(&changed_mock, "changed-read")?;
    assert_eq!(
        serde_json::from_str::<Value>(&changed_output)?["type"],
        "file_read"
    );
    assert!(changed_output.contains("artifact-middle-marker-B"));
    assert!(!changed_output.contains("tool_output_artifact"));
    let duplicate_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "duplicate-response",
                vec![read_call("duplicate-read", args)],
            ),
            assistant_response("duplicate-complete", "duplicate-message"),
        ],
    )
    .await;
    test.submit_turn("read the same changed window again")
        .await?;

    let duplicate_output = output(&duplicate_mock, "duplicate-read")?;
    assert_eq!(
        serde_json::from_str::<Value>(&duplicate_output)?["type"],
        "file_read"
    );
    assert!(!duplicate_output.contains("tool_output_artifact"));
    assert!(!changed_output.contains("tool_output_artifact"));
    Ok(())
}

#[tokio::test]
async fn repeated_small_read_stays_inline_when_an_artifact_cannot_save_bytes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = read_file_enabled_builder().with_workspace_setup(|cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("small-duplicate.txt"))?,
            b"small duplicate payload\n".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let args = json!({"path": "small-duplicate.txt", "max_bytes": 4_096});
    let first_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "small-duplicate-response-1",
                vec![read_call("small-duplicate-1", args.clone())],
            ),
            assistant_response("small-duplicate-complete-1", "small-duplicate-message-1"),
        ],
    )
    .await;
    test.submit_turn("read a small file").await?;
    let first = output(&first_mock, "small-duplicate-1")?;
    let second_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "small-duplicate-response-2",
                vec![read_call("small-duplicate-2", args)],
            ),
            assistant_response("small-duplicate-complete-2", "small-duplicate-message-2"),
        ],
    )
    .await;
    test.submit_turn("read the same small file").await?;
    let second = output(&second_mock, "small-duplicate-2")?;
    assert_eq!(serde_json::from_str::<Value>(&first)?["type"], "file_read");
    assert_eq!(serde_json::from_str::<Value>(&second)?["type"], "file_read");
    assert_eq!(first, second);
    Ok(())
}

#[tokio::test]
async fn repeated_medium_read_uses_an_economical_artifact() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!(
        "{}\n",
        "medium-duplicate-line-0123456789012345678901234567890123456789"
    )
    .repeat(180);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("medium-duplicate.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let first_args = json!({
        "path": "medium-duplicate.txt",
        "max_bytes": 32 * 1024,
        "max_lines": 2_000,
    });
    let second_args = json!({
        "path": "medium-duplicate.txt",
        "max_bytes": 128 * 1024,
        "max_lines": 1_000,
    });
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "medium-duplicate-response-1",
                vec![read_call("medium-duplicate-1", first_args)],
            ),
            assistant_response("medium-duplicate-complete-1", "medium-duplicate-message-1"),
            tool_response(
                "medium-duplicate-response-2",
                vec![read_call("medium-duplicate-2", second_args)],
            ),
            assistant_response("medium-duplicate-complete-2", "medium-duplicate-message-2"),
        ],
    )
    .await;
    test.submit_turn("read a medium file").await?;
    test.submit_turn("read the same medium file").await?;

    let first = output(&mock, "medium-duplicate-1")?;
    let second = output(&mock, "medium-duplicate-2")?;
    assert!(
        first.len() > 7 * 1024,
        "fixture should exercise the conservative large-window case"
    );
    assert_eq!(serde_json::from_str::<Value>(&first)?["type"], "file_read");
    assert_eq!(
        serde_json::from_str::<Value>(&second)?["type"],
        "tool_output_artifact"
    );
    assert!(
        second.len() < first.len(),
        "artifact envelope must save bytes"
    );
    Ok(())
}

#[tokio::test]
async fn repeated_large_read_uses_a_recoverable_artifact() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!(
        "{}\n",
        "unsupported-spill-line-012345678901234567890123456789"
    )
    .repeat(220);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder().with_workspace_setup(move |cwd, fs| async move {
        fs.write_file(
            &executor_path_uri(cwd.join("unsupported-spill.txt"))?,
            source_for_setup.as_bytes().to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
        Ok(())
    });
    let test = builder.build_with_auto_env(&server).await?;
    let args = json!({"path": "unsupported-spill.txt", "max_bytes": 32 * 1024});
    let mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "unsupported-spill-response-1",
                vec![read_call("unsupported-spill-1", args.clone())],
            ),
            assistant_response(
                "unsupported-spill-complete-1",
                "unsupported-spill-message-1",
            ),
            tool_response(
                "unsupported-spill-response-2",
                vec![read_call("unsupported-spill-2", args)],
            ),
            assistant_response(
                "unsupported-spill-complete-2",
                "unsupported-spill-message-2",
            ),
        ],
    )
    .await;
    test.submit_turn("read with artifact spilling").await?;
    test.submit_turn("read the same file with artifact spilling")
        .await?;

    let first = output(&mock, "unsupported-spill-1")?;
    let second = output(&mock, "unsupported-spill-2")?;
    assert_eq!(serde_json::from_str::<Value>(&first)?["type"], "file_read");
    assert_eq!(
        serde_json::from_str::<Value>(&second)?["type"],
        "tool_output_artifact"
    );
    assert!(first.contains("unsupported-spill-line-012345678901234567890123456789"));
    assert!(second.len() < first.len());
    assert!(test.codex_home_path().join("tool_outputs").is_dir());
    Ok(())
}

#[tokio::test]
async fn repeated_large_read_store_failure_falls_back_to_inline_json() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("{}\n", "store-failure-line-012345678901234567890123456789").repeat(220);
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder()
        .with_model("gpt-5.2")
        .with_workspace_setup(move |cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("store-failure.txt"))?,
                source_for_setup.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let tool_outputs = test.codex_home_path().join("tool_outputs");
    let args = json!({"path": "store-failure.txt", "max_bytes": 32 * 1024});
    let first_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "store-failure-response-1",
                vec![read_call("store-failure-1", args.clone())],
            ),
            assistant_response("store-failure-complete-1", "store-failure-message-1"),
        ],
    )
    .await;
    test.submit_turn("read the artifact-backed file once")
        .await?;
    let first = output(&first_mock, "store-failure-1")?;
    assert_eq!(serde_json::from_str::<Value>(&first)?["type"], "file_read");

    if tool_outputs.is_dir() {
        std::fs::remove_dir_all(&tool_outputs)?;
    }
    std::fs::write(&tool_outputs, b"artifact backend failure")?;

    let second_mock = mount_sse_sequence(
        &server,
        vec![
            tool_response(
                "store-failure-response-2",
                vec![read_call("store-failure-2", args)],
            ),
            assistant_response("store-failure-complete-2", "store-failure-message-2"),
        ],
    )
    .await;
    test.submit_turn("read it again after the artifact failure")
        .await?;
    let rendered = output(&second_mock, "store-failure-2")?;
    let value: Value = serde_json::from_str(&rendered)?;
    assert_eq!(value["type"], "file_read");
    assert!(
        rendered.len() > 7 * 1024,
        "store failure must reach the spill branch"
    );
    assert!(rendered.len() <= 8 * 1024);
    assert!(value["window"]["text"].as_str().is_some_and(|text| {
        text.contains("store-failure-line-012345678901234567890123456789")
    }));
    assert!(!rendered.contains("tool_output_artifact"));
    assert!(
        tool_outputs.is_file(),
        "the forced store failure must be observable"
    );
    Ok(())
}

#[tokio::test]
async fn read_file_synthetic_exact_window_rollout_output_body_savings_exceeds_eighty_percent()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let source = format!("{}\n", "source-line-012345678901234567890123456789").repeat(2_300);
    anyhow::ensure!(
        source.len() > 96 * 1024,
        "evaluation source should be substantial"
    );
    let source_for_setup = source.clone();
    let mut builder = read_file_enabled_builder()
        .with_config(|config| config.tool_output_token_limit = Some(100_000))
        .with_workspace_setup(move |cwd, fs| async move {
            fs.write_file(
                &executor_path_uri(cwd.join("source.txt"))?,
                source_for_setup.as_bytes().to_vec(),
                Default::default(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;
    let args = json!({"path": "source.txt", "max_bytes": 128 * 1024, "max_lines": 2_000});
    let mut responses = Vec::new();
    for index in 0..10 {
        responses.push(tool_response(
            &format!("savings-response-{index}"),
            vec![read_call(&format!("savings-read-{index}"), args.clone())],
        ));
        responses.push(assistant_response(
            &format!("savings-complete-{index}"),
            &format!("savings-message-{index}"),
        ));
    }
    let mock = mount_sse_sequence(&server, responses).await;
    for index in 0..10 {
        test.submit_turn(&format!("read the source repeatedly {index}"))
            .await?;
    }

    let outputs = (0..10)
        .map(|index| output(&mock, &format!("savings-read-{index}")))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        serde_json::from_str::<Value>(&outputs[0])?["type"],
        "file_read"
    );
    let artifact_outputs = outputs
        .iter()
        .filter(|text| text.contains("tool_output_artifact"))
        .count();
    assert!(
        artifact_outputs >= 8,
        "expected repeated reads to use artifacts"
    );
    let inline_output = outputs
        .iter()
        .find(|output| output.contains(r#""type":"file_read""#))
        .context("the first read should remain inline")?
        .clone();
    // This is deliberately a synthetic exact-window comparison: it contrasts the observed
    // deduplicated thread with the hypothetical cost of ten inline copies of the first result.
    // It is not a claim about whole-workflow savings or a corpus-level replay.
    let hypothetical_inline_model_bytes = outputs[0].len() * outputs.len();
    let actual_model_bytes: usize = outputs.iter().map(String::len).sum();
    let model_savings = 1.0 - (actual_model_bytes as f64 / hypothetical_inline_model_bytes as f64);
    assert!(
        model_savings > 0.80,
        "hypothetical_inline={hypothetical_inline_model_bytes} actual={actual_model_bytes} savings={model_savings:.4}"
    );

    let request_bodies = mock
        .requests()
        .into_iter()
        .map(|request| request.body_json())
        .collect::<Vec<_>>();
    let actual_workflow_model_bytes = request_bodies
        .iter()
        .map(|body| {
            serde_json::to_vec(body)
                .expect("Responses request should serialize")
                .len()
        })
        .sum::<usize>();
    let control_workflow_model_bytes = request_bodies
        .iter()
        .map(|body| {
            let mut control = body.clone();
            if let Some(input) = control["input"].as_array_mut() {
                for item in input {
                    let is_artifact_output = item["type"] == "function_call_output"
                        && item["output"]
                            .as_str()
                            .and_then(|output| serde_json::from_str::<Value>(output).ok())
                            .is_some_and(|output| output["type"] == "tool_output_artifact");
                    if is_artifact_output {
                        item["output"] = inline_output.clone().into();
                    }
                }
            }
            serde_json::to_vec(&control)
                .expect("control Responses request should serialize")
                .len()
        })
        .sum::<usize>();
    let workflow_model_savings =
        1.0 - (actual_workflow_model_bytes as f64 / control_workflow_model_bytes as f64);
    println!(
        "synthetic repeated-thread workflow evidence: control_input_bytes={control_workflow_model_bytes} actual_input_bytes={actual_workflow_model_bytes} model_input_savings={workflow_model_savings:.4}"
    );
    assert!(
        actual_workflow_model_bytes < control_workflow_model_bytes,
        "deduplicated workflow should send fewer model-input bytes"
    );

    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    let rollout = std::fs::read_to_string(rollout_path)?;
    let hypothetical_inline_rollout_output_body_bytes = outputs[0].len() * outputs.len();
    let actual_rollout_output_body_bytes = persisted_read_output_bytes(&rollout, "savings-read-")?;
    let rollout_output_body_savings = 1.0
        - (actual_rollout_output_body_bytes as f64
            / hypothetical_inline_rollout_output_body_bytes as f64);
    println!(
        "synthetic exact-window evidence: hypothetical_inline_model={hypothetical_inline_model_bytes} actual_model={actual_model_bytes} hypothetical_inline_rollout_output_body={hypothetical_inline_rollout_output_body_bytes} actual_rollout_output_body={actual_rollout_output_body_bytes} model_savings={model_savings:.4} rollout_output_body_savings={rollout_output_body_savings:.4}"
    );
    assert!(
        rollout_output_body_savings > 0.80,
        "hypothetical_inline_rollout_output_body={hypothetical_inline_rollout_output_body_bytes} actual={actual_rollout_output_body_bytes} savings={rollout_output_body_savings:.4}"
    );

    let artifact_id = outputs
        .iter()
        .find_map(|output| {
            serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|value| value["artifact_id"].as_str().map(str::to_string))
        })
        .context("a repeated read should produce an artifact")?;
    let mut recovered = String::new();
    let mut offset = 0_u64;
    for index in 0..16 {
        let call_id = format!("savings-retrieve-{index}");
        let retrieval_mock = mount_sse_sequence(
            &server,
            vec![
                tool_response(
                    &format!("savings-retrieve-response-{index}"),
                    vec![ev_function_call(
                        &call_id,
                        "read_tool_output",
                        &json!({
                            "artifact_id": artifact_id,
                            "mode": "bytes",
                            "offset": offset,
                            "limit": 128 * 1024,
                        })
                        .to_string(),
                    )],
                ),
                assistant_response(
                    &format!("savings-retrieve-complete-{index}"),
                    &format!("savings-retrieve-message-{index}"),
                ),
            ],
        )
        .await;
        test.submit_turn("retrieve the repeated source read")
            .await?;
        let retrieved: Value = serde_json::from_str(&output(&retrieval_mock, &call_id)?)?;
        assert_eq!(retrieved["type"], "tool_output_artifact_window");
        recovered.push_str(retrieved["text"].as_str().context("retrieved text")?);
        let Some(next_offset) = retrieved["next_offset"].as_u64() else {
            break;
        };
        anyhow::ensure!(next_offset > offset, "artifact retrieval must advance");
        offset = next_offset;
        anyhow::ensure!(index < 15, "artifact retrieval did not reach EOF");
    }
    assert_eq!(recovered, inline_output);
    Ok(())
}
