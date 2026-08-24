use codex_file_system::read_file_snapshot::read_file_snapshot_transaction;
use codex_otel::ToolResultLogPolicy;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ArgumentRepairPolicy;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::io;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_file_dedup::ReadFinalization;
use crate::tools::handlers::read_file_dedup::emit_error_metrics;
use crate::tools::handlers::read_file_dedup::finalize_read;
use crate::tools::handlers::read_file_spec::ReadFileToolOptions;
use crate::tools::handlers::read_file_spec::TOOL_NAME;
use crate::tools::handlers::read_file_spec::create_read_file_tool;
use crate::tools::handlers::read_file_window::DEFAULT_MAX_BYTES;
use crate::tools::handlers::read_file_window::DEFAULT_MAX_LINES;
use crate::tools::handlers::read_file_window::MAX_MODEL_VISIBLE_BYTES;
use crate::tools::handlers::read_file_window::MAX_RESPONSE_BYTES;
use crate::tools::handlers::read_file_window::MAX_RESPONSE_LINES;
use crate::tools::handlers::read_file_window::MIN_REQUEST_BYTES;
use crate::tools::handlers::read_file_window::ReadBounds;
use crate::tools::handlers::read_file_window::fit_window_to_response_budget;
use crate::tools::handlers::read_file_window::render_response;
use crate::tools::handlers::read_file_window::response_base_len;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_utils_output_truncation::approx_bytes_for_tokens;

const MAX_PATH_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_ENVIRONMENT_ID_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    max_lines: Option<usize>,
    #[serde(default)]
    environment_id: Option<String>,
}

pub(crate) struct ReadFileHandler {
    spec: Arc<ToolSpec>,
    argument_repair_policy: ArgumentRepairPolicy,
    code_mode_definitions: OnceLock<Vec<codex_code_mode::ToolDefinition>>,
}

impl ReadFileHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        let options = ReadFileToolOptions {
            include_environment_id,
        };
        let mut argument_repair_policy = ArgumentRepairPolicy::default();
        assert!(
            argument_repair_policy.allow_markdown_path("/path").is_ok(),
            "read_file path policy is a valid static pointer"
        );
        Self {
            spec: Arc::new(create_read_file_tool(options)),
            argument_repair_policy,
            code_mode_definitions: OnceLock::new(),
        }
    }
}

impl ToolExecutor<ToolInvocation> for ReadFileHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.as_ref().clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for ReadFileHandler {
    fn argument_repair_policy(&self) -> Option<ArgumentRepairPolicy> {
        Some(self.argument_repair_policy.clone())
    }

    fn tool_result_log_policy(&self) -> ToolResultLogPolicy {
        ToolResultLogPolicy::ContentFree {
            tool_family: "read_file",
        }
    }

    fn immutable_spec(&self) -> Option<&Arc<ToolSpec>> {
        Some(&self.spec)
    }

    fn cached_code_mode_definitions(&self) -> Option<&[codex_code_mode::ToolDefinition]> {
        Some(
            self.code_mode_definitions
                .get_or_init(|| {
                    let mut definitions = codex_tools::collect_code_mode_tool_definitions(
                        std::iter::once(self.spec.as_ref()),
                    );
                    for definition in &mut definitions {
                        definition.input_schema = None;
                    }
                    definitions
                })
                .as_slice(),
        )
    }
}

