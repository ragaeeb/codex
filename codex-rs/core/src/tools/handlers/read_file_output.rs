use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputProvenance;
use codex_tools::ToolPayload;
use codex_utils_output_truncation::OutputArtifactId;
use serde_json::Value;

pub(crate) const FILE_READ_CONTENT_TYPE: &str = "application/vnd.codex.file-read+json";

pub(crate) struct ReadFileToolOutput {
    response: FunctionToolOutput,
    structured: Value,
    rule: &'static str,
}

impl ReadFileToolOutput {
    pub(crate) fn new(
        text: String,
        managed_artifact: bool,
        rule: &'static str,
    ) -> Result<Self, FunctionCallError> {
        let structured = serde_json::from_str(&text).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "read_file produced an invalid structured result: {error}"
            ))
        })?;
        let response = if managed_artifact {
            FunctionToolOutput::from_managed_artifact_reference_text(text)
        } else {
            FunctionToolOutput::from_text(text, Some(true))
        };
        Ok(Self {
            response,
            structured,
            rule,
        })
    }
}

impl ToolOutput for ReadFileToolOutput {
    fn log_output(&self) -> String {
        let outcome = match self.provenance() {
            ToolOutputProvenance::ManagedArtifactRetrieval
            | ToolOutputProvenance::ManagedArtifactReference => "artifact",
            ToolOutputProvenance::Untrusted => "inline",
        };
        serde_json::json!({
            "tool_family": "read_file",
            "rule": self.rule,
            "outcome": outcome,
            "serialized_bytes": self.response_size(),
            "window_bytes": self.window_bytes(),
            "line_fragments": self.structured["window"]["line_fragments"],
        })
        .to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.response.success_for_logging()
    }

    fn provenance(&self) -> ToolOutputProvenance {
        self.response.provenance()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.response.to_response_item(call_id, payload)
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<Value> {
        Some(self.structured.clone())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.structured.clone()
    }
}

impl ReadFileToolOutput {
    fn response_size(&self) -> usize {
        self.response
            .body
            .iter()
            .map(|item| match item {
                codex_protocol::models::FunctionCallOutputContentItem::InputText { text } => {
                    text.len()
                }
                _ => 0,
            })
            .sum()
    }

    fn window_bytes(&self) -> u64 {
        self.structured["window"]["end_byte"]
            .as_u64()
            .unwrap_or_default()
            .saturating_sub(
                self.structured["window"]["start_byte"]
                    .as_u64()
                    .unwrap_or_default(),
            )
    }
}

pub(crate) fn inline_result_digest(text: &str) -> String {
    OutputArtifactId::for_text(text).as_str().to_string()
}

#[cfg(test)]
#[path = "read_file_output_tests.rs"]
mod tests;
