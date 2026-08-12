use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::format_output_omission_marker;
use std::collections::VecDeque;

/// A capped buffer that preserves a stable prefix ("head") and suffix ("tail"),
/// dropping the middle once it exceeds the configured maximum. The buffer is
/// symmetric meaning 50% of the capacity is allocated to the head and 50% is
/// allocated to the tail.
#[derive(Debug, Default)]
pub(crate) struct HeadTailBuffer<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
    capture_limit: Option<usize>,
    complete: Option<Vec<u8>>,
    capture_limit_exceeded: bool,
}

#[cfg(test)]
impl<const MAX_BYTES: usize> PartialEq for HeadTailBuffer<MAX_BYTES> {
    fn eq(&self, other: &Self) -> bool {
        self.head == other.head
            && self.tail == other.tail
            && self.omitted_bytes == other.omitted_bytes
    }
}

#[cfg(test)]
impl<const MAX_BYTES: usize> Eq for HeadTailBuffer<MAX_BYTES> {}

impl<const MAX_BYTES: usize> HeadTailBuffer<MAX_BYTES> {
    const HEAD_BUDGET: usize = MAX_BYTES / 2;
    const TAIL_BUDGET: usize = MAX_BYTES.saturating_sub(Self::HEAD_BUDGET);

    /// Create a preview buffer with a bounded, opt-in complete capture.
    pub(crate) fn new_recoverable(capture_bytes: usize) -> Self {
        Self {
            capture_limit: Some(capture_bytes),
            complete: Some(Vec::with_capacity(capture_bytes.min(MAX_BYTES))),
            ..Self::default()
        }
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes that were dropped from the middle due to the size cap.
    pub(crate) fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }

    /// Total bytes observed by the buffer, including bytes omitted by the cap.
    pub(crate) fn total_bytes(&self) -> usize {
        self.retained_bytes().saturating_add(self.omitted_bytes)
    }

    pub(crate) fn capture_limit_exceeded(&self) -> bool {
        self.capture_limit_exceeded
    }

    /// Append a chunk of bytes to the buffer.
    ///
    /// Bytes are first added to the head until the head budget is full; any
    /// remaining bytes are added to the tail, with older tail bytes being
    /// dropped to preserve the tail budget.
    pub(crate) fn push_chunk<B: AsRef<[u8]>>(&mut self, chunk: B) {
        let chunk = chunk.as_ref();
        if chunk.is_empty() {
            return;
        }
        if let Some(limit) = self.capture_limit
            && let Some(complete) = self.complete.as_mut()
        {
            if complete.len().saturating_add(chunk.len()) <= limit {
                complete.extend_from_slice(chunk);
            } else {
                self.complete = None;
                self.capture_limit_exceeded = true;
            }
        }
        self.push_preview_chunk(chunk);
    }

    /// Snapshot the retained output as head and tail chunks.
    #[allow(dead_code)]
    pub(crate) fn snapshot_chunks(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(2);
        if !self.head.is_empty() {
            out.push(self.head.clone());
        }
        if !self.tail.is_empty() {
            out.push(self.tail.iter().copied().collect());
        }
        out
    }

    /// Fill the stable prefix and return the bytes that did not fit.
    fn fill_head<'a>(&mut self, chunk: &'a [u8]) -> &'a [u8] {
        let Self {
            head,
            tail: _,
            omitted_bytes: _,
            ..
        } = self;

        let remaining_head = Self::HEAD_BUDGET.saturating_sub(head.len());
        // A shorter chunk fits entirely in the head.
        let (chunk_head, chunk_tail) = chunk
            .split_at_checked(remaining_head)
            .unwrap_or((chunk, &[]));
        head.extend_from_slice(chunk_head);
        chunk_tail
    }

    fn push_preview_chunk(&mut self, chunk: &[u8]) {
        let chunk = self.fill_head(chunk);
        self.push_tail(chunk);
    }

    /// Append bytes known not to belong in the head, keeping the newest tail bytes.
    fn push_tail(&mut self, chunk: &[u8]) {
        let Self {
            head: _,
            tail,
            omitted_bytes,
            ..
        } = self;

        let remaining_tail = Self::TAIL_BUDGET.saturating_sub(tail.len());
        let excess_tail = chunk.len().saturating_sub(remaining_tail);
        *omitted_bytes = omitted_bytes.saturating_add(excess_tail);

        // Discard old tail bytes first, then skip any excess incoming bytes.
        let chunk = match excess_tail.checked_sub(tail.len()) {
            None => {
                tail.drain(..excess_tail);
                chunk
            }
            Some(skip) => {
                tail.clear();
                &chunk[skip..]
            }
        };
        tail.extend(chunk);
    }

    /// Return the retained output as a single byte vector.
    ///
    /// The output is formed by concatenating head chunks, then tail chunks.
    /// Omitted bytes are not represented in the returned value.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        out.extend_from_slice(&self.head);
        out.extend(self.tail.iter().copied());
        out
    }

    /// Return the retained output with an explicit marker between the head and
    /// tail when bytes were omitted.
    pub(crate) fn to_bytes_with_omission_marker(&self) -> Vec<u8> {
        if self.omitted_bytes == 0 {
            return self.to_bytes();
        }

        let marker = format_output_omission_marker(self.omitted_bytes);
        let marker_delimiter_bytes = 2;
        let mut out = Vec::with_capacity(
            self.retained_bytes()
                .saturating_add(marker.len())
                .saturating_add(marker_delimiter_bytes),
        );
        out.extend_from_slice(&self.head);
        out.push(b'\n');
        out.extend_from_slice(marker.as_bytes());
        out.push(b'\n');
        out.extend(self.tail.iter().copied());
        out
    }

    /// Append a later buffer with the same budget. This preserves the summary
    /// of the original concatenated output, including its omission count.
    pub(crate) fn push_buffer(&mut self, mut buffer: Self) {
        let captured = match (self.capture_limit, &mut self.complete, &mut buffer.complete) {
            (Some(limit), Some(destination), Some(source))
                if destination.len().saturating_add(source.len()) <= limit =>
            {
                destination.append(source);
                true
            }
            _ => false,
        };
        if self.capture_limit.is_some() && !captured {
            self.complete = None;
            self.capture_limit_exceeded = true;
        }
        self.capture_limit_exceeded |= buffer.capture_limit_exceeded;

        let Self {
            head,
            tail,
            omitted_bytes,
            ..
        } = buffer;

        self.push_preview_chunk(&head);
        let (first, second) = tail.as_slices();
        self.push_preview_chunk(first);
        self.push_preview_chunk(second);
        self.omitted_bytes = self.omitted_bytes.saturating_add(omitted_bytes);
    }

    /// Drain the retained output and capture metadata while preserving the configured limits.
    pub(crate) fn drain(&mut self) -> Self {
        Self {
            head: std::mem::take(&mut self.head),
            tail: std::mem::take(&mut self.tail),
            omitted_bytes: std::mem::take(&mut self.omitted_bytes),
            capture_limit: self.capture_limit,
            complete: self
                .capture_limit
                .map(|_| Vec::new())
                .and_then(|next| self.complete.replace(next)),
            capture_limit_exceeded: std::mem::take(&mut self.capture_limit_exceeded),
        }
    }

    pub(crate) fn take_complete_bytes(&mut self) -> Option<Vec<u8>> {
        self.complete.take()
    }
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
