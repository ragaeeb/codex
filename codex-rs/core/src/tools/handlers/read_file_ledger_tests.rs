use super::*;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use std::thread;

fn key(index: usize) -> ReadLedgerKey {
    ReadLedgerKey {
        environment_id: "primary".to_string(),
        canonical_path: PathUri::from_host_native_path(
            std::env::temp_dir().join(format!("read-{index}")),
        )
        .expect("temporary path is absolute"),
        file_size: 10,
        created_at_ms: 19,
        modified_at_ms: 20,
        start_byte: index as u64,
        end_byte: index as u64 + 1,
        next_offset: Some(index as u64 + 1),
        eof: false,
        line_continues: false,
    }
}

#[test]
fn records_only_exact_duplicate_digests() {
    let ledger = ReadLedger::default();

    assert_eq!(
        ledger.observe(key(/*index*/ 1), "digest-a".to_string()),
        ReadLedgerObservation::FirstRead
    );
    assert_eq!(
        ledger.observe(key(/*index*/ 1), "digest-b".to_string()),
        ReadLedgerObservation::FingerprintChanged
    );
    assert_eq!(
        ledger.observe(key(/*index*/ 1), "digest-a".to_string()),
        ReadLedgerObservation::FingerprintChanged
    );
    assert_eq!(
        ledger.observe(key(/*index*/ 1), "digest-a".to_string()),
        ReadLedgerObservation::ExactDuplicate
    );
}

#[test]
fn metadata_changes_replace_the_previous_window_entry() {
    let ledger = ReadLedger::default();
    let original = key(/*index*/ 1);
    let mut changed = original.clone();
    changed.created_at_ms += 1;

    assert_eq!(
        ledger.observe(original, "digest-a".to_string()),
        ReadLedgerObservation::FirstRead
    );
    assert_eq!(
        ledger.observe(changed.clone(), "digest-b".to_string()),
        ReadLedgerObservation::FingerprintChanged
    );
    assert_eq!(
        ledger.observe(changed, "digest-b".to_string()),
        ReadLedgerObservation::ExactDuplicate
    );
}

#[test]
fn evicts_oldest_entry_at_fixed_bound() {
    let ledger = ReadLedger::default();

    for index in 0..=MAX_READ_LEDGER_ENTRIES {
        assert_eq!(
            ledger.observe(key(index), format!("digest-{index}")),
            ReadLedgerObservation::FirstRead
        );
    }
    assert_eq!(
        ledger.observe(key(/*index*/ 0), "digest-0".to_string()),
        ReadLedgerObservation::FirstRead
    );
    assert_eq!(
        ledger.observe(
            key(/*index*/ MAX_READ_LEDGER_ENTRIES),
            format!("digest-{MAX_READ_LEDGER_ENTRIES}")
        ),
        ReadLedgerObservation::ExactDuplicate
    );
}

#[test]
fn concurrent_exact_windows_are_race_safe() {
    let ledger = Arc::new(ReadLedger::default());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut handles = Vec::with_capacity(/*capacity*/ 8);
    for _ in 0..8 {
        let ledger = Arc::clone(&ledger);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            ledger.observe(key(/*index*/ 7), "digest-7".to_string())
        }));
    }
    let observations = handles
        .into_iter()
        .map(|handle| handle.join().expect("ledger worker should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(
        observations
            .iter()
            .filter(|observation| **observation == ReadLedgerObservation::FirstRead)
            .count(),
        1
    );
    assert_eq!(
        observations
            .iter()
            .filter(|observation| **observation == ReadLedgerObservation::ExactDuplicate)
            .count(),
        7
    );
}
