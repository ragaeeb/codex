use super::*;

impl ToolOutputProjector {
    pub(super) async fn project_content_items(
        &self,
        payload: &FunctionCallOutputPayload,
        family: &'static str,
    ) -> (FunctionCallOutputPayload, ProjectionMeasurement, bool) {
        let mut output = payload.clone();
        let original_bytes = match &output.body {
            FunctionCallOutputBody::ContentItems(items) => {
                items.iter().filter_map(text_item).map(String::len).sum()
            }
            FunctionCallOutputBody::Text(_) => unreachable!(),
        };
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
        let FunctionCallOutputBody::ContentItems(items) = &mut output.body else {
            unreachable!()
        };
        let large = items
            .iter()
            .filter_map(text_item)
            .filter(|text| text.len() > self.policy.byte_budget())
            .count();
        let small_bytes = items
            .iter()
            .filter_map(text_item)
            .filter(|text| text.len() <= self.policy.byte_budget())
            .map(String::len)
            .sum::<usize>();
        let envelope_budget = self.policy.byte_budget().saturating_sub(small_bytes) / large.max(1);
        if !(1..=4).contains(&large) || envelope_budget < MIN_ARTIFACT_ENVELOPE_BYTES {
            let text_items = items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    text_item(item).map(|text| json!({"index": index, "text": text}))
                })
                .collect::<Vec<_>>();
            let canonical = json!({
                "type": "tool_output_text_items",
                "version": 1,
                "items": text_items,
            })
            .to_string();
            return match self.store.store_text(&canonical).await {
                Ok(artifact) => {
                    let rule = if artifact.reused {
                        "exact_digest_reuse_v1"
                    } else {
                        "spill_v1"
                    };
                    let envelope = artifact_envelope(
                        &artifact,
                        &canonical,
                        self.policy.byte_budget().clamp(
                            MIN_ARTIFACT_ENVELOPE_BYTES,
                            STORE_BACKED_TOOL_OUTPUT_MAX_BYTES,
                        ),
                    );
                    let output = replace_text(payload, envelope);
                    let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
                    (
                        output,
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
            };
        }
        let mut reused = true;
        for text in items.iter_mut().filter_map(text_item_mut) {
            if text.len() <= self.policy.byte_budget() {
                continue;
            }
            match self.store.store_text(text).await {
                Ok(artifact) => {
                    reused &= artifact.reused;
                    *text = artifact_envelope(&artifact, text, envelope_budget);
                }
                Err(err) => {
                    warn!(error_kind = ?err.kind(), "tool output spill failed; using bounded truncation");
                    let output = truncate_function_output_payload(payload, self.policy);
                    let inline_bytes = output.body.to_text().map_or(0, |text| text.len());
                    return (
                        output,
                        measurement(
                            family,
                            "spill_failure_truncate_v1",
                            original_bytes,
                            inline_bytes,
                            "fallback",
                        ),
                        false,
                    );
                }
            }
        }
        let inline_bytes = items.iter().filter_map(text_item).map(String::len).sum();
        let rule = if reused {
            "exact_digest_reuse_v1"
        } else {
            "spill_v1"
        };
        (
            output,
            measurement(family, rule, original_bytes, inline_bytes, "spilled"),
            true,
        )
    }
}
