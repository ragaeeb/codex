use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::OutputArtifactId;
use serde_json::Value;
use std::collections::HashSet;

const MAX_COMPACTION_ARTIFACT_CONTROLS: usize = 32;
const MAX_COMPACTION_ARTIFACT_CONTROL_BYTES: usize = 16 * 1024;
const MAX_COMPACTION_ARTIFACT_CONTROL_PAIR_BYTES: usize = 8 * 1024;
const MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES: usize = 256;

/// Returns the store-backed artifacts referenced by an effective fork history snapshot.
///
/// Callers must pass reconstructed history rather than the append-only rollout records. Only
/// harness-authored provenance is accepted; artifact-shaped tool text without that sidecar
/// remains ordinary untrusted output.
pub(crate) fn referenced_output_artifact_ids(
    history: &[ResponseItemEnvelope],
) -> Vec<OutputArtifactId> {
    // A paired control is still model-visible in the effective snapshot and must win over an
    // older history-only sidecar when the bounded fork inventory is full. Sidecar references fill
    // the remaining capacity only after all effective controls have been selected.
    let mut paired = paired_artifact_ids(history).into_iter().collect::<Vec<_>>();
    paired.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    paired.truncate(MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES);

    let mut ids = paired.clone();
    let mut seen = paired.into_iter().collect::<HashSet<_>>();
    for id in sidecar_artifact_ids(history) {
        if ids.len() >= MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES {
            break;
        }
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    ids
}

/// Adds a bounded history-only sidecar to an existing replacement item. The sidecar is persisted
/// with the rollout envelope but is never sent to the model, so a long thread cannot turn its
/// artifact inventory into model-visible context.
pub(crate) fn attach_artifact_reference_sidecar(
    history: &mut Vec<ResponseItemEnvelope>,
    artifact_ids: &[OutputArtifactId],
) {
    let ids = artifact_ids
        .iter()
        .take(MAX_ARTIFACT_REFERENCE_SIDECAR_ENTRIES)
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }
    if history.is_empty() {
        history.push(ResponseItemEnvelope::new(ResponseItem::Other));
    }
    let Some(carrier) = history.last_mut() else {
        return;
    };
    let mut metadata = carrier.metadata.clone().unwrap_or_default();
    metadata.set_store_backed_artifact_references(ids);
    carrier.metadata = Some(metadata);
}

/// Returns paired, trusted artifact controls that can be carried through a history replacement.
///
/// Compaction implementations are allowed to discard ordinary tool calls and outputs, but a
/// managed artifact needs one durable call/output group so a later resume or fork can still find
/// and copy its store entry. Only the effective history is inspected, and an output is retained
/// only when its matching call is present. Artifact-shaped text without harness metadata is never
/// promoted by this helper.
pub(crate) fn artifact_controls_for_compaction(
    history: &[ResponseItemEnvelope],
) -> Vec<ResponseItemEnvelope> {
    let mut controls = Vec::new();
    let mut seen = HashSet::new();
    let mut serialized_bytes = 0usize;
    let call_ids = history
        .iter()
        .filter_map(|envelope| response_call_id(&envelope.item))
        .collect::<HashSet<_>>();
    for envelope in history {
        if controls.len() / 2 >= MAX_COMPACTION_ARTIFACT_CONTROLS
            || !envelope
                .metadata
                .as_ref()
                .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
        {
            continue;
        }
        let Some(call_id) = response_output_call_id(&envelope.item) else {
            continue;
        };
        if !call_ids.contains(call_id) {
            continue;
        }
        let Some(artifact_id) = envelope_artifact_id(envelope) else {
            continue;
        };
        if !seen.insert(artifact_id.clone()) {
            continue;
        }
        let Some(call) = history
            .iter()
            .find(|candidate| response_call_id(&candidate.item) == Some(call_id))
        else {
            continue;
        };
        let Some((call, output)) = synthetic_artifact_control_pair(call, envelope, &artifact_id)
        else {
            continue;
        };
        let pair_bytes = serde_json::to_string(&call.item).ok().and_then(|call| {
            serde_json::to_string(&output.item)
                .ok()
                .map(|output| call.len().saturating_add(output.len()))
        });
        let Some(pair_bytes) = pair_bytes else {
            continue;
        };
        if serialized_bytes.saturating_add(pair_bytes) > MAX_COMPACTION_ARTIFACT_CONTROL_BYTES {
            break;
        }
        serialized_bytes = serialized_bytes.saturating_add(pair_bytes);
        controls.push(call);
        controls.push(output);
    }
    controls
}

