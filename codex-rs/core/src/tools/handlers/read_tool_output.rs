use crate::function_tool::FunctionCallError;
use crate::tool_output::MAX_MANAGED_ARTIFACT_MODEL_BYTES;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_history::STORE_BACKED_TOOL_OUTPUT_MAX_BYTES;
use codex_otel::ToolResultLogPolicy;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::OutputArtifactId;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[path = "read_tool_output_window.rs"]
mod read_tool_output_window;

use read_tool_output_window::DEFAULT_BYTES;
use read_tool_output_window::MAX_MATCHES;
use read_tool_output_window::bounded_result;
use read_tool_output_window::read_bytes_value;
use read_tool_output_window::read_lines;
use read_tool_output_window::search;

const TOOL_NAME: &str = "read_tool_output";
const RETRIEVAL_METADATA_RESERVE_BYTES: usize = 128;
/// This matches the smallest producer-side artifact envelope floor. Smaller active policies
/// receive a valid structured error instead of a non-progressing recovery window.
const MIN_RETRIEVAL_OUTPUT_BYTES: usize = crate::tool_output::MIN_ARTIFACT_ENVELOPE_BYTES;
/// Line-mode responses need a little more metadata than byte windows, even after their optional
/// display fields are removed. Rejecting a smaller policy is safer than returning a cursor that
/// cannot carry the next line.
const MIN_LINE_RETRIEVAL_OUTPUT_BYTES: usize = 256;

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Bytes,
    Lines,
    Search,
}

#[derive(Deserialize)]
struct Args {
    artifact_id: String,
    #[serde(default)]
    mode: Mode,
    offset: Option<u64>,
    /// Byte cursor returned with a bounded line scan continuation.
    byte_offset: Option<u64>,
    /// True when byte_offset is inside the line preceding the requested target line.
    scan_continuation: Option<bool>,
    /// True when byte_offset continues a line that was fitted across two responses.
    line_continuation: Option<bool>,
    /// Remaining lines to skip when a line scan resumes after its bounded byte ceiling.
    scan_lines_remaining: Option<usize>,
    limit: Option<usize>,
    query: Option<String>,
}

pub struct ReadToolOutputHandler;

struct ReadToolOutputResult {
    response: FunctionToolOutput,
    mode: &'static str,
    serialized_bytes: usize,
}

impl ReadToolOutputResult {
    fn new(text: String, mode: &'static str) -> Self {
        let success = !serde_json::from_str::<Value>(&text).is_ok_and(|value| {
            value.get("type").and_then(Value::as_str) == Some("tool_output_error")
        });
        Self::with_success(text, mode, success)
    }

    fn error(text: String, mode: &'static str) -> Self {
        Self::with_success(text, mode, /*success*/ false)
    }

    fn with_success(text: String, mode: &'static str, success: bool) -> Self {
        let serialized_bytes = text.len();
        let managed = is_recoverable_artifact_control(&text);
        Self {
            response: if managed {
                FunctionToolOutput::from_managed_artifact_text(text)
            } else {
                FunctionToolOutput::from_text(text, Some(success))
            },
            mode,
            serialized_bytes,
        }
    }
}

fn is_recoverable_artifact_control(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "tool_output_artifact_window" | "tool_output_artifact_search" | "tool_output_artifact"
        )
    ) && value
        .get("artifact_id")
        .and_then(Value::as_str)
        .and_then(|id| OutputArtifactId::parse(id).ok())
        .is_some()
}

impl codex_tools::ToolOutput for ReadToolOutputResult {
    fn log_output(&self) -> String {
        json!({
            "tool_family": TOOL_NAME,
            "mode": self.mode,
            "serialized_bytes": self.serialized_bytes,
        })
        .to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.response.success_for_logging()
    }

    fn provenance(&self) -> codex_tools::ToolOutputProvenance {
        self.response.provenance()
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.response.to_response_item(call_id, payload)
    }
}

