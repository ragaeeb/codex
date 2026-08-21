use anyhow::Result;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LOCAL_FS;
use codex_file_system::read_file_snapshot::read_file_snapshot_transaction;
use codex_file_system::read_file_window::ReadFileWindowBounds;
use codex_utils_path_uri::PathUri;
use std::io;
use tempfile::tempdir;

#[tokio::test]
async fn local_snapshot_reads_from_an_exact_offset_and_handles_eof() -> Result<()> {
    let temp = tempdir()?;
    let path = temp.path().join("snapshot.txt");
    let contents = "aé\nlast";
    tokio::fs::write(&path, contents).await?;
    let uri = PathUri::from_host_native_path(&path)?;
    let sandbox: Option<&FileSystemSandboxContext> = None;
    let snapshot = read_file_snapshot_transaction(
        LOCAL_FS.as_ref(),
        &uri,
        sandbox,
        ReadFileWindowBounds {
            offset: 1,
            max_bytes: 3,
            max_line_fragments: 10,
            max_line_fragment_chars: 2_000,
        },
    )
    .await?;
    let file_size = snapshot.metadata.size;
    let window = snapshot.window;
    assert_eq!(window.text, "é\n");
    assert_eq!(window.start_byte, 1);
    assert_eq!(window.end_byte, 4);
    assert_eq!(window.next_offset, Some(4));
    assert!(!window.eof);

    let eof = read_file_snapshot_transaction(
        LOCAL_FS.as_ref(),
        &uri,
        sandbox,
        ReadFileWindowBounds {
            offset: file_size,
            max_bytes: 1,
            max_line_fragments: 1,
            max_line_fragment_chars: 1,
        },
    )
    .await?;
    assert!(eof.window.eof);
    assert_eq!(eof.window.start_byte, file_size);

    let past_eof = read_file_snapshot_transaction(
        LOCAL_FS.as_ref(),
        &uri,
        sandbox,
        ReadFileWindowBounds {
            offset: file_size + 1,
            max_bytes: 1,
            max_line_fragments: 1,
            max_line_fragment_chars: 1,
        },
    )
    .await
    .expect_err("offset past EOF should be rejected");
    assert_eq!(past_eof.kind(), io::ErrorKind::InvalidInput);
    Ok(())
}