/// Adds missing trusted artifact call/output groups to a replacement history.
pub(crate) fn merge_artifact_controls(
    history: Vec<ResponseItemEnvelope>,
    controls: Vec<ResponseItemEnvelope>,
) -> Vec<ResponseItemEnvelope> {
    // Provider-returned legacy compaction output is not allowed to redefine a trusted control.
    // Rebuild every trusted artifact pair from the bounded synthetic controls, which removes
    // echoed call arguments and caps both pair count and aggregate bytes.
    let existing_controls = artifact_controls_for_compaction(&history);
    let mut candidates = existing_controls;
    candidates.extend(controls);
    let mut seen = HashSet::new();
    let mut retained_controls = Vec::new();
    let mut serialized_bytes = 0usize;
    let mut candidates = candidates.into_iter();
    while let Some(call) = candidates.next() {
        let Some(output) = candidates.next() else {
            break;
        };
        let Some(artifact_id) = envelope_artifact_id(&output) else {
            continue;
        };
        let Some((call, output)) = synthetic_artifact_control_pair(&call, &output, &artifact_id)
        else {
            continue;
        };
        let Some(pair_bytes) = serde_json::to_string(&call.item).ok().and_then(|call| {
            serde_json::to_string(&output.item)
                .ok()
                .map(|output| call.len().saturating_add(output.len()))
        }) else {
            continue;
        };
        if retained_controls.len() / 2 >= MAX_COMPACTION_ARTIFACT_CONTROLS
            || pair_bytes > MAX_COMPACTION_ARTIFACT_CONTROL_PAIR_BYTES
            || serialized_bytes.saturating_add(pair_bytes) > MAX_COMPACTION_ARTIFACT_CONTROL_BYTES
            || !seen.insert(artifact_id)
        {
            continue;
        }
        serialized_bytes = serialized_bytes.saturating_add(pair_bytes);
        retained_controls.push(call);
        retained_controls.push(output);
    }

    let call_ids = history
        .iter()
        .filter_map(|envelope| response_call_id(&envelope.item))
        .collect::<HashSet<_>>();
    let paired_call_ids = history
        .iter()
        .filter(|envelope| {
            envelope
                .metadata
                .as_ref()
                .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
                && envelope_artifact_id(envelope).is_some()
        })
        .filter_map(|envelope| response_output_call_id(&envelope.item))
        .filter(|call_id| call_ids.contains(call_id))
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let history = history
        .into_iter()
        .filter(|envelope| {
            if envelope
                .metadata
                .as_ref()
                .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
                && envelope_artifact_id(envelope).is_some()
            {
                return false;
            }
            !response_call_id(&envelope.item)
                .is_some_and(|call_id| paired_call_ids.contains(call_id))
        })
        .collect::<Vec<_>>();
    retained_controls.extend(history);
    retained_controls
}

/// Returns whether an output item contains a syntactically valid managed artifact control.
///
/// History metadata is trusted only when the harness has already marked the item, but checking
/// the bounded control before retaining that metadata prevents a low-budget fallback error from
/// becoming falsely inheritable or recoverable.
pub(crate) fn has_recoverable_output_artifact(item: &ResponseItem) -> bool {
    let mut ids = HashSet::new();
    collect_response_item_ids(item, &mut ids);
    ids.len() == 1
}

fn collect_envelope_ids(envelope: &ResponseItemEnvelope, ids: &mut HashSet<OutputArtifactId>) {
    if !envelope
        .metadata
        .as_ref()
        .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
    {
        return;
    }
    let output = match &envelope.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return,
    };
    collect_response_item_ids_from_output(output, ids);
}

fn sidecar_artifact_ids(history: &[ResponseItemEnvelope]) -> Vec<OutputArtifactId> {
    let mut ids = history
        .iter()
        .flat_map(|envelope| {
            envelope
                .metadata
                .as_ref()
                .into_iter()
                .flat_map(CodexHarnessMetadata::store_backed_artifact_references)
        })
        .filter_map(|reference| OutputArtifactId::parse(&reference.artifact_id).ok())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    ids
}

