use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_utils_output_truncation::MAX_ARTIFACT_READ_BYTES;
use codex_utils_output_truncation::OutputArtifactId;
use codex_utils_output_truncation::OutputArtifactStore;
use serde_json::Value;
use serde_json::json;

pub(super) const DEFAULT_BYTES: usize = 16 * 1024;
pub(super) const MAX_LINES: usize = 2_000;
pub(super) const MAX_MATCHES: usize = 50;
pub(super) const MAX_SEARCH_SCAN_BYTES: usize = 512 * 1024;

pub(super) fn bounded_result(mut value: serde_json::Value, max_bytes: usize) -> String {
    loop {
        let rendered = value.to_string();
        if rendered.len() <= max_bytes {
            return rendered;
        }
        let line_mode = value.get("mode").and_then(Value::as_str) == Some("lines");
        let original_start_byte = value.get("start_byte").and_then(Value::as_u64);
        let empty_line_scan_continuation = line_mode
            && value
                .get("scan_continues")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && value.get("text").and_then(Value::as_str) == Some("");
        if empty_line_scan_continuation
            && max_bytes < 256
            && let Some(fields) = value.as_object_mut()
        {
            // A scan continuation has no text to trim. Keep the byte cursor and remaining-line
            // state, and drop display-only ranges so the smallest producer-supported policy can
            // still carry an advancing, resumable JSON object.
            fields.remove("mode");
            fields.remove("complete");
            fields.remove("scan_continues");
            fields.remove("start_byte");
            fields.remove("end_byte");
        }
        if max_bytes < 256
            && let Some(fields) = value.as_object_mut()
        {
            // Keep the artifact identity, byte range, text, and continuation first. These
            // descriptive fields are optional and otherwise make the producer-supported 200-byte
            // policy unable to return one advancing scalar.
            if !line_mode {
                fields.remove("mode");
            }
            if line_mode {
                fields.remove("start_byte");
                fields.remove("end_byte");
            }
            fields.remove("complete");
            fields.remove("scan_continues");
        }
        let compacted_len = value.to_string().len();
        if compacted_len <= max_bytes {
            return value.to_string();
        }
        let excess = compacted_len.saturating_sub(max_bytes);
        if let Some(text) = value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if text.is_empty() {
                if let Some(fields) = value.as_object_mut() {
                    fields.remove("text");
                }
                continue;
            }
            let target = text.len().saturating_sub(excess.saturating_add(8));
            let target = (target..=text.len())
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(text.len());
            if target == 0 || target == text.len() {
                return crate::tool_output::bounded_structured_error(max_bytes);
            }
            value["text"] = text[..target].to_string().into();
            if let Some(start_byte) = original_start_byte {
                let has_next = value
                    .get("next_offset")
                    .is_some_and(|next_offset| !next_offset.is_null());
                let end_byte = start_byte.saturating_add(target as u64);
                value["end_byte"] = end_byte.into();
                value["complete"] = false.into();
                if value.get("mode").and_then(Value::as_str) == Some("lines") {
                    let completed_lines =
                        text[..target].bytes().filter(|byte| *byte == b'\n').count();
                    let start_line = value.get("start_line").and_then(Value::as_u64).unwrap_or(1);
                    value["next_offset"] = start_line.saturating_add(completed_lines as u64).into();
                    value["next_byte"] = end_byte.into();
                    value["line_continuation"] =
                        (has_next && !text[..target].ends_with('\n')).into();
                } else {
                    value["next_offset"] = end_byte.into();
                }
            }
            continue;
        }
        if let Some(encoded) = value
            .get("bytes_base64")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if encoded.is_empty() {
                if let Some(fields) = value.as_object_mut() {
                    fields.remove("bytes_base64");
                }
                continue;
            }
            let target = encoded
                .len()
                .saturating_sub(excess.saturating_add(8))
                .checked_div(4)
                .unwrap_or(0)
                .saturating_mul(4)
                .min(encoded.len());
            if target == 0 {
                return crate::tool_output::bounded_structured_error(max_bytes);
            }
            let decoded_len = BASE64_STANDARD
                .decode(&encoded[..target])
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            if decoded_len == 0 {
                return crate::tool_output::bounded_structured_error(max_bytes);
            }
            value["bytes_base64"] = encoded[..target].to_string().into();
            if let Some(start_byte) = value.get("start_byte").and_then(Value::as_u64) {
                let end_byte = start_byte.saturating_add(decoded_len as u64);
                value["end_byte"] = end_byte.into();
                value["complete"] = false.into();
                value["next_offset"] = end_byte.into();
            }
            continue;
        }
        if let Some(offsets) = value.get_mut("byte_offsets").and_then(Value::as_array_mut)
            && !offsets.is_empty()
        {
            if let Some(next_offset) = offsets.pop() {
                value["next_offset"] = next_offset;
            }
            value["complete"] = false.into();
            continue;
        }
        return crate::tool_output::bounded_structured_error(max_bytes);
    }
}

