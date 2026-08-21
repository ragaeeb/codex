use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use std::io;
use std::str;

/// Neutral limits for one byte-based, line-bounded streamed read.
#[derive(Debug, Clone, Copy)]
pub struct ReadFileWindowBounds {
    pub offset: u64,
    /// Maximum raw UTF-8 bytes returned in the window.
    pub max_bytes: usize,
    pub max_line_fragments: usize,
    pub max_line_fragment_chars: usize,
}

/// A complete UTF-8 window and the exact byte continuation state for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadFileWindow {
    pub text: String,
    pub start_byte: u64,
    pub end_byte: u64,
    pub line_fragments: usize,
    pub line_continues: bool,
    pub next_offset: Option<u64>,
    pub eof: bool,
}

/// Consumes only a bounded stream window, preserving UTF-8 and exact byte offsets.
pub async fn read_file_window(
    mut stream: impl Stream<Item = io::Result<Bytes>> + Unpin,
    bounds: ReadFileWindowBounds,
    file_size: u64,
) -> io::Result<ReadFileWindow> {
    if bounds.offset > file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offset is beyond the declared file size",
        ));
    }
    if bounds.max_bytes == 0
        || bounds.max_line_fragments == 0
        || bounds.max_line_fragment_chars == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "read window limits must be positive",
        ));
    }

    let mut cursor = bounds.offset;
    let mut pending = Vec::with_capacity(4);
    let mut pending_start = bounds.offset;
    let mut text = String::new();
    let mut encoded_text_len = 0;
    let mut line_fragments = 0;
    let mut line_chars = 0;
    let mut line_open = false;
    let mut empty_chunks = 0;
    let mut stopped_for_line_limit = false;
    'chunks: while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.is_empty() {
            empty_chunks += 1;
            if empty_chunks > 1_024 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "stream made no progress",
                ));
            }
            continue;
        }
        empty_chunks = 0;
        for byte in chunk {
            if cursor >= file_size {
                break;
            }
            let byte_offset = cursor;
            cursor = cursor.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "file offset overflow")
            })?;
            if byte == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file contains NUL bytes",
                ));
            }
            if pending.is_empty()
                && byte_offset == bounds.offset
                && (byte & 0b1100_0000) == 0b1000_0000
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "offset {} is inside a UTF-8 code point; use a byte boundary near {}",
                        bounds.offset,
                        bounds.offset.saturating_sub(1)
                    ),
                ));
            }
            if pending.is_empty() {
                pending_start = byte_offset;
            }
            pending.push(byte);
            let decoded = match str::from_utf8(&pending) {
                Ok(decoded) => decoded,
                Err(error) if error.error_len().is_none() => continue,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "file is not valid UTF-8",
                    ));
                }
            };
            let character = decoded.chars().next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file contained an empty UTF-8 unit",
                )
            })?;
            if character != '\n' && line_chars >= bounds.max_line_fragment_chars {
                return Ok(finish_window(
                    text,
                    bounds.offset,
                    pending_start,
                    line_fragments,
                    /*line_continues*/ true,
                    file_size,
                ));
            }

            let candidate_fragments = line_fragments + usize::from(!line_open);
            let candidate_bytes = encoded_text_len + character.len_utf8();
            if candidate_bytes > bounds.max_bytes {
                if text.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "max_bytes is too small to include one UTF-8 character",
                    ));
                }
                return Ok(finish_window(
                    text,
                    bounds.offset,
                    pending_start,
                    line_fragments,
                    character != '\n',
                    file_size,
                ));
            }

            text.push(character);
            encoded_text_len = candidate_bytes;
            line_fragments = candidate_fragments;
            line_open = true;
            if character == '\n' {
                line_chars = 0;
                line_open = false;
            } else {
                line_chars += 1;
            }
            pending.clear();

            if character == '\n' && line_fragments >= bounds.max_line_fragments {
                stopped_for_line_limit = true;
                break 'chunks;
            }
        }
        if cursor >= file_size || (line_fragments >= bounds.max_line_fragments && !line_open) {
            break;
        }
    }

    if !pending.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file ends in an incomplete UTF-8 code point",
        ));
    }
    if cursor < file_size && !stopped_for_line_limit {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stream ended before the declared file size",
        ));
    }
    Ok(finish_window(
        text,
        bounds.offset,
        cursor,
        line_fragments,
        line_open && cursor < file_size,
        file_size,
    ))
}

fn finish_window(
    text: String,
    start_byte: u64,
    end_byte: u64,
    line_fragments: usize,
    line_continues: bool,
    file_size: u64,
) -> ReadFileWindow {
    let eof = end_byte >= file_size;
    ReadFileWindow {
        text,
        start_byte,
        end_byte,
        line_fragments,
        line_continues: line_continues && !eof,
        next_offset: (!eof).then_some(end_byte),
        eof,
    }
}

#[cfg(test)]
#[path = "read_file_window_tests.rs"]
mod tests;
