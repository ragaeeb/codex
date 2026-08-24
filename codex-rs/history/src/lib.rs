//! Model-history and persisted-rollout domain types.

use std::borrow::Borrow;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::mcp::McpResourceOriginCheckpoint;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::realtime::RealtimeItem;
use codex_protocol::security_risk::SecurityRiskScore;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use serde::de::SeqAccess;
use serde::de::Visitor;

/// A model-history item with room for history-only metadata.
///
/// Persistence keeps the response item intact and stores its metadata separately.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseItemEnvelope {
    pub item: ResponseItem,
    pub metadata: Option<CodexHarnessMetadata>,
}

/// Maximum serialized text carried by a store-backed tool-output control item.
///
/// This is intentionally independent of a model's normal tool-output policy:
/// the fixed metadata needed to recover an artifact can be larger than a very
/// small configured output limit, but must still have a hard context ceiling.
pub const STORE_BACKED_TOOL_OUTPUT_MAX_BYTES: usize = 32 * 1024;
/// Maximum history-only artifact references retained on one metadata carrier.
pub const MAX_STORE_BACKED_ARTIFACT_REFERENCES: usize = 256;
/// Maximum stable rule occurrences retained in one argument-repair receipt.
pub const MAX_TOOL_ARGUMENT_REPAIR_RULES: usize = 8;
/// Maximum byte value retained for one argument-repair measurement.
pub const MAX_TOOL_ARGUMENT_REPAIR_BYTES: usize = 64 * 1024;
/// Maximum candidate-work count retained in one argument-repair receipt.
pub const MAX_TOOL_ARGUMENT_REPAIR_CANDIDATE_WORK: usize = 4096;
/// Maximum repair duration retained in one argument-repair receipt.
pub const MAX_TOOL_ARGUMENT_REPAIR_DURATION_MICROS: u64 = 60 * 1_000_000;

/// Trusted tool families used by the bounded argument-repair cohort.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentRepairToolFamily {
    ReadFile,
    Unlisted,
}

/// Stable result categories for one argument-repair attempt.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentRepairOutcome {
    PolicyMiss,
    ValidUnchanged,
    Repaired,
    NotRepairable,
    LimitExceeded,
    UnsupportedSchema,
}

/// Stable reasons that can accompany a non-repaired attempt.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolArgumentRepairReason {
    NotAllowlisted,
    UnsupportedSchema,
    LimitExceeded,
    ValidationFailure,
}

/// Bounded history-only receipt for the effective arguments used by a tool handler.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct ToolArgumentRepairReceipt {
    pub tool_family: ToolArgumentRepairToolFamily,
    pub outcome: ToolArgumentRepairOutcome,
    #[serde(default, deserialize_with = "deserialize_bounded_rule_ids")]
    pub rules: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_bounded_bytes")]
    pub input_bytes: usize,
    #[serde(default, deserialize_with = "deserialize_bounded_bytes")]
    pub effective_bytes: usize,
    #[serde(default, deserialize_with = "deserialize_bounded_candidate_work")]
    pub candidate_work: usize,
    #[serde(default, deserialize_with = "deserialize_bounded_duration")]
    pub repair_duration_micros: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ToolArgumentRepairReason>,
}

fn deserialize_bounded_rule_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedRuleIdsVisitor;

    impl<'de> Visitor<'de> for BoundedRuleIdsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "at most {MAX_TOOL_ARGUMENT_REPAIR_RULES} bounded argument-repair rule IDs"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut rules = Vec::with_capacity(MAX_TOOL_ARGUMENT_REPAIR_RULES);
            while let Some(rule) = sequence.next_element::<String>()? {
                if rules.len() >= MAX_TOOL_ARGUMENT_REPAIR_RULES {
                    return Err(A::Error::invalid_length(rules.len() + 1, &self));
                }
                if rule.len() <= 64
                    && !rule.is_empty()
                    && rule.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                    && is_stable_tool_argument_repair_rule(&rule)
                {
                    rules.push(rule);
                }
            }
            Ok(rules)
        }
    }

    deserializer.deserialize_seq(BoundedRuleIdsVisitor)
}

