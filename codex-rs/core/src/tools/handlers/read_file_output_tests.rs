use super::*;
use codex_tools::ToolOutputProvenance;

fn payload() -> ToolPayload {
    ToolPayload::Function {
        arguments: "{}".to_string(),
    }
}

#[test]
fn keeps_textual_responses_and_structured_code_mode_values_in_sync() {
    let inline = ReadFileToolOutput::new(
        r#"{"type":"file_read","path":"/private/秘密.txt","window":{"text":"café","start_byte":0,"end_byte":5,"line_fragments":1}}"#.to_string(),
        /*managed_artifact*/ false,
        "inline_v1",
    )
    .expect("inline result should parse");
    assert_eq!(
        inline.code_mode_result(&payload())["window"]["text"],
        "café"
    );
    assert_eq!(inline.provenance(), ToolOutputProvenance::Untrusted);
    assert!(matches!(
        inline.to_response_item("call", &payload()),
        ResponseInputItem::FunctionCallOutput { .. }
    ));
    let log = inline.log_output();
    assert!(log.contains("read_file"));
    assert!(!log.contains("café"));
    assert!(!log.contains("秘密"));

    let artifact = ReadFileToolOutput::new(
        r#"{"type":"tool_output_artifact","artifact_id":"out_id","retrieval":"Use read_tool_output."}"#.to_string(),
        /*managed_artifact*/ true,
        "exact_window_artifact_v1",
    )
    .expect("artifact result should parse");
    assert_eq!(
        artifact.code_mode_result(&payload())["type"],
        "tool_output_artifact"
    );
    assert_eq!(
        artifact.provenance(),
        ToolOutputProvenance::ManagedArtifactReference
    );
    assert!(!artifact.log_output().contains("out_id"));
}
