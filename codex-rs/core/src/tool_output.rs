use crate::context_manager::truncate_function_output_payload;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::STORE_BACKED_TOOL_OUTPUT_MAX_BYTES;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ToolOutputProvenance;
use codex_utils_output_truncation::OutputArtifactStore;
use codex_utils_output_truncation::content_type;
use serde_json::Value;
use serde_json::json;
use tracing::warn;

mod content_items;
mod event;
mod inheritance;
mod retention;

pub(crate) use inheritance::referenced_output_artifact_ids;
pub(crate) use retention::sweep_expired_artifacts_once;

pub(crate) const MIN_ARTIFACT_ENVELOPE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectionMeasurement {
    pub(crate) original_bytes: usize,
    pub(crate) inline_bytes: usize,
    pub(crate) outcome: &'static str,
    pub(crate) rule: &'static str,
    pub(crate) tool_family: &'static str,
}

#[derive(Clone)]
pub(crate) struct ToolOutputProjector {
    store: OutputArtifactStore,
    policy: TruncationPolicy,
    spilling_supported: bool,
}

impl ToolOutputProjector {
    pub(crate) fn new(store: OutputArtifactStore, policy: TruncationPolicy) -> Self {
        Self {
            store,
            policy,
            spilling_supported: true,
        }
    }

    pub(crate) fn with_spilling_supported(mut self, spilling_supported: bool) -> Self {
        self.spilling_supported = spilling_supported;
        self
    }

    pub(crate) async fn project_response_item(
        &self,
        item: &ResponseItem,
    ) -> (ResponseItemEnvelope, Option<ProjectionMeasurement>) {
        self.project_response_item_with_provenance(item, ToolOutputProvenance::Untrusted)
            .await
    }

    pub(crate) async fn project_response_item_with_provenance(
        &self,
        item: &ResponseItem,
        provenance: ToolOutputProvenance,
    ) -> (ResponseItemEnvelope, Option<ProjectionMeasurement>) {
        let mut item = item.clone();
        let (output, family) = match &mut item {
            ResponseItem::FunctionCallOutput { output, .. } => (output, "function"),
            ResponseItem::CustomToolCallOutput { output, .. } => (output, "custom"),
            _ => return (ResponseItemEnvelope::new(item), None),
        };
        let (projected, measurement, store_backed) =
            self.project_payload(output, family, provenance).await;
        *output = projected;
        let metadata = store_backed.then(CodexHarnessMetadata::store_backed_tool_output);
        (ResponseItemEnvelope { item, metadata }, Some(measurement))
    }

    async fn project_payload(
        &self,
        payload: &FunctionCallOutputPayload,
        family: &'static str,
        provenance: ToolOutputProvenance,
    ) -> (FunctionCallOutputPayload, ProjectionMeasurement, bool) {
        if provenance == ToolOutputProvenance::ManagedArtifactRetrieval
            && let FunctionCallOutputBody::Text(text) = &payload.body
            && text.len() <= STORE_BACKED_TOOL_OUTPUT_MAX_BYTES
        {
            return (
                payload.clone(),
                measurement(
                    family,
                    "managed_artifact_retrieval_v1",
                    text.len(),
                    text.len(),
                    "inline",
                ),
                true,
            );
        }
        if let FunctionCallOutputBody::ContentItems(items) = &payload.body {
            let original_bytes = items.iter().filter_map(text_item).map(String::len).sum();
            if original_bytes > self.policy.byte_budget() {
                return self.project_content_items(payload, family).await;
            }
            return (
                payload.clone(),
                measurement(
                    family,
                    "inline_v1",
                    original_bytes,
                    original_bytes,
                    "inline",
                ),
                false,
            );
        }
        let Some(text) = payload.body.to_text() else {
            return (
                payload.clone(),
                measurement(
                    family,
                    "non_text_v1",
                    /*original_bytes*/ 0,
                    /*inline_bytes*/ 0,
                    "inline",
                ),
                false,
            );
        };
        let original_bytes = text.len();
        if original_bytes <= self.policy.byte_budget() {
            return (
                payload.clone(),
                measurement(
                    family,
                    "inline_v1",
                    original_bytes,
                    original_bytes,
                    "inline",
                ),
                false,
            );
        }
        if !self.spilling_supported {
            let output = truncate_function_output_payload(payload, self.policy);
            let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
            return (
                output,
                measurement(
                    family,
                    "artifact_backend_unavailable_v1",
                    original_bytes,
                    inline_bytes,
                    "fallback",
                ),
                false,
            );
        }
        match self.store.store_text(&text).await {
            Ok(artifact) => {
                let rule = if artifact.reused {
                    "exact_digest_reuse_v1"
                } else {
                    "spill_v1"
                };
                let envelope = artifact_envelope(
                    &artifact,
                    &text,
                    self.policy.byte_budget().clamp(
                        MIN_ARTIFACT_ENVELOPE_BYTES,
                        STORE_BACKED_TOOL_OUTPUT_MAX_BYTES,
                    ),
                );
                let inline_bytes = envelope.len();
                (
                    replace_text(payload, envelope),
                    measurement(family, rule, original_bytes, inline_bytes, "spilled"),
                    true,
                )
            }
            Err(err) => {
                warn!(error_kind = ?err.kind(), "tool output spill failed; using bounded truncation");
                let output = truncate_function_output_payload(payload, self.policy);
                let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
                (
                    output,
                    measurement(
                        family,
                        "spill_failure_truncate_v1",
                        original_bytes,
                        inline_bytes,
                        "fallback",
                    ),
                    false,
                )
            }
        }
    }
}