fn deserialize_bounded_bytes<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(usize::deserialize(deserializer)?.min(MAX_TOOL_ARGUMENT_REPAIR_BYTES))
}

fn deserialize_bounded_candidate_work<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(usize::deserialize(deserializer)?.min(MAX_TOOL_ARGUMENT_REPAIR_CANDIDATE_WORK))
}

fn deserialize_bounded_duration<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u64::deserialize(deserializer)?.min(MAX_TOOL_ARGUMENT_REPAIR_DURATION_MICROS))
}

fn is_stable_tool_argument_repair_rule(rule: &str) -> bool {
    matches!(
        rule,
        "optional_null_removed"
            | "stringified_array_decoded"
            | "stringified_object_decoded"
            | "scalar_wrapped_in_array"
            | "numeric_string_typed"
            | "boolean_string_typed"
            | "known_field_alias"
            | "markdown_path_unwrapped"
    )
}

/// Metadata owned by the Codex harness and persisted with a response item.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct CodexHarnessMetadata {
    /// Whether a developer message was supplied by an app-server client.
    #[serde(default)]
    pub client_authored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_output_provenance: Option<ToolOutputProvenance>,
    /// Bounded, history-only references retained across compaction. These are never projected to
    /// the model; they let fork/resume copy a managed artifact even when its model-facing control
    /// was omitted by the compaction budget.
    #[serde(
        default,
        deserialize_with = "deserialize_bounded_artifact_references",
        skip_serializing_if = "Vec::is_empty"
    )]
    store_backed_artifact_references: Vec<StoreBackedArtifactReference>,
    /// Bounded effective-call metadata for tool argument repair. The raw response item remains
    /// unchanged; this receipt is the history-only record of what the handler observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_argument_repair: Option<ToolArgumentRepairReceipt>,
}

/// A validated-at-use, history-only reference to an existing managed output artifact.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct StoreBackedArtifactReference {
    pub artifact_id: String,
}

fn deserialize_bounded_artifact_references<'de, D>(
    deserializer: D,
) -> Result<Vec<StoreBackedArtifactReference>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Vec::<StoreBackedArtifactReference>::deserialize(deserializer)?
            .into_iter()
            .take(MAX_STORE_BACKED_ARTIFACT_REFERENCES)
            .collect(),
    )
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ToolOutputProvenance {
    StoreBackedArtifactV1,
}

impl CodexHarnessMetadata {
    /// Marks a developer message supplied by an app-server client.
    pub fn client_authored() -> Self {
        Self {
            client_authored: true,
            ..Default::default()
        }
    }

    /// Marks a bounded tool-output control document whose artifact was verified
    /// in the managed store by the harness.
    pub fn store_backed_tool_output() -> Self {
        Self {
            client_authored: false,
            tool_output_provenance: Some(ToolOutputProvenance::StoreBackedArtifactV1),
            store_backed_artifact_references: Vec::new(),
            tool_argument_repair: None,
        }
    }

    /// Adds a bounded effective-call receipt while preserving other harness metadata.
    pub fn with_tool_argument_repair(mut self, receipt: ToolArgumentRepairReceipt) -> Self {
        self.set_tool_argument_repair(receipt);
        self
    }

    /// Replaces the effective-call receipt on this metadata value.
    pub fn set_tool_argument_repair(&mut self, receipt: ToolArgumentRepairReceipt) {
        self.tool_argument_repair = Some(ToolArgumentRepairReceipt {
            rules: receipt
                .rules
                .into_iter()
                .filter(|rule| is_stable_tool_argument_repair_rule(rule))
                .take(MAX_TOOL_ARGUMENT_REPAIR_RULES)
                .collect(),
            input_bytes: receipt.input_bytes.min(MAX_TOOL_ARGUMENT_REPAIR_BYTES),
            effective_bytes: receipt.effective_bytes.min(MAX_TOOL_ARGUMENT_REPAIR_BYTES),
            candidate_work: receipt
                .candidate_work
                .min(MAX_TOOL_ARGUMENT_REPAIR_CANDIDATE_WORK),
            repair_duration_micros: receipt
                .repair_duration_micros
                .min(MAX_TOOL_ARGUMENT_REPAIR_DURATION_MICROS),
            ..receipt
        });
    }

