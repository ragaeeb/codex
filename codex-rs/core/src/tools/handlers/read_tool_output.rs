use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_history::STORE_BACKED_TOOL_OUTPUT_MAX_BYTES;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::MAX_ARTIFACT_READ_BYTES;
use codex_utils_output_truncation::OutputArtifactId;
use codex_utils_output_truncation::OutputArtifactStore;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "read_tool_output";
const DEFAULT_BYTES: usize = 16 * 1024;
const MAX_LINES: usize = 2_000;
const MAX_MATCHES: usize = 50;
const MAX_SEARCH_SCAN_BYTES: usize = 512 * 1024;

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
    limit: Option<usize>,
    query: Option<String>,
}

pub struct ReadToolOutputHandler;

impl ToolExecutor<ToolInvocation> for ReadToolOutputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.into(),
            description: "Read spilled output with mode bytes (default), lines, or search. Offset is a zero-based byte except for one-based lines; limit is bytes, lines, or matches. Search requires query.".into(),
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
            let ToolPayload::Function { arguments } = invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "read_tool_output requires function arguments".into(),
                ));
            };
            let args: Args = parse_arguments(&arguments)?;
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
            let output_budget = codex_protocol::protocol::TruncationPolicy::from(
                invocation.turn.model_info.truncation_policy,
            )
            .byte_budget()
            .clamp(
                crate::tool_output::MIN_ARTIFACT_ENVELOPE_BYTES,
                STORE_BACKED_TOOL_OUTPUT_MAX_BYTES,
            );
            let window_bytes = output_budget.saturating_sub(512).clamp(4, DEFAULT_BYTES);
            let (mode, value) = match args.mode {
                Mode::Bytes if args.query.is_none() => {
                    let (text, start, end, next) = store
                        .read_bytes(
                            &id,
                            args.offset.unwrap_or(0),
                            args.limit.unwrap_or(window_bytes).min(window_bytes),
                        )
                        .await
                        .map_err(retrieval_error)?;
                    (
                        "bytes",
                        json!({"type":"tool_output_artifact_window","mode":"bytes","artifact_id":id.as_str(),"start_byte":start,"end_byte":end,"text":text,"next_offset":next,"complete":next.is_none()}),
                    )
                }
                Mode::Lines if args.query.is_none() => {
                    let start =
                        usize::try_from(args.offset.unwrap_or(1)).map_err(|_| invalid_request())?;
                    let (text, start_byte, end_byte, next_line, next_byte) =
                        read_lines(&store, &id, start, args.limit.unwrap_or(200))
                            .await
                            .map_err(retrieval_error)?;
                    (
                        "lines",
                        json!({"type":"tool_output_artifact_window","mode":"lines","artifact_id":id.as_str(),"start_line":start,"start_byte":start_byte,"end_byte":end_byte,"text":text,"next_offset":next_line,"next_byte":next_byte,"complete":next_line.is_none() && next_byte.is_none()}),
                    )
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
            Ok(boxed_tool_output(
                FunctionToolOutput::from_managed_artifact_text(text),
            ))
        })
    }
}

