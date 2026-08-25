use codex_utils_path_uri::PathUri;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::PoisonError;

pub(crate) const MAX_READ_LEDGER_ENTRIES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadLedgerKey {
    pub(crate) environment_id: String,
    pub(crate) canonical_path: PathUri,
    pub(crate) file_size: u64,
    pub(crate) created_at_ms: i64,
    pub(crate) modified_at_ms: i64,
    pub(crate) start_byte: u64,
    pub(crate) end_byte: u64,
    pub(crate) next_offset: Option<u64>,
    pub(crate) eof: bool,
    pub(crate) line_continues: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ReadLedger {
    entries: Mutex<VecDeque<ReadLedgerEntry>>,
}

#[derive(Debug)]
struct ReadLedgerEntry {
    key: ReadLedgerKey,
    serialized_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadLedgerObservation {
    FirstRead,
    FingerprintChanged,
    ExactDuplicate,
}

impl ReadLedger {
    /// Records metadata for a bounded read and reports an exact prior result.
    ///
    /// This ledger is intentionally scoped to one live session incarnation. Resume/reopen starts
    /// it empty; durable artifact envelopes in effective history remain the recovery mechanism,
    /// so restart loses only the in-memory dedup opportunity and never a handle.
    ///
    /// The lock is held only for this in-memory operation. Artifact storage is
    /// deliberately performed by the caller after the lock has been released.
    pub(crate) fn observe(
        &self,
        key: ReadLedgerKey,
        serialized_digest: String,
    ) -> ReadLedgerObservation {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = entries.iter().position(|entry| entry.key == key) {
            let Some(mut entry) = entries.remove(index) else {
                return ReadLedgerObservation::FirstRead;
            };
            let observation = if entry.serialized_digest == serialized_digest {
                ReadLedgerObservation::ExactDuplicate
            } else {
                entry.serialized_digest = serialized_digest;
                ReadLedgerObservation::FingerprintChanged
            };
            entries.push_back(entry);
            return observation;
        }

        if let Some(index) = entries
            .iter()
            .position(|entry| entry.key.same_read_request(&key))
        {
            if entries.remove(index).is_none() {
                return ReadLedgerObservation::FirstRead;
            }
            entries.push_back(ReadLedgerEntry {
                key,
                serialized_digest,
            });
            return ReadLedgerObservation::FingerprintChanged;
        }

        entries.push_back(ReadLedgerEntry {
            key,
            serialized_digest,
        });
        while entries.len() > MAX_READ_LEDGER_ENTRIES {
            entries.pop_front();
        }
        ReadLedgerObservation::FirstRead
    }
}

impl ReadLedgerKey {
    fn same_read_request(&self, other: &Self) -> bool {
        self.environment_id == other.environment_id
            && self.canonical_path == other.canonical_path
            && self.start_byte == other.start_byte
            && self.end_byte == other.end_byte
            && self.next_offset == other.next_offset
            && self.eof == other.eof
            && self.line_continues == other.line_continues
    }
}

#[cfg(test)]
#[path = "read_file_ledger_tests.rs"]
mod tests;