    /// Returns the effective-call receipt, if one was persisted.
    pub fn tool_argument_repair(&self) -> Option<&ToolArgumentRepairReceipt> {
        self.tool_argument_repair.as_ref()
    }

    /// Returns whether this response item is a verified store-backed control document.
    pub fn is_store_backed_tool_output(&self) -> bool {
        matches!(
            self.tool_output_provenance,
            Some(ToolOutputProvenance::StoreBackedArtifactV1)
        )
    }

    /// Adds bounded history-only artifact references while preserving unrelated metadata.
    pub fn with_store_backed_artifact_references(
        mut self,
        artifact_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        self.set_store_backed_artifact_references(artifact_ids);
        self
    }

    /// Replaces the bounded history-only artifact references on this metadata value.
    pub fn set_store_backed_artifact_references(
        &mut self,
        artifact_ids: impl IntoIterator<Item = String>,
    ) {
        self.store_backed_artifact_references = artifact_ids
            .into_iter()
            .take(MAX_STORE_BACKED_ARTIFACT_REFERENCES)
            .map(|artifact_id| StoreBackedArtifactReference { artifact_id })
            .collect();
    }

    /// Returns history-only artifact references; callers must validate the IDs before use.
    pub fn store_backed_artifact_references(&self) -> &[StoreBackedArtifactReference] {
        &self.store_backed_artifact_references
    }
}

impl ResponseItemEnvelope {
    /// Wraps a raw Responses API item for persisted history.
    pub fn new(item: ResponseItem) -> Self {
        Self {
            item,
            metadata: None,
        }
    }

    /// Unwraps the raw Responses API item.
    pub fn into_item(self) -> ResponseItem {
        self.item
    }
}

impl From<ResponseItem> for ResponseItemEnvelope {
    fn from(item: ResponseItem) -> Self {
        Self::new(item)
    }
}

impl Deref for ResponseItemEnvelope {
    type Target = ResponseItem;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl DerefMut for ResponseItemEnvelope {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

impl Borrow<ResponseItem> for ResponseItemEnvelope {
    fn borrow(&self) -> &ResponseItem {
        &self.item
    }
}

/// Persisted rollout item used by core history and rollout storage.
#[derive(Debug, Clone)]
pub enum RolloutItem {
    SessionMeta(SessionMetaLine),
    ResponseItem(ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
    InterAgentCommunicationMetadata {
        trigger_turn: bool,
    },
    Compacted(CompactedItem),
    TurnContext(TurnContextItem),
    WorldState(WorldStateItem),
    SecurityRiskScore(SecurityRiskScore),
    EventMsg(EventMsg),
    /// Sparse, model-invisible facts used to reconstruct realtime presentation.
    RealtimeItem(RealtimeItem),
}

impl Serialize for RolloutItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        rollout_payload::RolloutItemWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RolloutItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        rollout_payload::RolloutItemWire::deserialize(deserializer).map(Into::into)
    }
}

impl JsonSchema for RolloutItem {
    fn schema_name() -> String {
        "RolloutItem".to_string()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::RolloutItem"))
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        rollout_payload::RolloutItemWire::json_schema(generator)
    }
}

mod rollout_payload;

#[derive(Clone, Debug, PartialEq)]
pub struct CompactedItem {
    pub message: String,
    pub replacement_history: Option<Vec<ResponseItemEnvelope>>,
    pub mcp_resource_origins: Option<McpResourceOriginCheckpoint>,
    pub window_number: Option<u64>,
    pub first_window_id: Option<String>,
    pub previous_window_id: Option<String>,
    pub window_id: Option<String>,
}

impl Serialize for CompactedItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        rollout_payload::CompactedItemWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompactedItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        rollout_payload::CompactedItemWire::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
}

impl JsonSchema for CompactedItem {
    fn schema_name() -> String {
        "CompactedItem".to_string()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::CompactedItem"))
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        rollout_payload::CompactedItemWire::json_schema(generator)
    }
}

impl From<CompactedItem> for ResponseItem {
    fn from(value: CompactedItem) -> Self {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: value.message,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, JsonSchema)]
