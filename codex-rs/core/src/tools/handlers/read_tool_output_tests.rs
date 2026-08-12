use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn absent_search_returns_a_bounded_scan_continuation() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let artifact = store
        .store_text(&"x".repeat(MAX_SEARCH_SCAN_BYTES * 2))
        .await
        .expect("store artifact");

    let (matches, next) = search(
        &store,
        &artifact.id,
        "absent",
        /*offset*/ 0,
        /*limit*/ 20,
        MAX_MATCHES,
    )
    .await
    .expect("bounded search");

    assert_eq!(matches, Vec::<u64>::new());
    assert_eq!(next, Some(MAX_SEARCH_SCAN_BYTES as u64));
}

#[tokio::test]
async fn search_finds_a_match_crossing_the_scan_boundary() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let mut original = "x".repeat(MAX_SEARCH_SCAN_BYTES - 3);
    original.push_str("boundary-match");
    let artifact = store.store_text(&original).await.expect("store artifact");

    let (matches, next) = search(
        &store,
        &artifact.id,
        "boundary-match",
        /*offset*/ 0,
        /*limit*/ 20,
        MAX_MATCHES,
    )
    .await
    .expect("bounded search");

    assert_eq!(matches, vec![(MAX_SEARCH_SCAN_BYTES - 3) as u64]);
    assert_eq!(next, None);
}

#[test]
fn retrieval_documents_remain_structurally_valid_when_bounded() {
    let value = json!({
        "type": "tool_output_artifact_window",
        "mode": "bytes",
        "artifact_id": format!("out_{}", "0".repeat(64)),
        "start_byte": 0,
        "end_byte": 2_600,
        "text": "quoted \" text \\ and more".repeat(100),
        "next_offset": 42,
        "complete": false,
    });

    let rendered = bounded_result(value, /*max_bytes*/ 512);

    assert!(rendered.len() <= 512);
    let bounded = serde_json::from_str::<serde_json::Value>(&rendered).expect("valid bounded JSON");
    assert_eq!(bounded["type"], "tool_output_artifact_window");
    let text = bounded["text"].as_str().expect("bounded text");
    assert!("quoted \" text \\ and more".repeat(100).starts_with(text));
    assert_eq!(bounded["end_byte"], text.len() as u64);
    assert_eq!(bounded["next_offset"], text.len() as u64);
}

#[tokio::test]
async fn line_windows_seek_past_the_search_scan_ceiling() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let first_line = "x".repeat(MAX_SEARCH_SCAN_BYTES + 1);
    let original = format!("{first_line}\ntarget line\n");
    let artifact = store.store_text(&original).await.expect("store artifact");

    let (text, start_byte, end_byte, next_line, next_byte) = read_lines(
        &store,
        &artifact.id,
        /*start_line*/ 2,
        /*count*/ 1,
    )
    .await
    .expect("line window");

    assert_eq!(text, "target line\n");
    assert_eq!(start_byte, (first_line.len() + 1) as u64);
    assert_eq!(end_byte, original.len() as u64);
    assert_eq!((next_line, next_byte), (None, None));
}