impl ReadFileHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let result = self.handle_call_inner(&invocation).await;
        if let Err(error) = &result {
            emit_error_metrics(&invocation, error);
        }
        result
    }

    async fn handle_call_inner(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "read_file requires function arguments".to_string(),
            ));
        };
        let args: ReadFileArgs = parse_arguments(arguments)?;
        if args.path.len() > MAX_PATH_ARGUMENT_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "read_file path must be at most {MAX_PATH_ARGUMENT_BYTES} bytes"
            )));
        }
        if args
            .environment_id
            .as_ref()
            .is_some_and(|environment_id| environment_id.len() > MAX_ENVIRONMENT_ID_BYTES)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "read_file environment_id must be at most {MAX_ENVIRONMENT_ID_BYTES} bytes"
            )));
        }
        let bounds = parse_bounds(&args)?;
        let step_context = invocation.step_context.clone();
        let turn = invocation.turn.clone();
        let Some(turn_environment) =
            resolve_tool_environment(&step_context.environments, args.environment_id.as_deref())?
        else {
            return Err(FunctionCallError::RespondToModel(
                "read_file is unavailable because this session has no execution environment"
                    .to_string(),
            ));
        };
        let path_uri = turn_environment.cwd().join(&args.path).map_err(|error| {
            path_error(
                &args.path,
                format!("could not resolve the path against the environment cwd: {error}"),
            )
        })?;
        let model_path = path_uri.inferred_native_path_string();
        let fs = turn_environment.environment.get_filesystem();
        let sandbox = turn
            .file_system_sandbox_context(/*additional_permissions*/ None, turn_environment);
        let snapshot = read_file_snapshot_transaction(
            fs.as_ref(),
            &path_uri,
            Some(&sandbox),
            codex_file_system::read_file_window::ReadFileWindowBounds {
                offset: bounds.offset,
                max_bytes: bounds.max_bytes,
                max_line_fragments: bounds.max_lines,
                max_line_fragment_chars:
                    crate::tools::handlers::read_file_window::MAX_LINE_FRAGMENT_CHARS,
            },
        )
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidInput => path_error(&model_path, error.to_string()),
            io::ErrorKind::Interrupted
            | io::ErrorKind::InvalidData
            | io::ErrorKind::Unsupported => read_window_error(&model_path, error),
            _ => filesystem_error(&model_path, error),
        })?;
        let canonical_path = snapshot.canonical_path;
        let metadata = snapshot.metadata;
        let snapshot_window = snapshot.window;

        let base_len = response_base_len(
            &model_path,
            metadata.size,
            metadata.modified_at_ms,
            bounds.offset,
            bounds.max_lines,
        );
        let response_max_bytes = effective_model_response_max_bytes(
            bounds,
            invocation.turn.model_info.truncation_policy,
            invocation.turn.config.tool_output_token_limit,
        );
        if response_max_bytes == 0 {
            return Err(FunctionCallError::RespondToModel(
                "read_file requires a positive output policy".to_string(),
            ));
        }
        if base_len > response_max_bytes {
            return Err(FunctionCallError::RespondToModel(format!(
                "read_file response metadata requires {base_len} bytes, above the model-facing limit of {response_max_bytes}"
            )));
        }

        let window = fit_window_to_response_budget(
            &model_path,
            metadata.size,
            metadata.modified_at_ms,
            snapshot_window,
            response_max_bytes,
        )
        .map_err(|error| read_window_error(&model_path, error))?;
        let inline_result =
            render_response(&model_path, metadata.size, metadata.modified_at_ms, &window)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        if inline_result.len() > response_max_bytes {
            return Err(FunctionCallError::RespondToModel(
                "read_file could not fit a structurally valid response within max_bytes"
                    .to_string(),
            ));
        }

        finalize_read(
            invocation,
            ReadFinalization {
                environment_id: &turn_environment.selection.environment_id,
                canonical_path,
                metadata: &metadata,
                window,
                inline_result,
                response_max_bytes,
            },
        )
        .await
    }
}

fn effective_model_response_max_bytes(
    bounds: ReadBounds,
    model_policy: codex_protocol::openai_models::TruncationPolicyConfig,
    configured_token_limit: Option<usize>,
) -> usize {
    let hard_cap = bounds.model_max_bytes();
    let model_bytes = TruncationPolicy::from(model_policy).byte_budget();
    let configured_bytes = configured_token_limit.map(approx_bytes_for_tokens);
    [hard_cap, model_bytes]
        .into_iter()
        .chain(configured_bytes)
        .min()
        .unwrap_or(hard_cap)
}

fn parse_bounds(args: &ReadFileArgs) -> Result<ReadBounds, FunctionCallError> {
    let offset = args.offset.unwrap_or(0);
    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let max_lines = args.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    if !(MIN_REQUEST_BYTES..=MAX_RESPONSE_BYTES).contains(&max_bytes) {
        return Err(FunctionCallError::RespondToModel(format!(
            "read_file max_bytes must be between {MIN_REQUEST_BYTES} and {MAX_RESPONSE_BYTES}"
        )));
    }
    if !(1..=MAX_RESPONSE_LINES).contains(&max_lines) {
        return Err(FunctionCallError::RespondToModel(format!(
            "read_file max_lines must be between 1 and {MAX_RESPONSE_LINES}"
        )));
    }
    Ok(ReadBounds {
        offset,
        max_bytes,
        max_lines,
    })
}

fn path_error(path: &str, detail: String) -> FunctionCallError {
    let diagnostic = format!("read_file {detail} at {}", quoted(path));
    if diagnostic.len() <= MAX_MODEL_VISIBLE_BYTES {
        FunctionCallError::RespondToModel(diagnostic)
    } else {
        FunctionCallError::RespondToModel(
            "read_file could not describe the requested path within the response limit".to_string(),
        )
    }
}

fn filesystem_error(path: &str, error: io::Error) -> FunctionCallError {
    let detail = match error.kind() {
        io::ErrorKind::NotFound => "could not find the file",
        io::ErrorKind::PermissionDenied => "was denied by the filesystem sandbox",
        io::ErrorKind::InvalidInput => "received an invalid filesystem path",
        _ => "could not access the file",
    };
    path_error(path, detail.to_string())
}

fn read_window_error(path: &str, error: io::Error) -> FunctionCallError {
    let detail = match error.kind() {
        io::ErrorKind::InvalidData => error.to_string(),
        io::ErrorKind::InvalidInput => error.to_string(),
        io::ErrorKind::PermissionDenied => "the filesystem sandbox denied the read".to_string(),
        io::ErrorKind::Interrupted => error.to_string(),
        io::ErrorKind::Unsupported => {
            "the selected filesystem executor does not support efficient byte-offset reads"
                .to_string()
        }
        _ => "the streamed filesystem read failed".to_string(),
    };
    path_error(path, detail)
}

fn quoted(path: &str) -> String {
    serde_json::to_string(path).unwrap_or_else(|_| "\"<unrepresentable path>\"".to_string())
}

#[cfg(test)]
#[path = "read_file_tests.rs"]
mod tests;