pub struct RolloutLine {
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(flatten)]
    pub item: RolloutItem,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResumedHistory {
    pub conversation_id: ThreadId,
    pub history: Arc<Vec<RolloutItem>>,
    pub rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum InitialHistory {
    New,
    Cleared,
    Resumed(ResumedHistory),
    Forked(Vec<RolloutItem>),
}

impl InitialHistory {
    pub fn scan_rollout_items(&self, mut predicate: impl FnMut(&RolloutItem) -> bool) -> bool {
        match self {
            Self::New | Self::Cleared => false,
            Self::Resumed(resumed) => resumed.history.iter().any(&mut predicate),
            Self::Forked(items) => items.iter().any(predicate),
        }
    }

    pub fn forked_from_id(&self) -> Option<ThreadId> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.forked_from_id,
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id),
                _ => None,
            }),
        }
    }

    pub fn session_cwd(&self) -> Option<PathBuf> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => session_cwd_from_items(&resumed.history),
            Self::Forked(items) => session_cwd_from_items(items),
        }
    }

    pub fn get_rollout_items(&self) -> &[RolloutItem] {
        match self {
            Self::New | Self::Cleared => &[],
            Self::Resumed(resumed) => &resumed.history,
            Self::Forked(items) => items,
        }
    }

    pub fn get_event_msgs(&self) -> Option<Vec<EventMsg>> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => Some(
                resumed
                    .history
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(event) => Some(event.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            Self::Forked(items) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(event) => Some(event.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
        }
    }

    pub fn get_base_instructions(&self) -> Option<BaseInstructions> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_dynamic_tools(&self) -> Option<Vec<DynamicToolSpec>> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_selected_capability_roots(&self) -> Vec<SelectedCapabilityRoot> {
        self.get_session_meta()
            .map(|meta| meta.selected_capability_roots.clone())
            .unwrap_or_default()
    }

    pub fn get_multi_agent_version(&self) -> Option<MultiAgentVersion> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => {
                multi_agent_version_from_items(&resumed.history, Some(resumed.conversation_id))
            }
            Self::Forked(items) => multi_agent_version_from_items(items, /*thread_id*/ None),
        }
    }

    pub fn get_history_mode(&self, default_history_mode: ThreadHistoryMode) -> ThreadHistoryMode {
        match self {
            Self::New | Self::Cleared => default_history_mode,
            Self::Resumed(_) | Self::Forked(_) => self
                .get_session_meta()
                .map(|meta| meta.history_mode)
                .unwrap_or(default_history_mode),
        }
    }

    pub fn get_resumed_session_sources(&self) -> Option<(SessionSource, Option<ThreadSource>)> {
        let meta = self.get_resumed_session_meta()?;
        Some((meta.source.clone(), meta.thread_source.clone()))
    }

    pub fn get_resumed_thread_source(&self) -> Option<ThreadSource> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.thread_source.clone())
    }

    pub fn get_session_originator(&self) -> Option<String> {
        self.get_session_meta()
            .map(|meta| meta.originator.clone())
            .filter(|originator| !originator.is_empty())
    }

    pub fn get_resumed_parent_thread_id(&self) -> Option<ThreadId> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.parent_thread_id)
    }

    fn get_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
        }
    }

    fn get_resumed_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            Self::New | Self::Cleared | Self::Forked(_) => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
        }
    }
}

fn session_cwd_from_items(items: &[RolloutItem]) -> Option<PathBuf> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.cwd.clone()),
        _ => None,
    })
}

fn multi_agent_version_from_items(
    items: &[RolloutItem],
    thread_id: Option<ThreadId>,
) -> Option<MultiAgentVersion> {
    let session_meta_version = items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line)
            if thread_id.is_none_or(|thread_id| meta_line.meta.id == thread_id) =>
        {
            meta_line.meta.multi_agent_version
        }
        _ => None,
    });

    session_meta_version.or_else(|| {
        items.iter().rev().find_map(|item| match item {
            RolloutItem::TurnContext(turn_context) => turn_context.multi_agent_version,
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::EventMsg(_) => None,
        })
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
