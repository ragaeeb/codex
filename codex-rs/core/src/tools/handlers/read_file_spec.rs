use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const TOOL_NAME: &str = "read_file";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadFileToolOptions {
    pub(crate) include_environment_id: bool,
}

pub(crate) fn create_read_file_tool(options: ReadFileToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "path".to_string(),
            JsonSchema::string(Some(
                "Path relative to the selected environment's working directory.".to_string(),
            )),
        ),
        (
            "offset".to_string(),
            JsonSchema::integer(Some(
                "Zero-based byte offset from a previous read_file next_offset; defaults to 0."
                    .to_string(),
            )),
        ),
        (
            "max_bytes".to_string(),
            JsonSchema::integer(Some(
                "Maximum serialized response size in bytes; defaults to 32768 and cannot exceed 131072. The model-facing result is conservatively capped at 8192 bytes including its metadata."
                    .to_string(),
            )),
        ),
        (
            "max_lines".to_string(),
            JsonSchema::integer(Some(
                "Maximum returned line fragments; defaults to 200 and cannot exceed 2000."
                    .to_string(),
            )),
        ),
    ]);
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>. Omit to use the primary environment."
                    .to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Read a bounded UTF-8 text window from a regular file. Use next_offset for exact byte-based continuation; read_file never returns lossy text or silently skips omitted bytes.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["path".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(file_read_output_schema()),
    })
}

fn file_read_output_schema() -> Value {
    json!({
        "oneOf": [file_read_object_schema(), tool_output_artifact_schema()]
    })
}

fn file_read_object_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "type": {"const": "file_read"},
            "version": {"type": "integer"},
            "path": {"type": "string"},
            "fingerprint": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "size_bytes": {"type": "integer"},
                    "modified_at_ms": {"type": "integer"},
                    "window_digest": {"type": "string"}
                },
                "required": ["size_bytes", "modified_at_ms", "window_digest"]
            },
            "window": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "start_byte": {"type": "integer"},
                    "end_byte": {"type": "integer"},
                    "text": {"type": "string"},
                    "line_fragments": {"type": "integer"},
                    "line_continues": {"type": "boolean"}
                },
                "required": ["start_byte", "end_byte", "text", "line_fragments", "line_continues"]
            },
            "next_offset": {"type": ["integer", "null"]},
            "eof": {"type": "boolean"},
            "continuation": {"type": ["string", "null"]}
        },
        "required": ["type", "version", "path", "fingerprint", "window", "next_offset", "eof", "continuation"]
    })
}

fn tool_output_artifact_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "type": {"const": "tool_output_artifact"},
            "version": {"const": 1},
            "artifact_id": {"type": "string"},
            "content_type": {"type": "string"},
            "original_bytes": {"type": "integer"},
            "original_lines": {"type": "integer"},
            "approximate_tokens": {"type": "integer"},
            "digest": {"type": "string"},
            "preview": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "head": {"type": "string"},
                    "tail": {"type": "string"}
                },
                "required": ["head", "tail"]
            },
            "retrieval": {"type": "string"}
        },
        "required": ["type", "artifact_id"]
    })
}