fn bounded_result(mut value: serde_json::Value, max_bytes: usize) -> String {
    loop {
        let rendered = value.to_string();
        if rendered.len() <= max_bytes {
            return rendered;
        }
        let excess = rendered.len() - max_bytes;
        if let Some(text) = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        {
            if text.is_empty() {
                if let Some(fields) = value.as_object_mut() {
                    fields.remove("text");
                }
                continue;
            }
            let target = text.len().saturating_sub(excess.saturating_add(8));
            let target = (target..=text.len())
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(text.len());
            if target == text.len() {
                value["text"] = String::new().into();
            } else {
                value["text"] = text[..target].to_string().into();
            }
            if let Some(start_byte) = value.get("start_byte").and_then(serde_json::Value::as_u64) {
                let end_byte = start_byte.saturating_add(target as u64);
                value["end_byte"] = end_byte.into();
                value["complete"] = false.into();
                if value.get("mode").and_then(serde_json::Value::as_str) == Some("lines") {
                    value["next_offset"] = serde_json::Value::Null;
                    value["next_byte"] = end_byte.into();
                } else {
                    value["next_offset"] = end_byte.into();
                }
            }
            continue;
        }
        if let Some(offsets) = value
            .get_mut("byte_offsets")
            .and_then(serde_json::Value::as_array_mut)
            && !offsets.is_empty()
        {
            if let Some(next_offset) = offsets.pop() {
                value["next_offset"] = next_offset;
            }
            value["complete"] = false.into();
            continue;
        }
        return truncate_text(
            "output artifact metadata exceeded the configured tool output limit",
            TruncationPolicy::Bytes(max_bytes),
        );
    }
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

impl CoreToolRuntime for ReadToolOutputHandler {}

async fn read_lines(
    store: &OutputArtifactStore,
    id: &OutputArtifactId,
    start_line: usize,
    count: usize,
) -> std::io::Result<(String, u64, u64, Option<usize>, Option<u64>)> {
    if start_line == 0 || count == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "line bounds must be positive",
        ));
    }
    let count = count.min(MAX_LINES);
    let mut byte = 0;
    let mut lines_to_skip = start_line - 1;
    while lines_to_skip > 0 {
        let batch = lines_to_skip.min(MAX_LINES);
        let (offsets, next) = search(store, id, "\n", byte, batch, MAX_LINES).await?;
        if offsets.len() < batch {
            if let Some(next) = next {
                lines_to_skip -= offsets.len();
                byte = next;
                continue;
            }
            return Ok((String::new(), byte, byte, None, None));
        }
        byte = offsets[offsets.len() - 1] + 1;
        lines_to_skip -= batch;
    }
    let (breaks, _) = search(store, id, "\n", byte, count, MAX_LINES).await?;
    let line_end = (breaks.len() == count).then(|| breaks[count - 1] + 1);
    let max_bytes = line_end.map_or(DEFAULT_BYTES, |end| {
        usize::try_from(end - byte)
            .unwrap_or(usize::MAX)
            .min(DEFAULT_BYTES)
    });
    let (text, _, end, next_byte) = store.read_bytes(id, byte, max_bytes).await?;
    let next_line = line_end
        .filter(|line_end| *line_end == end && next_byte.is_some())
        .map(|_| start_line + count);
    Ok((text, byte, end, next_line, next_byte))
}

async fn search(
    store: &OutputArtifactStore,
    id: &OutputArtifactId,
    query: &str,
    offset: u64,
    limit: usize,
    ceiling: usize,
) -> std::io::Result<(Vec<u64>, Option<u64>)> {
    if query.is_empty() || query.len() > 1_024 || limit == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid search bounds",
        ));
    }
    let limit = limit.min(ceiling);
    let scan_end = offset.saturating_add(MAX_SEARCH_SCAN_BYTES as u64);
    let read_end = scan_end.saturating_add(query.len().saturating_sub(1) as u64);
    let (mut cursor, mut candidate, mut carry, mut offsets) =
        (offset, offset, String::new(), Vec::new());
    loop {
        let remaining = usize::try_from(read_end.saturating_sub(cursor))
            .unwrap_or(usize::MAX)
            .min(MAX_ARTIFACT_READ_BYTES);
        if remaining == 0 {
            return Ok((offsets, Some(scan_end)));
        }
        let (text, start, end, next) = store.read_bytes(id, cursor, remaining).await?;
        let base = start.saturating_sub(carry.len() as u64);
        carry.push_str(&text);
        for (index, _) in carry.match_indices(query) {
            let found = base + index as u64;
            if found >= candidate && found < scan_end {
                offsets.push(found);
                if offsets.len() == limit {
                    return Ok((offsets, Some(found + 1)));
                }
            }
        }
        let Some(next) = next else {
            return Ok((offsets, None));
        };
        if end >= read_end {
            return Ok((offsets, Some(scan_end)));
        }
        let keep = query.len().saturating_sub(1).min(carry.len());
        let keep = (carry.len() - keep..=carry.len())
            .find(|index| carry.is_char_boundary(*index))
            .unwrap_or(carry.len());
        candidate = end.saturating_sub((carry.len() - keep) as u64);
        carry.drain(..keep);
        cursor = next;
    }
}

#[cfg(test)]
#[path = "read_tool_output_tests.rs"]
mod tests;
