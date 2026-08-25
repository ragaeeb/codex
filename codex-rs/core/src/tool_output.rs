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
use codex_utils_output_truncation::OutputArtifactId;
use codex_utils_output_truncation::OutputArtifactStore;
use codex_utils_output_truncation::content_type;
use serde_json::Value;
use serde_json::json;
use tracing::warn;

mod artifact_control_ids;
mod content_items;
mod event;
mod inheritance;
mod retention;

pub(crate) use artifact_control_ids::canonicalize_legacy_artifact_controls;
pub(crate) use artifact_control_ids::provider_call_ids_within_limit;
pub(crate) use inheritance::artifact_controls_for_compaction;
pub(crate) use inheritance::attach_artifact_reference_sidecar;
pub(crate) use inheritance::has_recoverable_output_artifact;
pub(crate) use inheritance::merge_artifact_controls;
pub(crate) use inheritance::referenced_output_artifact_ids;
pub(crate) use retention::sweep_expired_artifacts_once;

/// Smallest practical model-facing budget that can carry a recoverable artifact window.
///
/// This is a guard, not a renderer: callers still use `try_envelope` and fail closed if a future
/// schema change makes the concrete control larger. The floor is above the identity-only envelope
/// because retrieval must have room for a bounded window and continuation metadata as well.
pub(crate) const MIN_ARTIFACT_ENVELOPE_BYTES: usize = 200;
/// Independent ceiling for a single managed output item entering model context.
///
/// This is deliberately separate from configured truncation policies and the larger artifact
/// storage/retrieval limit. Eight KiB is a tokenizer-independent ceiling well below the
/// ten-thousand-token hard limit under the conservative one-byte-per-token bound. It intentionally
/// crosses the repository's one-thousand-token manual-review gate; the explicit Stage 2 review
/// acceptance for this cap is recorded here instead of treating it as an ordinary limit.
pub(crate) const MAX_MANAGED_ARTIFACT_MODEL_BYTES: usize = 8 * 1024;
/// Aggregate budget for one content-item output after modality-specific accounting. Media inputs
/// use their own token estimator, so a normal resized image may cost more than the text-item gate
/// while remaining far below the ten-thousand-token invariant.
pub(crate) const MAX_CONTENT_ITEMS_MODEL_BYTES: usize = 8 * 1024;
/// Independent serialized-size ceiling for one content-item response. Modality estimates account
/// for the model's image/audio cost, while this bound prevents a large encoded media body from
/// bypassing the ordinary per-item serialization guard.
pub(crate) const MAX_CONTENT_ITEMS_SERIALIZED_BYTES: usize = 128 * 1024;
/// Reserve room for the surrounding Responses function-output item when a managed content-item
/// control is fitted. The generic output policy is expressed in body bytes, but a content-item
/// control is persisted as a complete model-visible item.
pub(crate) const MODEL_ITEM_CONTROL_RESERVATION_BYTES: usize = 128;

pub(crate) fn bounded_structured_error(max_bytes: usize) -> String {
    let error = json!({
        "type": "tool_output_error",
        "version": 1,
        "error": "tool output exceeded the active model output policy"
    })
    .to_string();
    if error.len() <= max_bytes {
        error
    } else {
        [
            r#"{"type":"tool_output_error","version":1}"#,
            r#"{"type":"tool_output_error"}"#,
            r#"{"type":"error"}"#,
            "{}",
            "0",
            "",
        ]
        .into_iter()
        .find(|candidate| candidate.len() <= max_bytes)
        .unwrap_or_default()
        .to_string()
    }
}

pub(crate) fn bounded_output_payload(
    output: &FunctionCallOutputPayload,
    max_bytes: usize,
) -> FunctionCallOutputPayload {
    let text = bounded_structured_error(max_bytes);
    FunctionCallOutputPayload {
        // Collapse content-item payloads to one text control so the fallback has no aggregate
        // wrapper siblings that could exceed a tiny policy or inherit a stale sidecar.
        body: FunctionCallOutputBody::Text(text),
        // Zero is a representable legacy policy but cannot carry even a structured diagnostic.
        // Mark the empty body as an explicit fail-closed tool result rather than implying that an
        // empty successful response was recovered.
        success: (max_bytes == 0).then_some(false).or(output.success),
    }
}

