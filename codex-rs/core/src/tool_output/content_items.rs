use super::*;
use codex_protocol::models::ImageDetail;

const MAX_MODEL_VISIBLE_CONTENT_ITEMS: usize = 64;
const MEDIA_OMITTED_MARKER: &str = "[omitted media content to stay within the model budget]";

impl ToolOutputProjector {
    pub(super) async fn project_content_items(
        &self,
        payload: &FunctionCallOutputPayload,
        family: &'static str,
    ) -> (FunctionCallOutputPayload, ProjectionMeasurement, bool) {
        let original_bytes = serialized_body_bytes(payload);
        let text_bytes = match &payload.body {
            FunctionCallOutputBody::ContentItems(items) => items
                .iter()
                .filter_map(text_item)
                .map(String::len)
                .sum::<usize>(),
            FunctionCallOutputBody::Text(_) => unreachable!(),
        };
        let model_limit = self.policy.byte_budget().min(MAX_CONTENT_ITEMS_MODEL_BYTES);
        let model_visible_bytes = estimate_content_items_model_visible_bytes(payload);
        let serialized_exceeds_policy =
            serialized_body_bytes(payload) > MAX_CONTENT_ITEMS_SERIALIZED_BYTES;
        let model_exceeds_policy = serialized_exceeds_policy
            || model_visible_bytes > model_limit
            || match &payload.body {
                FunctionCallOutputBody::ContentItems(items) => {
                    items.len() > MAX_MODEL_VISIBLE_CONTENT_ITEMS
                }
                FunctionCallOutputBody::Text(_) => false,
            };
        let has_media = match &payload.body {
            FunctionCallOutputBody::ContentItems(items) => items.iter().any(|item| {
                matches!(
                    item,
                    FunctionCallOutputContentItem::InputImage { .. }
                        | FunctionCallOutputContentItem::InputAudio { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. }
                )
            }),
            FunctionCallOutputBody::Text(_) => false,
        };
        // A policy below the recovery-control floor must not replace an already-fitting text-only
        // body merely because the surrounding response-item estimate includes wrapper overhead.
        let text_only_body_fits_policy = self.policy.byte_budget() < MIN_ARTIFACT_ENVELOPE_BYTES
            && !has_media
            && original_bytes <= model_limit
            && match &payload.body {
                FunctionCallOutputBody::ContentItems(items) => {
                    items.len() <= MAX_MODEL_VISIBLE_CONTENT_ITEMS
                }
                FunctionCallOutputBody::Text(_) => false,
            };
        // The hard model-item ceiling is also a recoverability boundary for text. A text-only
        // body that fits the configured policy but exceeds the independent ceiling must take the
        // same store-backed path as any other oversized text, rather than becoming an
        // unrecoverable bounded error.
        let text_exceeds_policy =
            text_bytes > self.policy.byte_budget() || text_bytes > MAX_MANAGED_ARTIFACT_MODEL_BYTES;
        if !has_media
            && !text_exceeds_policy
            && (text_only_body_fits_policy || !model_exceeds_policy)
        {
            return (
                payload.clone(),
                measurement(
                    family,
                    "inline_v1",
                    original_bytes,
                    serialized_body_bytes(payload),
                    "inline",
                ),
                false,
            );
        }
        if has_media && !text_exceeds_policy && !model_exceeds_policy {
            // Media payloads have their own modality/token accounting. Do not route an
            // image/audio-only response through the string-only fallback: that would turn a
            // legitimate view_image/audio result into a misleading text error merely because
            // the serialized data URL is larger than the text byte budget.
            return (
                payload.clone(),
                measurement(
                    family,
                    "media_policy_preserved_v1",
                    original_bytes,
                    serialized_body_bytes(payload),
                    "inline",
                ),
                false,
            );
        }
        if has_media && !text_exceeds_policy && model_exceeds_policy {
            let output = fit_media_to_model_budget(payload, model_limit);
            let inline_bytes = serialized_body_bytes(&output);
            return (
                output,
                measurement(
                    family,
                    "media_aggregate_bound_v1",
                    original_bytes,
                    inline_bytes,
                    "inline",
                ),
                false,
            );
        }
        if !self.spilling_supported {
            let fallback_policy =
                TruncationPolicy::Bytes(self.policy.byte_budget().min(model_limit));
            let output = if text_exceeds_policy {
                truncate_function_output_payload(payload, fallback_policy)
            } else if model_exceeds_policy && !has_media {
                fit_media_to_model_budget(payload, model_limit)
            } else {
                payload.clone()
            };
            let output = if has_media {
                fit_media_to_model_budget(&output, model_limit)
            } else {
                output
            };
            let inline_bytes = serialized_body_bytes(&output);
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
        let artifact_budget = self
            .policy
            .byte_budget()
            .saturating_sub(MODEL_ITEM_CONTROL_RESERVATION_BYTES);
        if artifact_budget < MIN_ARTIFACT_ENVELOPE_BYTES {
            let output = bounded_output_payload(payload, self.policy.byte_budget());
            let inline_bytes = serialized_body_bytes(&output);
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
        let FunctionCallOutputBody::ContentItems(items) = &payload.body else {
            unreachable!()
        };
        if !text_exceeds_policy {
            if model_exceeds_policy && !has_media {
                let output = fit_media_to_model_budget(payload, model_limit);
                let inline_bytes = serialized_body_bytes(&output);
                return (
                    output,
                    measurement(
                        family,
                        "model_aggregate_bound_v1",
                        original_bytes,
                        inline_bytes,
                        "fallback",
                    ),
                    false,
                );
            }
            let output = fit_media_to_model_budget(payload, model_limit);
            let inline_bytes = serialized_body_bytes(&output);
            return (
                output,
                measurement(
                    family,
                    "aggregate_policy_fallback_v1",
                    original_bytes,
                    inline_bytes,
                    "fallback",
                ),
                false,
            );
        }
        // A content-item output has one provenance sidecar, so all text siblings must share one
        // canonical artifact. Leaving a small sibling beside a managed envelope would let the
        // sidecar accidentally bless untrusted text and would make the aggregate item cap
        // unenforceable during history projection.
        let canonical = json!({
            "type": "tool_output_text_items",
            "version": 1,
            "items": items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    text_item(item).map(|text| json!({"index": index, "text": text}))
                })
                .collect::<Vec<_>>(),
        })
        .to_string();
        match self.store.store_text(&canonical).await {
            Ok(artifact) => {
                let rule = if artifact.reused {
                    "exact_digest_reuse_v1"
                } else {
                    "spill_v1"
                };
                let Some((output, _envelope)) = fit_content_items_output(
                    payload,
                    &artifact,
                    &canonical,
                    artifact_budget.min(STORE_BACKED_TOOL_OUTPUT_MAX_BYTES),
                    model_limit,
                ) else {
                    let output = bounded_output_payload(payload, self.policy.byte_budget());
                    let inline_bytes = serialized_body_bytes(&output);
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
                let inline_bytes = serialized_body_bytes(&output);
                (
                    output,
                    measurement(family, rule, original_bytes, inline_bytes, "spilled"),
                    true,
                )
            }
            Err(err) => {
                warn!(error_kind = ?err.kind(), "tool output spill failed; using bounded truncation");
                let output = bounded_output_payload(payload, self.policy.byte_budget());
                let inline_bytes = serialized_body_bytes(&output);
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

fn serialized_body_bytes(payload: &FunctionCallOutputPayload) -> usize {
    serde_json::to_string(&payload.body)
        .map(|body| body.len())
        .unwrap_or(usize::MAX)
}

fn fit_content_items_output(
    payload: &FunctionCallOutputPayload,
    artifact: &codex_utils_output_truncation::StoredOutputArtifact,
    canonical: &str,
    max_bytes: usize,
    model_limit: usize,
) -> Option<(FunctionCallOutputPayload, String)> {
    let mut low = 0usize;
    let mut high = max_bytes;
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let Some(envelope) = artifact_envelope(artifact, canonical, middle) else {
            low = middle.saturating_add(1);
            continue;
        };
        let Some(envelope) = compact_store_backed_envelope(
            &envelope,
            MAX_MANAGED_ARTIFACT_MODEL_BYTES.min(model_limit),
        ) else {
            low = middle.saturating_add(1);
            continue;
        };
        let fitted =
            fit_media_to_model_budget(&replace_text(payload, envelope.clone()), model_limit);
        let retains_media_or_marker = fitted.content_items().is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item,
                    FunctionCallOutputContentItem::InputImage { .. }
                        | FunctionCallOutputContentItem::InputAudio { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. }
                ) || matches!(
                    item,
                    FunctionCallOutputContentItem::InputText { text }
                        if text == MEDIA_OMITTED_MARKER
                )
            })
        });
        let mixed = if retains_media_or_marker {
            fitted
        } else {
            let mut text_only = fitted;
            text_only.body = FunctionCallOutputBody::Text(envelope.clone());
            text_only
        };
        let retains_envelope = match &mixed.body {
            FunctionCallOutputBody::Text(text) => text == &envelope,
            FunctionCallOutputBody::ContentItems(items) => items.iter().any(|item| {
                matches!(
                    item,
                    FunctionCallOutputContentItem::InputText { text } if text == &envelope
                )
            }),
        };
        let candidate = (retains_envelope
            && (serde_json::to_string(&mixed.body).is_ok_and(|body| body.len() <= max_bytes)
                || estimate_content_items_model_visible_bytes(&mixed) <= model_limit))
            .then_some(mixed);
        if let Some(candidate) = candidate {
            best = Some((candidate, envelope));
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

fn fit_media_to_model_budget(
    payload: &FunctionCallOutputPayload,
    model_limit: usize,
) -> FunctionCallOutputPayload {
    let FunctionCallOutputBody::ContentItems(items) = &payload.body else {
        return payload.clone();
    };
    let mut retained = Vec::with_capacity(items.len().min(MAX_MODEL_VISIBLE_CONTENT_ITEMS));
    let mut omitted = false;
    for item in items {
        if retained.len() >= MAX_MODEL_VISIBLE_CONTENT_ITEMS {
            omitted = true;
            continue;
        }
        let mut candidate = retained.clone();
        candidate.push(item.clone());
        let candidate_payload = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(candidate),
            success: payload.success,
        };
        if estimate_content_items_model_visible_bytes(&candidate_payload) <= model_limit
            && serialized_body_bytes(&candidate_payload) <= MAX_CONTENT_ITEMS_SERIALIZED_BYTES
        {
            retained.push(item.clone());
        } else {
            omitted = true;
        }
    }
    let has_text = retained
        .iter()
        .any(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }));
    if omitted && has_text {
        if retained.len() >= MAX_MODEL_VISIBLE_CONTENT_ITEMS
            && let Some(index) = retained
                .iter()
                .rposition(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }))
                .or_else(|| retained.len().checked_sub(1))
        {
            retained.remove(index);
        }
        if retained.len() < MAX_MODEL_VISIBLE_CONTENT_ITEMS {
            let marker = FunctionCallOutputContentItem::InputText {
                text: MEDIA_OMITTED_MARKER.to_string(),
            };
            let mut candidate = retained.clone();
            candidate.push(marker.clone());
            let candidate_payload = FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(candidate),
                success: payload.success,
            };
            if estimate_content_items_model_visible_bytes(&candidate_payload) <= model_limit
                && serialized_body_bytes(&candidate_payload) <= MAX_CONTENT_ITEMS_SERIALIZED_BYTES
            {
                retained.push(marker);
            }
        }
    } else if omitted {
        if retained.len() >= MAX_MODEL_VISIBLE_CONTENT_ITEMS
            && let Some(index) = retained
                .iter()
                .rposition(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }))
                .or_else(|| retained.len().checked_sub(1))
        {
            retained.remove(index);
        }
        if retained.len() < MAX_MODEL_VISIBLE_CONTENT_ITEMS {
            let marker = FunctionCallOutputContentItem::InputText {
                text: MEDIA_OMITTED_MARKER.to_string(),
            };
            let mut candidate = retained.clone();
            candidate.push(marker.clone());
            let candidate_payload = FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(candidate),
                success: payload.success,
            };
            if estimate_content_items_model_visible_bytes(&candidate_payload) <= model_limit
                && serialized_body_bytes(&candidate_payload) <= MAX_CONTENT_ITEMS_SERIALIZED_BYTES
            {
                retained.push(marker);
            }
        }
    }
    if retained.is_empty() && !items.is_empty() {
        return FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(bounded_structured_error(model_limit)),
            success: Some(false),
        };
    }
    FunctionCallOutputPayload {
        body: FunctionCallOutputBody::ContentItems(retained),
        success: payload.success,
    }
}