impl ToolExecutor<ToolInvocation> for ReadToolOutputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.into(),
        description: "Read spilled output with mode bytes (default), lines, or search. Offset is a zero-based byte except for one-based lines; line continuations may provide byte_offset, line_continuation, and scan_lines_remaining. Limit is bytes, lines, or matches. Search requires query.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([
                    (
                        "artifact_id".into(),
                        JsonSchema::string(/*description*/ None),
                    ),
                    (
                        "mode".into(),
                        JsonSchema::string_enum(
                            vec![json!("bytes"), json!("lines"), json!("search")],
                            /*description*/ None,
                        ),
                    ),
                    (
                        "offset".into(),
                        JsonSchema::integer(/*description*/ None),
                    ),
                    (
                        "byte_offset".into(),
                        JsonSchema::integer(/*description*/ None),
                    ),
                    (
                        "scan_continuation".into(),
                        JsonSchema::boolean(/*description*/ None),
                    ),
                    (
                        "line_continuation".into(),
                        JsonSchema::boolean(/*description*/ None),
                    ),
                    (
                        "scan_lines_remaining".into(),
                        JsonSchema::integer(/*description*/ None),
                    ),
                    (
                        "limit".into(),
                        JsonSchema::integer(/*description*/ None),
                    ),
                    ("query".into(), JsonSchema::string(/*description*/ None)),
                ]),
                Some(vec!["artifact_id".into()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_tool_output requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(arguments)?;
            let id = OutputArtifactId::parse(&args.artifact_id).map_err(retrieval_error)?;
            invocation.turn.session_telemetry.counter(
                "codex.tool_output.artifact_retrieval",
                /*inc*/ 1,
                &[("rule", "bounded_artifact_retrieval_v1")],
            );
            if !invocation.session.output_artifact_spilling_supported() {
                return Err(FunctionCallError::RespondToModel(
                    "output artifact retrieval is unavailable for this thread backend".into(),
                ));
            }
            let store = invocation.session.output_artifact_store().await;
            let output_budget = effective_output_budget(&invocation);
            if output_budget == 0 {
                return Err(FunctionCallError::RespondToModel(
                    "read_tool_output requires a positive output policy".into(),
                ));
            }
            if output_budget < MIN_RETRIEVAL_OUTPUT_BYTES
                || output_budget <= RETRIEVAL_METADATA_RESERVE_BYTES
            {
                return Ok(boxed_tool_output(ReadToolOutputResult::error(
                    crate::tool_output::bounded_structured_error(output_budget),
                    "bytes",
                )));
            }
            if matches!(&args.mode, Mode::Lines) && output_budget < MIN_LINE_RETRIEVAL_OUTPUT_BYTES
            {
                return Ok(boxed_tool_output(ReadToolOutputResult::error(
                    crate::tool_output::bounded_structured_error(output_budget),
                    "lines",
                )));
            }
            let window_bytes = output_budget
                .saturating_sub(RETRIEVAL_METADATA_RESERVE_BYTES)
                .min(DEFAULT_BYTES);
            let (mode, value) = match args.mode {
                Mode::Bytes if args.query.is_none() => {
                    let value = read_bytes_value(
                        &store,
                        &id,
                        args.offset.unwrap_or(0),
                        args.limit.unwrap_or(window_bytes).min(window_bytes),
                    )
                    .await
                    .map_err(retrieval_error)?;
                    ("bytes", value)
                }
                Mode::Lines if args.query.is_none() => {
                    let start =
                        usize::try_from(args.offset.unwrap_or(1)).map_err(|_| invalid_request())?;
                    let (
                        text,
                        start_byte,
                        end_byte,
                        next_line,
                        next_byte,
                        scan_continues,
                        line_continuation,
                        scan_lines_remaining,
                    ) = read_lines(
                        &store,
                        &id,
                        start,
                        args.byte_offset.unwrap_or(0),
                        args.scan_continuation.unwrap_or(false),
                        args.line_continuation.unwrap_or(false),
                        args.scan_lines_remaining,
                        args.limit.unwrap_or(200),
                    )
                    .await
                    .map_err(retrieval_error)?;
                    let mut value = json!({"type":"tool_output_artifact_window","mode":"lines","artifact_id":id.as_str(),"start_line":start,"start_byte":start_byte,"end_byte":end_byte,"text":text,"next_offset":next_line,"next_byte":next_byte,"scan_continues":scan_continues,"scan_lines_remaining":scan_lines_remaining,"complete":next_line.is_none() && next_byte.is_none()});
                    if line_continuation {
                        value["line_continuation"] = true.into();
                    }
                    ("lines", value)
                }
                Mode::Search => {
                    let query = args.query.ok_or_else(invalid_request)?;
                    let (offsets, next) = search(
                        &store,
                        &id,
                        &query,
                        args.offset.unwrap_or(0),
                        args.limit.unwrap_or(20),
                        MAX_MATCHES,
                    )
                    .await
                    .map_err(retrieval_error)?;
                    (
                        "search",
                        json!({"type":"tool_output_artifact_search","artifact_id":id.as_str(),"byte_offsets":offsets,"next_offset":next,"complete":next.is_none()}),
                    )
                }
                Mode::Bytes | Mode::Lines => return Err(invalid_request()),
            };
            let text = bounded_result(value, output_budget);
            invocation.turn.session_telemetry.histogram(
                "codex.tool_output.retrieved_bytes",
                i64::try_from(text.len()).unwrap_or(i64::MAX),
                &[("mode", mode)],
            );
            Ok(boxed_tool_output(ReadToolOutputResult::new(text, mode)))
        })
    }
}

fn effective_output_budget(invocation: &ToolInvocation) -> usize {
    invocation
        .turn
        .tool_output_truncation_policy()
        .byte_budget()
        .min(MAX_MANAGED_ARTIFACT_MODEL_BYTES)
        .min(STORE_BACKED_TOOL_OUTPUT_MAX_BYTES)
}

fn invalid_request() -> FunctionCallError {
    FunctionCallError::RespondToModel("invalid output artifact request".into())
}

fn retrieval_error(error: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(
        match error.kind() {
            std::io::ErrorKind::NotFound => "output artifact is unavailable or expired",
            std::io::ErrorKind::InvalidInput => "invalid output artifact request",
            std::io::ErrorKind::PermissionDenied => {
                "output artifact failed managed-storage safety checks"
            }
            _ => "output artifact could not be read",
        }
        .into(),
    )
}

impl CoreToolRuntime for ReadToolOutputHandler {
    fn tool_result_log_policy(&self) -> ToolResultLogPolicy {
        ToolResultLogPolicy::ContentFree {
            tool_family: "read_tool_output",
        }
    }
}

#[cfg(test)]
#[path = "read_tool_output_tests.rs"]
mod tests;
