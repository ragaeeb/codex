use super::ToolOutputProjector;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::protocol::EventMsg;
use codex_utils_string::take_bytes_at_char_boundary;
use serde_json::Value;
use serde_json::json;

const DISPLAY_TRUNCATION_MARKER_BUDGET: usize = 64;

impl ToolOutputProjector {
    pub(crate) async fn project_event_msg(&self, message: &EventMsg) -> EventMsg {
        let mut message = message.clone();
        match &mut message {
            EventMsg::ItemCompleted(event) => match &mut event.item {
                TurnItem::CommandExecution(item) => {
                    self.project_strings(
                        [
                            &mut item.stdout,
                            &mut item.stderr,
                            &mut item.aggregated_output,
                        ]
                        .into_iter()
                        .filter_map(Option::as_mut)
                        .collect(),
                    )
                    .await;
                    if let Some(formatted) = &mut item.formatted_output {
                        self.truncate_display_only(formatted);
                    }
                }
                TurnItem::FileChange(item) => {
                    self.project_strings(
                        [&mut item.stdout, &mut item.stderr]
                            .into_iter()
                            .filter_map(Option::as_mut)
                            .collect(),
                    )
                    .await
                }
                TurnItem::DynamicToolCall(item) => {
                    if let Some(items) = &mut item.content_items {
                        self.project_strings(
                            items
                                .iter_mut()
                                .filter_map(|item| match item {
                                    DynamicToolCallOutputContentItem::InputText { text } => {
                                        Some(text)
                                    }
                                    DynamicToolCallOutputContentItem::InputImage { .. }
                                    | DynamicToolCallOutputContentItem::InputAudio { .. } => None,
                                })
                                .collect(),
                        )
                        .await;
                    }
                    self.project_strings(item.error.iter_mut().collect()).await;
                }
                TurnItem::McpToolCall(item) => {
                    if let Some(result) = &mut item.result {
                        self.project_mcp(result).await;
                    }
                }
                TurnItem::WebSearch(item) => self.project_json(&mut item.results).await,
                _ => {}
            },
            EventMsg::McpToolCallEnd(event) => {
                if let Ok(result) = &mut event.result {
                    self.project_mcp(result).await;
                }
            }
            EventMsg::ExecCommandEnd(event) => {
                self.project_strings(vec![
                    &mut event.stdout,
                    &mut event.stderr,
                    &mut event.aggregated_output,
                ])
                .await;
                self.truncate_display_only(&mut event.formatted_output);
            }
            EventMsg::PatchApplyEnd(event) => {
                self.project_strings(vec![&mut event.stdout, &mut event.stderr])
                    .await
            }
            EventMsg::WebSearchEnd(event) => self.project_json(&mut event.results).await,
            _ => {}
        }
        message
    }

    pub(super) async fn project_strings(&self, values: Vec<&mut String>) {
        for value in values {
            self.truncate_display_only(value);
        }
    }

    fn truncate_display_only(&self, value: &mut String) {
        if value.len() <= self.policy.byte_budget() {
            return;
        }
        *value = codex_utils_output_truncation::formatted_truncate_text(value, self.policy);
        if value.len() > self.policy.byte_budget() {
            *value = codex_utils_output_truncation::truncate_text(
                value,
                codex_utils_output_truncation::TruncationPolicy::Bytes(
                    self.policy
                        .byte_budget()
                        .saturating_sub(DISPLAY_TRUNCATION_MARKER_BUDGET),
                ),
            );
        }
        if value.len() > self.policy.byte_budget() {
            *value = take_bytes_at_char_boundary(value, self.policy.byte_budget()).to_string();
        }
    }

    async fn project_mcp(&self, result: &mut CallToolResult) {
        let mut text = result
            .content
            .iter()
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            let original = text.clone();
            self.project_strings(vec![&mut text]).await;
            if text != original {
                result
                    .content
                    .retain(|content| content.get("text").is_none());
                result
                    .content
                    .insert(0, json!({"type": "text", "text": text}));
            }
        }
        if let Some(value) = &mut result.structured_content {
            let mut text = value.to_string();
            let original = text.clone();
            self.project_strings(vec![&mut text]).await;
            if text != original {
                *value = Value::String(text);
            }
        }
    }

    async fn project_json(&self, values: &mut Option<Vec<Value>>) {
        if let Some(values) = values {
            let original = serde_json::to_string(values).unwrap_or_default();
            let mut projected = original.clone();
            self.project_strings(vec![&mut projected]).await;
            if projected != original {
                *values = vec![Value::String(projected)];
            }
        }
    }
}
