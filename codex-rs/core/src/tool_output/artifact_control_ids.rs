use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::OutputArtifactId;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;

pub(crate) const MAX_PROVIDER_IDENTIFIER_BYTES: usize = 64;

const LEGACY_SYNTHETIC_CALL_ID_PREFIX: &str = "artifact_ref_";
const LEGACY_SYNTHETIC_CALL_ID_LEN: usize = 77;
const SYNTHETIC_CALL_ID_DIGEST_HEX_CHARS: usize = 46;

/// Allocates deterministic provider-safe IDs for managed artifact controls.
///
/// The first candidate retains 184 bits of the artifact digest. A fixed-width collision suffix
/// keeps every candidate within the provider's 64-byte identifier limit while making collision
/// resolution deterministic for the order in which a history is replayed.
pub(crate) struct SyntheticArtifactCallIdAllocator {
    used_ids: HashSet<String>,
    assignments: HashMap<OutputArtifactId, String>,
}

impl SyntheticArtifactCallIdAllocator {
    pub(crate) fn for_history(history: &[ResponseItemEnvelope]) -> Self {
        let mut used_ids = HashSet::new();
        for envelope in history {
            collect_provider_identifiers(&envelope.item, &mut used_ids);
        }
        Self {
            used_ids,
            assignments: HashMap::new(),
        }
    }

    pub(crate) fn next(&mut self, artifact_id: &OutputArtifactId) -> Option<String> {
        if let Some(call_id) = self.assignments.get(artifact_id) {
            return Some(call_id.clone());
        }

        for ordinal in 0..=u16::MAX {
            let call_id = synthetic_candidate(artifact_id, ordinal);
            if self.used_ids.insert(call_id.clone()) {
                self.assignments
                    .insert(artifact_id.clone(), call_id.clone());
                return Some(call_id);
            }
        }
        None
    }
}

/// Repairs only the legacy managed artifact controls emitted before synthetic IDs were bounded.
///
/// This changes the in-memory history slice supplied by callers; persisted rollout records are
/// intentionally left untouched. The exact call/output shape and trusted metadata are required
/// so a user or provider-controlled ID cannot gain managed-artifact privileges by prefix alone.
pub(crate) fn canonicalize_legacy_artifact_controls(history: &mut [ResponseItemEnvelope]) {
    let mut allocator = SyntheticArtifactCallIdAllocator::for_history(history);
    for call_index in 0..history.len() {
        let Some(legacy_call_id) = response_call_id(&history[call_index].item)
            .filter(|call_id| call_id.len() == LEGACY_SYNTHETIC_CALL_ID_LEN)
            .map(str::to_string)
        else {
            continue;
        };
        let output_indices = history
            .iter()
            .enumerate()
            .filter(|(_, envelope)| {
                response_output_call_id(&envelope.item) == Some(legacy_call_id.as_str())
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if output_indices.len() != 1 {
            continue;
        }
        let output_index = output_indices[0];
        let Some(artifact_id) = trusted_legacy_artifact_id(
            &history[call_index],
            &history[output_index],
            &legacy_call_id,
        ) else {
            continue;
        };
        let Some(new_call_id) = allocator.next(&artifact_id) else {
            continue;
        };
        set_response_call_id(&mut history[call_index].item, &new_call_id);
        set_response_output_call_id(&mut history[output_index].item, &new_call_id);
    }
}

/// Returns whether all Responses tool-call identifiers fit the provider-bound limit.
///
/// Provider-authored response item IDs deliberately remain permissive for rollout compatibility;
/// only `call_id` fields carry the 64-byte contract that this feature violated.
pub(crate) fn provider_call_ids_within_limit(items: &[ResponseItem]) -> bool {
    items.iter().all(|item| {
        response_call_id(item)
            .or_else(|| response_output_call_id(item))
            .is_none_or(|call_id| call_id.len() <= MAX_PROVIDER_IDENTIFIER_BYTES)
    })
}

pub(crate) fn synthetic_retrieval_arguments(artifact_id: &OutputArtifactId) -> String {
    json!({
        "artifact_id": artifact_id.as_str(),
        "mode": "bytes",
        "offset": 0,
        "limit": 1,
    })
    .to_string()
}

pub(crate) fn synthetic_artifact_control_body(artifact_id: &OutputArtifactId) -> String {
    json!({
        "type": "tool_output_artifact",
        "version": 1,
        "artifact_id": artifact_id.as_str(),
        "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
    })
    .to_string()
}

fn synthetic_candidate(artifact_id: &OutputArtifactId, ordinal: u16) -> String {
    let digest = artifact_id.digest();
    debug_assert!(digest.len() >= SYNTHETIC_CALL_ID_DIGEST_HEX_CHARS);
    if ordinal == 0 {
        return format!(
            "{LEGACY_SYNTHETIC_CALL_ID_PREFIX}{}",
            &digest[..SYNTHETIC_CALL_ID_DIGEST_HEX_CHARS]
        );
    }

    format!(
        "{LEGACY_SYNTHETIC_CALL_ID_PREFIX}{}_{ordinal:04x}",
        &digest[..SYNTHETIC_CALL_ID_DIGEST_HEX_CHARS],
    )
}

fn trusted_legacy_artifact_id(
    call: &ResponseItemEnvelope,
    output: &ResponseItemEnvelope,
    legacy_call_id: &str,
) -> Option<OutputArtifactId> {
    if !output
        .metadata
        .as_ref()
        .is_some_and(CodexHarnessMetadata::is_store_backed_tool_output)
        || !canonical_read_tool_output_call(&call.item, legacy_call_id)
    {
        return None;
    }

    let artifact_id = match &output.item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            name: Some(name),
            namespace: None,
            output,
            ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id,
            name: Some(name),
            output,
            ..
        } if call_id == legacy_call_id && name == "read_tool_output" => {
            let FunctionCallOutputBody::Text(text) = &output.body else {
                return None;
            };
            let value = serde_json::from_str::<Value>(text).ok()?;
            let artifact_id = value.get("artifact_id")?.as_str()?;
            let artifact_id = OutputArtifactId::parse(artifact_id).ok()?;
            if value
                != serde_json::from_str::<Value>(&synthetic_artifact_control_body(&artifact_id))
                    .ok()?
            {
                return None;
            }
            artifact_id
        }
        _ => return None,
    };
    let expected_call_id = format!("{LEGACY_SYNTHETIC_CALL_ID_PREFIX}{}", artifact_id.digest());
    (legacy_call_id == expected_call_id).then_some(artifact_id)
}

