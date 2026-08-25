use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_optimizations_repair_nested_first_party_read_file_arguments() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let code_mode_host = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?;
    let mut builder = test_codex()
        .with_code_mode_host_program(code_mode_host)
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
        })
        .with_workspace_setup(|cwd, _fs| async move {
            fs::write(cwd.join("stage3-window.txt"), "stage3 nested marker")?;
            Ok(())
        });
    let test = builder.build_with_auto_env(&server).await?;

    let program = r#"const result = await tools.read_file({path: "stage3-window.txt", offset: 0, max_bytes: "4096", max_lines: 2000});
text(result.window.text.includes("stage3 nested marker") ? "STAGE3_NESTED_REPAIR_SUCCESS" : "marker missing");"#;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("call-1", "exec", program),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let follow_up = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("Read the file through one nested Code Mode call.")
        .await?;

    let request = follow_up.single_request();
    let output = request.custom_tool_call_output("call-1");
    let output = serde_json::to_string(&output)?;
    assert!(output.contains("STAGE3_NESTED_REPAIR_SUCCESS"), "{output}");
    assert!(!output.contains("unsupported custom tool call"));
    assert!(output.contains("Script completed"));
    assert!(!output.contains("Script failed"));

    let input = request.input();
    let call_index = input
        .iter()
        .position(|item| item.get("call_id") == Some(&Value::String("call-1".to_string())))
        .expect("outer call should be retained");
    assert_eq!(input[call_index]["input"], program);
    let output_index = input
        .iter()
        .position(|item| {
            item.get("type").and_then(Value::as_str) == Some("custom_tool_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("call-1")
        })
        .expect("outer output should be retained");
    let repair_disclosures = input
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let text = item
                .get("content")?
                .as_array()?
                .iter()
                .filter_map(|content| content.get("text").and_then(Value::as_str))
                .find(|text| text.contains("tool_argument_repair"))?;
            Some((index, text))
        })
        .collect::<Vec<_>>();
    assert_eq!(repair_disclosures.len(), 1);
    let (disclosure_index, disclosure) = repair_disclosures[0];
    assert!(output_index < disclosure_index, "{input:#?}");
    assert!(disclosure.contains("numeric_string_typed"));
    assert!(disclosure.len() < 1_024);
    Ok(())
}