fn paired_artifact_ids(history: &[ResponseItemEnvelope]) -> HashSet<OutputArtifactId> {
    let call_ids = history
        .iter()
        .filter_map(|envelope| response_call_id(&envelope.item))
        .collect::<HashSet<_>>();
    history
        .iter()
        .filter(|envelope| {
            envelope
                .metadata
                .as_ref()
                .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
        })
        .filter(|envelope| {
            response_output_call_id(&envelope.item)
                .is_some_and(|call_id| call_ids.contains(call_id))
        })
        .filter_map(envelope_artifact_id)
        .collect()
}

fn synthetic_artifact_control_pair(
    call: &ResponseItemEnvelope,
    output: &ResponseItemEnvelope,
    artifact_id: &OutputArtifactId,
) -> Option<(ResponseItemEnvelope, ResponseItemEnvelope)> {
    let call_id = format!("artifact_ref_{}", artifact_id.digest());
    let retrieval_arguments = serde_json::json!({
        "artifact_id": artifact_id.as_str(),
        "mode": "bytes",
        "offset": 0,
        "limit": 1,
    })
    .to_string();
    let call_item = match &call.item {
        ResponseItem::FunctionCall { .. } => ResponseItem::FunctionCall {
            id: None,
            name: "read_tool_output".to_string(),
            namespace: None,
            arguments: retrieval_arguments,
            encrypted_function_args: None,
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::LocalShellCall { .. } => ResponseItem::FunctionCall {
            id: None,
            name: "read_tool_output".to_string(),
            namespace: None,
            arguments: retrieval_arguments,
            encrypted_function_args: None,
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall { .. } => ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: call_id.clone(),
            name: "read_tool_output".to_string(),
            namespace: None,
            input: retrieval_arguments,
            internal_chat_message_metadata_passthrough: None,
        },
        _ => return None,
    };
    let body = FunctionCallOutputBody::Text(
        serde_json::json!({
        "type": "tool_output_artifact",
            "version": 1,
            "artifact_id": artifact_id.as_str(),
            "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
        })
        .to_string(),
    );
    let output_item = match &output.item {
        ResponseItem::FunctionCallOutput { .. } => ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id),
            name: Some("read_tool_output".to_string()),
            namespace: None,
            output: FunctionCallOutputPayload {
                body,
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput { .. } => ResponseItem::CustomToolCallOutput {
            id: None,
            call_id,
            name: Some("read_tool_output".to_string()),
            output: FunctionCallOutputPayload {
                body,
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        _ => return None,
    };
    Some((
        ResponseItemEnvelope::new(call_item),
        ResponseItemEnvelope {
            item: output_item,
            metadata: Some(CodexHarnessMetadata::store_backed_tool_output()),
        },
    ))
}

fn collect_response_item_ids(item: &ResponseItem, ids: &mut HashSet<OutputArtifactId>) {
    let output = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return,
    };
    collect_response_item_ids_from_output(output, ids);
}

fn collect_response_item_ids_from_output(
    output: &codex_protocol::models::FunctionCallOutputPayload,
    ids: &mut HashSet<OutputArtifactId>,
) {
    match &output.body {
        FunctionCallOutputBody::Text(text) => collect_text_id(text, ids),
        FunctionCallOutputBody::ContentItems(items) => {
            for item in items {
                if let FunctionCallOutputContentItem::InputText { text } = item {
                    collect_text_id(text, ids);
                }
            }
        }
    }
}

fn response_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.as_str()),
        ResponseItem::LocalShellCall { call_id, .. } => call_id.as_deref(),
        _ => None,
    }
}

fn response_output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => call_id.as_deref(),
        ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    }
}

fn envelope_artifact_id(envelope: &ResponseItemEnvelope) -> Option<OutputArtifactId> {
    let mut ids = HashSet::new();
    collect_envelope_ids(envelope, &mut ids);
    (ids.len() == 1).then(|| ids.into_iter().next()).flatten()
}

fn collect_text_id(text: &str, ids: &mut HashSet<OutputArtifactId>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some(
            "tool_output_artifact" | "tool_output_artifact_window" | "tool_output_artifact_search"
        )
    ) {
        return;
    }
    if let Some(id) = value
        .get("artifact_id")
        .and_then(Value::as_str)
        .and_then(|id| OutputArtifactId::parse(id).ok())
    {
        ids.insert(id);
    }
}

#[cfg(test)]
#[path = "inheritance_tests.rs"]
mod tests;