fn estimate_content_items_model_visible_bytes(payload: &FunctionCallOutputPayload) -> usize {
    let raw = serde_json::to_string(&payload.body)
        .map(|body| body.len())
        .unwrap_or(usize::MAX);
    let FunctionCallOutputBody::ContentItems(items) = &payload.body else {
        return raw.saturating_add(256);
    };
    let mut raw_media_bytes = 0usize;
    let mut estimated_media_bytes = 0usize;
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                if let Some(payload) = data_url_payload(image_url, "image/") {
                    raw_media_bytes = raw_media_bytes.saturating_add(payload.len());
                }
                // A short remote URL can still produce a large image input after the model
                // fetches it, so apply the modality estimate to every image, not only data URLs.
                estimated_media_bytes = estimated_media_bytes.saturating_add(
                    if matches!(detail, Some(ImageDetail::Original)) {
                        40_000
                    } else {
                        7_373
                    },
                );
            }
            FunctionCallOutputContentItem::InputAudio { audio_url } => {
                if let Some(payload) = data_url_payload(audio_url, "audio/") {
                    raw_media_bytes = raw_media_bytes.saturating_add(payload.len());
                }
                estimated_media_bytes = estimated_media_bytes.saturating_add(
                    codex_utils_output_truncation::approx_bytes_for_tokens(
                        codex_utils_audio::estimate_audio_token_count_uncached(audio_url),
                    ),
                );
            }
            FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                raw_media_bytes = raw_media_bytes.saturating_add(encrypted_content.len());
                estimated_media_bytes = estimated_media_bytes
                    .saturating_add(encrypted_content.len().saturating_mul(9).div_ceil(16));
            }
            FunctionCallOutputContentItem::InputText { .. } => {}
        }
    }
    raw.saturating_sub(raw_media_bytes)
        .saturating_add(estimated_media_bytes)
        .saturating_add(256)
}

fn data_url_payload<'a>(url: &'a str, media_prefix: &str) -> Option<&'a str> {
    let (metadata, payload) = url.split_once(',')?;
    let metadata = metadata.strip_prefix("data:")?;
    let mut parts = metadata.split(';');
    let mime = parts.next()?;
    if !mime
        .get(..media_prefix.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(media_prefix))
        || !parts.any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    Some(payload)
}
