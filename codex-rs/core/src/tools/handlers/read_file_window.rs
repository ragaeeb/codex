use codex_file_system::read_file_window::ReadFileWindow;
use codex_utils_output_truncation::OutputArtifactId;
use serde::Serialize;
use std::io;

pub(crate) const DEFAULT_MAX_BYTES: usize = 32 * 1024;
pub(crate) const DEFAULT_MAX_LINES: usize = 200;
pub(crate) const MAX_RESPONSE_BYTES: usize = 128 * 1024;
/// Conservative complete-item model-facing cap.
///
/// Eight KiB is intentionally below the ten-thousand-token hard ceiling even for a tokenizer that
/// approaches one token per byte. This crosses the repository's one-thousand-token manual-review
/// threshold, so the cap is an explicit Stage 2 review acceptance. The larger request ceiling
/// remains a transport bound; fitting happens structurally before the result can enter context.
pub(crate) const MAX_MODEL_VISIBLE_BYTES: usize = 8 * 1024;
pub(crate) const MAX_RESPONSE_LINES: usize = 2_000;
pub(crate) const MAX_LINE_FRAGMENT_CHARS: usize = 2_000;
pub(crate) const MIN_REQUEST_BYTES: usize = 1;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadBounds {
    pub(crate) offset: u64,
    pub(crate) max_bytes: usize,
    pub(crate) max_lines: usize,
}

impl ReadBounds {
    pub(crate) fn model_max_bytes(self) -> usize {
        self.max_bytes.min(MAX_MODEL_VISIBLE_BYTES)
    }
}

#[derive(Debug, Serialize)]
struct FileReadResponse {
    #[serde(rename = "type")]
    response_type: &'static str,
    version: u8,
    path: String,
    fingerprint: FileReadFingerprint,
    window: FileReadWindow,
    next_offset: Option<u64>,
    eof: bool,
    continuation: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct FileReadFingerprint {
    size_bytes: u64,
    modified_at_ms: i64,
    window_digest: String,
}

#[derive(Debug, Serialize)]
struct FileReadWindow {
    start_byte: u64,
    end_byte: u64,
    text: String,
    line_fragments: usize,
    line_continues: bool,
}

pub(crate) fn fit_window_to_response_budget(
    path: &str,
    file_size: u64,
    modified_at_ms: i64,
    window: ReadFileWindow,
    max_bytes: usize,
) -> io::Result<ReadFileWindow> {
    if render_response(path, file_size, modified_at_ms, &window)?.len() <= max_bytes {
        return Ok(window);
    }

    let empty = truncate_window(&window, /*text_len*/ 0, file_size);
    if render_response(path, file_size, modified_at_ms, &empty)?.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response metadata leaves no room for a UTF-8 character",
        ));
    }
    if let Some(first_character) = window.text.chars().next()
        && render_response(
            path,
            file_size,
            modified_at_ms,
            &truncate_window(&window, first_character.len_utf8(), file_size),
        )?
        .len()
            > max_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "response metadata leaves no room for a UTF-8 character",
        ));
    }

    let boundaries = window
        .text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(window.text.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    let mut best = empty;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = truncate_window(&window, boundaries[middle], file_size);
        if render_response(path, file_size, modified_at_ms, &candidate)?.len() <= max_bytes {
            best = candidate;
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    Ok(best)
}

fn truncate_window(window: &ReadFileWindow, text_len: usize, file_size: u64) -> ReadFileWindow {
    let mut truncated = window.clone();
    truncated.text.truncate(text_len);
    truncated.end_byte = truncated.start_byte + text_len as u64;
    truncated.line_fragments = truncated
        .text
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + usize::from(!truncated.text.is_empty() && !truncated.text.ends_with('\n'));
    truncated.eof = truncated.end_byte >= file_size;
    truncated.line_continues = !truncated.text.ends_with('\n') && !truncated.eof;
    truncated.next_offset = (!truncated.eof).then_some(truncated.end_byte);
    truncated
}

pub(crate) fn render_response(
    path: &str,
    file_size: u64,
    modified_at_ms: i64,
    window: &ReadFileWindow,
) -> io::Result<String> {
    serde_json::to_string(&FileReadResponse {
        response_type: "file_read",
        version: 1,
        path: path.to_string(),
        fingerprint: FileReadFingerprint {
            size_bytes: file_size,
            modified_at_ms,
            window_digest: format!(
                "sha256:{}",
                OutputArtifactId::for_text(&window.text).digest()
            ),
        },
        window: FileReadWindow {
            start_byte: window.start_byte,
            end_byte: window.end_byte,
            text: window.text.clone(),
            line_fragments: window.line_fragments,
            line_continues: window.line_continues,
        },
        next_offset: window.next_offset,
        eof: window.eof,
        continuation: window
            .next_offset
            .map(|_| "Call read_file again with offset=next_offset."),
    })
    .map_err(io::Error::other)
}

pub(crate) fn response_base_len(
    path: &str,
    file_size: u64,
    modified_at_ms: i64,
    start_byte: u64,
    max_lines: usize,
) -> usize {
    let response = FileReadResponse {
        response_type: "file_read",
        version: 1,
        path: path.to_string(),
        fingerprint: FileReadFingerprint {
            size_bytes: file_size,
            modified_at_ms,
            window_digest: format!("sha256:{}", OutputArtifactId::for_text("").digest()),
        },
        window: FileReadWindow {
            start_byte,
            end_byte: u64::MAX,
            text: String::new(),
            line_fragments: max_lines,
            line_continues: true,
        },
        next_offset: Some(u64::MAX),
        eof: false,
        continuation: Some("Call read_file again with offset=next_offset."),
    };
    serde_json::to_string(&response)
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
#[path = "read_file_window_tests.rs"]
mod tests;
