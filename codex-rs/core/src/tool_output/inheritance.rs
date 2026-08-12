use codex_history::CodexHarnessMetadata;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::OutputArtifactId;
use serde_json::Value;
use std::collections::HashSet;

/// Returns the store-backed artifacts referenced by forkable model history.
///
/// Only harness-authored provenance is accepted. Artifact-shaped tool text without
/// that sidecar remains ordinary untrusted output.
pub(crate) fn referenced_output_artifact_ids(history: &InitialHistory) -> Vec<OutputArtifactId> {
    let mut ids = HashSet::new();
    for item in history.get_rollout_items() {
        match item {
            RolloutItem::ResponseItem(envelope) => collect_envelope_ids(envelope, &mut ids),
            RolloutItem::Compacted(compacted) => {
                if let Some(replacement_history) = &compacted.replacement_history {
                    for envelope in replacement_history {
                        collect_envelope_ids(envelope, &mut ids);
                    }
                }
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::EventMsg(_) => {}
        }
    }
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    ids
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
