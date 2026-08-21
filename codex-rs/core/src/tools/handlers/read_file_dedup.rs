use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::read_file_ledger::ReadLedger;
use crate::tools::handlers::read_file_ledger::ReadLedgerKey;
use crate::tools::handlers::read_file_ledger::ReadLedgerObservation;
use crate::tools::handlers::read_file_output::FILE_READ_CONTENT_TYPE;
use crate::tools::handlers::read_file_output::ReadFileToolOutput;
use crate::tools::handlers::read_file_output::inline_result_digest;
use codex_exec_server::FileMetadata;
use codex_file_system::read_file_window::ReadFileWindow;
use codex_otel::SessionTelemetry;
use codex_utils_output_truncation::OutputArtifactId;
use codex_utils_output_truncation::try_artifact_envelope;
use codex_utils_path_uri::PathUri;
use codex_utils_string::take_bytes_at_char_boundary;

const ARTIFACT_SAVINGS_MARGIN: usize = 2;
const MAX_READ_FILE_ARTIFACT_ENVELOPE_BYTES: usize = 768;
const ARTIFACT_PREVIEW_BYTES: usize = 384;

pub(super) struct ReadFinalization<'a> {
    pub(super) environment_id: &'a str,
    pub(super) canonical_path: PathUri,
    pub(super) metadata: &'a FileMetadata,
    pub(super) window: ReadFileWindow,
    pub(super) inline_result: String,
    pub(super) response_max_bytes: usize,
}

pub(super) async fn finalize_read(
    invocation: &ToolInvocation,
    input: ReadFinalization<'_>,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    finalize_read_with_spilling_support(
        invocation,
        input,
        invocation.session.output_artifact_spilling_supported(),
    )
    .await
}

async fn finalize_read_with_spilling_support(
    invocation: &ToolInvocation,
    input: ReadFinalization<'_>,
    spilling_supported: bool,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ReadFinalization {
        environment_id,
        canonical_path,
        metadata,
        window,
        inline_result,
        response_max_bytes,
    } = input;
    let ledger = invocation
        .session
        .services
        .thread_extension_data
        .get_or_init(ReadLedger::default);
    let ledger_key = ReadLedgerKey {
        environment_id: environment_id.to_string(),
        canonical_path,
        file_size: metadata.size,
        created_at_ms: metadata.created_at_ms,
        modified_at_ms: metadata.modified_at_ms,
        start_byte: window.start_byte,
        end_byte: window.end_byte,
        next_offset: window.next_offset,
        eof: window.eof,
        line_continues: window.line_continues,
    };
    let serialized_digest = inline_result_digest(&inline_result);
    let observation = ledger.observe(ledger_key, serialized_digest);

    let (output_text, rule, outcome) = match observation {
        ReadLedgerObservation::FirstRead => (inline_result, "inline_v1", "inline"),
        ReadLedgerObservation::FingerprintChanged => {
            (inline_result, "fingerprint_changed_v1", "inline")
        }
        ReadLedgerObservation::ExactDuplicate => {
            duplicate_output(
                invocation,
                &inline_result,
                response_max_bytes,
                spilling_supported,
            )
            .await?
        }
    };
    emit_metrics(
        &invocation.turn.session_telemetry,
        window.end_byte.saturating_sub(window.start_byte),
        output_text.len(),
        rule,
        outcome,
    );
    let managed = outcome == "artifact";
    Ok(boxed_tool_output(ReadFileToolOutput::new(
        output_text,
        managed,
        rule,
    )?))
}

pub(super) fn emit_error_metrics(invocation: &ToolInvocation, error: &FunctionCallError) {
    let rule = error_metric_rule(error);
    emit_metrics(
        &invocation.turn.session_telemetry,
        /*source_bytes*/ 0,
        /*inline_bytes*/ 0,
        rule,
        "error",
    );
}