pub(super) async fn read_bytes_value(
    store: &OutputArtifactStore,
    id: &OutputArtifactId,
    offset: u64,
    limit: usize,
) -> std::io::Result<Value> {
    match store.read_bytes(id, offset, limit).await {
        Ok((text, start, end, next)) => Ok(json!({
            "type": "tool_output_artifact_window",
            "mode": "bytes",
            "artifact_id": id.as_str(),
            "start_byte": start,
            "end_byte": end,
            "text": text,
            "next_offset": next,
            "complete": next.is_none(),
        })),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput
            ) =>
        {
            let (bytes, start, end, next) = store.read_raw_bytes(id, offset, limit).await?;
            Ok(json!({
                "type": "tool_output_artifact_window",
                "mode": "bytes",
                "artifact_id": id.as_str(),
                "encoding": "base64",
                "start_byte": start,
                "end_byte": end,
                "bytes_base64": BASE64_STANDARD.encode(bytes),
                "next_offset": next,
                "complete": next.is_none(),
            }))
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn read_lines(
    store: &OutputArtifactStore,
    id: &OutputArtifactId,
    start_line: usize,
    byte_offset: u64,
    scan_continuation: bool,
    line_continuation: bool,
    scan_lines_remaining: Option<usize>,
    count: usize,
) -> std::io::Result<(
    String,
    u64,
    u64,
    Option<usize>,
    Option<u64>,
    bool,
    bool,
    Option<usize>,
)> {
    if start_line == 0 || count == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "line bounds must be positive",
        ));
    }
    let count = count.min(MAX_LINES);
    let mut byte = byte_offset;
    let mut lines_to_skip = if line_continuation {
        0
    } else if let Some(remaining) = scan_lines_remaining {
        remaining
    } else if scan_continuation {
        1
    } else if byte_offset == 0 {
        start_line - 1
    } else {
        0
    };
    let mut scanned_bytes = 0usize;
    while lines_to_skip > 0 {
        if scanned_bytes >= MAX_SEARCH_SCAN_BYTES {
            // The next request supplies both the absolute target line and this byte cursor, so
            // it can resume without rescanning from byte zero.
            return Ok((
                String::new(),
                byte,
                byte,
                Some(start_line),
                Some(byte),
                true,
                false,
                Some(lines_to_skip),
            ));
        }
        let batch = lines_to_skip.min(MAX_LINES);
        let (offsets, next) = search(store, id, "\n", byte, batch, MAX_LINES).await?;
        let progress = next
            .map(|next| next.saturating_sub(byte))
            .and_then(|progress| usize::try_from(progress).ok())
            .unwrap_or(MAX_SEARCH_SCAN_BYTES);
        scanned_bytes = scanned_bytes.saturating_add(progress);
        if offsets.len() < batch {
            if let Some(next) = next {
                lines_to_skip -= offsets.len();
                byte = next;
                continue;
            }
            return Ok((String::new(), byte, byte, None, None, false, false, None));
        }
        byte = offsets[offsets.len() - 1].saturating_add(1);
        lines_to_skip -= batch;
    }
    let (breaks, _) = search(store, id, "\n", byte, count, MAX_LINES).await?;
    let line_end = (breaks.len() == count).then(|| breaks[count - 1].saturating_add(1));
    let max_bytes = line_end.map_or(DEFAULT_BYTES, |end| {
        usize::try_from(end - byte)
            .unwrap_or(usize::MAX)
            .min(DEFAULT_BYTES)
    });
    let (text, _, end, next_byte) = store.read_bytes(id, byte, max_bytes).await?;
    let completed_lines = text.bytes().filter(|byte| *byte == b'\n').count();
    let next_line = next_byte.map(|_| start_line.saturating_add(completed_lines));
    let line_continuation = next_byte.is_some() && !text.ends_with('\n');
    Ok((
        text,
        byte,
        end,
        next_line,
        next_byte,
        false,
        line_continuation,
        None,
    ))
}

pub(super) async fn search(
    store: &OutputArtifactStore,
    id: &OutputArtifactId,
    query: &str,
    offset: u64,
    limit: usize,
    ceiling: usize,
) -> std::io::Result<(Vec<u64>, Option<u64>)> {
    if query.is_empty() || query.len() > 1_024 || limit == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid search bounds",
        ));
    }
    let limit = limit.min(ceiling);
    let scan_end = offset.saturating_add(MAX_SEARCH_SCAN_BYTES as u64);
    let read_end = scan_end.saturating_add(query.len().saturating_sub(1) as u64);
    let query = query.as_bytes();
    let (mut cursor, mut candidate, mut carry, mut offsets) =
        (offset, offset, Vec::new(), Vec::new());
    loop {
        let remaining = usize::try_from(read_end.saturating_sub(cursor))
            .unwrap_or(usize::MAX)
            .min(MAX_ARTIFACT_READ_BYTES);
        if remaining == 0 {
            return Ok((offsets, Some(scan_end)));
        }
        let (bytes, start, end, next) = store.read_raw_bytes(id, cursor, remaining).await?;
        let base = start.saturating_sub(carry.len() as u64);
        carry.extend_from_slice(&bytes);
        if carry.len() >= query.len() {
            for index in 0..=carry.len() - query.len() {
                if &carry[index..index + query.len()] == query {
                    let found = base + index as u64;
                    if found >= candidate && found < scan_end {
                        offsets.push(found);
                        if offsets.len() == limit {
                            return Ok((offsets, Some(found.saturating_add(1))));
                        }
                    }
                }
            }
        }
        let Some(next) = next else {
            return Ok((offsets, None));
        };
        if end >= read_end {
            return Ok((offsets, Some(scan_end)));
        }
        let keep = query.len().saturating_sub(1).min(carry.len());
        candidate = end.saturating_sub(keep as u64);
        let drain = carry.len().saturating_sub(keep);
        carry.drain(..drain);
        cursor = next;
    }
}