/// Re-renders a trusted artifact control document without its previews when a
/// smaller policy would otherwise discard the recovery handle. This is only
/// called for harness-marked output; an arbitrary JSON object must never gain
/// managed-artifact semantics from this parser.
pub(crate) fn compact_store_backed_envelope(text: &str, max_bytes: usize) -> Option<String> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let response_type = value.get("type")?.as_str()?;
    if !matches!(
        response_type,
        "tool_output_artifact" | "tool_output_artifact_window" | "tool_output_artifact_search"
    ) {
        return None;
    }
    let artifact_id = value.get("artifact_id")?.as_str()?;
    OutputArtifactId::parse(artifact_id).ok()?;
    let mut compact = json!({
        "type": response_type,
        "artifact_id": artifact_id,
    });
    if response_type == "tool_output_artifact" {
        compact["retrieval"] = Value::String(
            value
                .get("retrieval")
                .and_then(Value::as_str)
                .unwrap_or("Use read_tool_output with this artifact_id.")
                .to_string(),
        );
    }
    for field in [
        "mode",
        "start_byte",
        "start_line",
        "next_offset",
        "next_byte",
        "complete",
    ] {
        if let Some(field_value) = value.get(field) {
            compact[field] = field_value.clone();
        }
    }
    let with_continuation = compact.to_string();
    if with_continuation.len() <= max_bytes {
        return Some(with_continuation);
    }
    let id_only = json!({
        "type": response_type,
        "artifact_id": artifact_id,
    })
    .to_string();
    (id_only.len() <= max_bytes).then_some(id_only)
}

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
        if matches!(
            provenance,
            ToolOutputProvenance::ManagedArtifactRetrieval
                | ToolOutputProvenance::ManagedArtifactReference
        ) && let FunctionCallOutputBody::Text(text) = &payload.body
        {
            let managed_budget = self
                .policy
                .byte_budget()
                .min(STORE_BACKED_TOOL_OUTPUT_MAX_BYTES)
                .min(MAX_MANAGED_ARTIFACT_MODEL_BYTES);
            if text.len() <= managed_budget {
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
            let bounded = compact_store_backed_envelope(text, managed_budget);
            let store_backed = bounded.is_some();
            let bounded = bounded.unwrap_or_else(|| bounded_structured_error(managed_budget));
            return (
                replace_text(payload, bounded.clone()),
                measurement(
                    family,
                    "managed_artifact_policy_compact_v1",
                    text.len(),
                    bounded.len(),
                    "fallback",
                ),
                store_backed,
            );
        }
        if let FunctionCallOutputBody::ContentItems(_) = &payload.body {
            return self.project_content_items(payload, family).await;
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
        if self.policy.byte_budget() < MIN_ARTIFACT_ENVELOPE_BYTES {
            let output = truncate_function_output_payload(payload, self.policy);
            let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
            return (
                output,
                measurement(
                    family,
                    "artifact_policy_too_small_v1",
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
                let Some(envelope) = artifact_envelope(
                    &artifact,
                    &text,
                    self.policy
                        .byte_budget()
                        .min(STORE_BACKED_TOOL_OUTPUT_MAX_BYTES),
                ) else {
                    let output = truncate_function_output_payload(
                        payload,
                        TruncationPolicy::Bytes(
                            self.policy
                                .byte_budget()
                                .min(STORE_BACKED_TOOL_OUTPUT_MAX_BYTES),
                        ),
                    );
                    let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
                    return (
                        output,
                        measurement(
                            family,
                            "artifact_control_unavailable_v1",
                            original_bytes,
                            inline_bytes,
                            "fallback",
                        ),
                        false,
                    );
                };
                let inline_bytes = envelope.len();
                let store_backed = serde_json::from_str::<Value>(&envelope)
                    .ok()
                    .is_some_and(|value| value["type"] == "tool_output_artifact");
                (
                    replace_text(payload, envelope),
                    measurement(family, rule, original_bytes, inline_bytes, "spilled"),
                    store_backed,
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
) -> Option<String> {
    let envelope = artifact.try_envelope(content_type(text), max_bytes)?;
    let mut envelope = serde_json::from_str::<Value>(&envelope).ok()?;
    if let Some(execution) = unified_exec_metadata(text) {
        envelope["execution"] = execution;
    }
    let with_execution = envelope.to_string();
    if with_execution.len() <= max_bytes && envelope["type"] == "tool_output_artifact" {
        Some(with_execution)
    } else {
        artifact
            .try_envelope(content_type(text), max_bytes)
            .filter(|envelope| {
                serde_json::from_str::<Value>(envelope)
                    .ok()
                    .is_some_and(|value| value["type"] == "tool_output_artifact")
            })
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