fn error_metric_rule(error: &FunctionCallError) -> &'static str {
    match error {
        FunctionCallError::RespondToModel(message)
            if message.contains("not valid UTF-8") || message.contains("NUL") =>
        {
            "binary_rejected_v1"
        }
        FunctionCallError::RespondToModel(message) if message.contains("sandbox") => {
            "sandbox_denied_v1"
        }
        FunctionCallError::RespondToModel(message)
            if message.contains("max_bytes") || message.contains("max_lines") =>
        {
            "invalid_bounds_v1"
        }
        _ => "read_error_v1",
    }
}

async fn duplicate_output(
    invocation: &ToolInvocation,
    inline_result: &str,
    max_bytes: usize,
    spilling_supported: bool,
) -> Result<(String, &'static str, &'static str), FunctionCallError> {
    if matches!(invocation.source, ToolCallSource::CodeMode { .. }) {
        return Ok((inline_result.to_string(), "code_mode_inline_v1", "inline"));
    }
    let envelope_budget = max_bytes.min(MAX_READ_FILE_ARTIFACT_ENVELOPE_BYTES);
    let Some(candidate) = estimated_artifact_envelope(inline_result, envelope_budget) else {
        return Ok((
            inline_result.to_string(),
            "artifact_not_economical_v1",
            "inline",
        ));
    };
    if candidate.len().saturating_add(ARTIFACT_SAVINGS_MARGIN) >= inline_result.len() {
        return Ok((
            inline_result.to_string(),
            "artifact_not_economical_v1",
            "inline",
        ));
    }
    if !spilling_supported {
        return Ok((
            inline_result.to_string(),
            "artifact_backend_unavailable_v1",
            "inline",
        ));
    }
    let store = invocation.session.output_artifact_store().await;
    match store.store_text(inline_result).await {
        // `candidate` is rendered from the same deterministic digest/preview inputs as the
        // stored artifact. Reusing it avoids creating an artifact first and then discovering
        // that a second rendering is not economical.
        Ok(_artifact) => Ok((candidate, "exact_window_artifact_v1", "artifact")),
        Err(_) => Ok((
            inline_result.to_string(),
            "artifact_store_fallback_v1",
            "inline",
        )),
    }
}

fn estimated_artifact_envelope(inline_result: &str, max_bytes: usize) -> Option<String> {
    let artifact_id = OutputArtifactId::for_text(inline_result);
    let lines = inline_result.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!inline_result.is_empty() && !inline_result.ends_with('\n'));
    let preview_head = take_bytes_at_char_boundary(inline_result, ARTIFACT_PREVIEW_BYTES);
    let tail_start = (inline_result.len().saturating_sub(ARTIFACT_PREVIEW_BYTES)
        ..=inline_result.len())
        .find(|index| inline_result.is_char_boundary(*index))
        .unwrap_or(inline_result.len());
    try_artifact_envelope(
        &artifact_id,
        FILE_READ_CONTENT_TYPE,
        inline_result.len(),
        lines,
        preview_head,
        &inline_result[tail_start..],
        max_bytes,
    )
}

fn emit_metrics(
    telemetry: &SessionTelemetry,
    source_bytes: u64,
    inline_bytes: usize,
    rule: &'static str,
    outcome: &'static str,
) {
    let tags = [
        ("rule", rule),
        ("outcome", outcome),
        ("tool_family", "read_file"),
    ];
    telemetry.counter("codex.tool.read_file.result", /*inc*/ 1, &tags);
    telemetry.histogram(
        "codex.tool.read_file.source_bytes",
        i64::try_from(source_bytes).unwrap_or(i64::MAX),
        &[("tool_family", "read_file")],
    );
    telemetry.histogram(
        "codex.tool.read_file.inline_bytes",
        i64::try_from(inline_bytes).unwrap_or(i64::MAX),
        &[("tool_family", "read_file")],
    );
}

#[cfg(test)]
#[path = "read_file_dedup_tests.rs"]
mod tests;