fn canonical_read_tool_output_call(item: &ResponseItem, legacy_call_id: &str) -> bool {
    let (name, namespace, arguments, call_id) = match item {
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            call_id,
            ..
        } => (
            name.as_str(),
            namespace.as_deref(),
            arguments.as_str(),
            call_id,
        ),
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            call_id,
            ..
        } => (name.as_str(), namespace.as_deref(), input.as_str(), call_id),
        _ => return false,
    };
    if name != "read_tool_output" || namespace.is_some() || call_id != legacy_call_id {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    let Some(artifact_id) = value.get("artifact_id").and_then(Value::as_str) else {
        return false;
    };
    let Ok(artifact_id) = OutputArtifactId::parse(artifact_id) else {
        return false;
    };
    value
        == serde_json::from_str(&synthetic_retrieval_arguments(&artifact_id)).unwrap_or(Value::Null)
}

fn collect_provider_identifiers(item: &ResponseItem, identifiers: &mut impl Extend<String>) {
    if let Some(id) = item.id() {
        identifiers.extend(std::iter::once(id.as_str().to_string()));
    }
    let call_id = match item {
        ResponseItem::LocalShellCall { call_id, .. }
        | ResponseItem::ToolSearchCall { call_id, .. }
        | ResponseItem::ToolSearchOutput { call_id, .. }
        | ResponseItem::FunctionCallOutput { call_id, .. } => call_id.as_deref(),
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    };
    if let Some(call_id) = call_id {
        identifiers.extend(std::iter::once(call_id.to_string()));
    }
}

fn response_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.as_str()),
        ResponseItem::LocalShellCall { call_id, .. }
        | ResponseItem::ToolSearchCall { call_id, .. }
        | ResponseItem::ToolSearchOutput { call_id, .. } => call_id.as_deref(),
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

fn set_response_call_id(item: &mut ResponseItem, call_id: &str) {
    match item {
        ResponseItem::FunctionCall {
            call_id: current, ..
        }
        | ResponseItem::CustomToolCall {
            call_id: current, ..
        } => *current = call_id.to_string(),
        ResponseItem::LocalShellCall {
            call_id: current, ..
        }
        | ResponseItem::ToolSearchCall {
            call_id: current, ..
        } => *current = Some(call_id.to_string()),
        _ => {}
    }
}

fn set_response_output_call_id(item: &mut ResponseItem, call_id: &str) {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id: current, ..
        } => *current = Some(call_id.to_string()),
        ResponseItem::CustomToolCallOutput {
            call_id: current, ..
        } => *current = call_id.to_string(),
        _ => {}
    }
}

#[cfg(test)]
#[path = "artifact_control_ids_tests.rs"]
mod tests;