fn text_item(item: &FunctionCallOutputContentItem) -> Option<&String> {
    match item {
        FunctionCallOutputContentItem::InputText { text } => Some(text),
        _ => None,
    }
}

fn text_item_mut(item: &mut FunctionCallOutputContentItem) -> Option<&mut String> {
    match item {
        FunctionCallOutputContentItem::InputText { text } => Some(text),
        _ => None,
    }
}

fn replace_text(payload: &FunctionCallOutputPayload, text: String) -> FunctionCallOutputPayload {
    let mut output = payload.clone();
    match &mut output.body {
        FunctionCallOutputBody::Text(value) => *value = text,
        FunctionCallOutputBody::ContentItems(items) => {
            items.retain(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }));
            items.insert(0, FunctionCallOutputContentItem::InputText { text });
        }
    }
    output
}

fn artifact_envelope(
    artifact: &codex_utils_output_truncation::StoredOutputArtifact,
    text: &str,
    max_bytes: usize,
) -> String {
    let envelope = artifact.envelope(content_type(text), max_bytes);
    let Ok(mut envelope) = serde_json::from_str::<Value>(&envelope) else {
        return envelope;
    };
    if let Some(execution) = unified_exec_metadata(text) {
        envelope["execution"] = execution;
    }
    let with_execution = envelope.to_string();
    if with_execution.len() <= max_bytes {
        with_execution
    } else {
        artifact.envelope(content_type(text), max_bytes)
    }
}

fn unified_exec_metadata(text: &str) -> Option<Value> {
    let (header, _) = text.split_once("\nOutput:\n")?;
    let mut lines = header.lines();
    let chunk_id = lines.next()?.strip_prefix("Chunk ID: ")?;
    let mut metadata = json!({"chunk_id": chunk_id});
    for line in lines {
        if let Some(value) = line
            .strip_prefix("Wall time: ")
            .and_then(|value| value.strip_suffix(" seconds"))
            .and_then(|value| value.parse::<f64>().ok())
        {
            metadata["wall_time_seconds"] = value.into();
        } else if let Some(value) = line
            .strip_prefix("Process exited with code ")
            .and_then(|value| value.parse::<i32>().ok())
        {
            metadata["exit_code"] = value.into();
        } else if let Some(value) = line
            .strip_prefix("Process running with session ID ")
            .and_then(|value| value.parse::<i32>().ok())
        {
            metadata["session_id"] = value.into();
        } else if let Some(value) = line
            .strip_prefix("Original token count: ")
            .and_then(|value| value.parse::<usize>().ok())
        {
            metadata["original_token_count"] = value.into();
        }
    }
    Some(metadata)
}

fn measurement(
    tool_family: &'static str,
    rule: &'static str,
    original_bytes: usize,
    inline_bytes: usize,
    outcome: &'static str,
) -> ProjectionMeasurement {
    ProjectionMeasurement {
        original_bytes,
        inline_bytes,
        outcome,
        rule,
        tool_family,
    }
}

#[cfg(test)]
#[path = "tool_output_tests.rs"]
mod tests;
