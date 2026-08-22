use super::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_tools::ToolOutput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::OutputArtifactStore;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::read_tool_output_window::MAX_SEARCH_SCAN_BYTES;
use super::read_tool_output_window::bounded_result;
use super::read_tool_output_window::read_bytes_value;
use super::read_tool_output_window::read_lines;
use super::read_tool_output_window::search;

#[test]
fn retrieval_output_logging_is_content_free() {
    let output = ReadToolOutputResult::new(
        r#"{"artifact_id":"SECRET_ARTIFACT_ID","text":"SECRET_CONTENT","query":"SECRET_QUERY"}"#
            .to_string(),
        "bytes",
    );
    let log = output.log_output();
    assert!(log.contains("read_tool_output"));
    assert!(log.contains("serialized_bytes"));
    for secret in ["SECRET_ARTIFACT_ID", "SECRET_CONTENT", "SECRET_QUERY"] {
        assert!(!log.contains(secret));
    }
}

#[test]
fn a_budget_without_a_recovery_handle_stays_untrusted() {
    let output = ReadToolOutputResult::new(
        r#"{"type":"tool_output_error","version":1,"error":"bounded"}"#.to_string(),
        "bytes",
    );
    assert_eq!(
        output.provenance(),
        codex_tools::ToolOutputProvenance::Untrusted
    );
}

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

#[tokio::test]
async fn invalid_utf8_retrieval_uses_exact_base64_windows_and_byte_search() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let bytes = [0xff, 0x00, b'S', b'E', b'C', b'R', b'E', b'T'];
    let artifact = store.store_bytes(&bytes).await.expect("store bytes");

    let value = read_bytes_value(&store, &artifact.id, /*offset*/ 0, bytes.len())
        .await
        .expect("base64 window");
    assert_eq!(value["encoding"], "base64");
    assert_eq!(
        BASE64_STANDARD
            .decode(value["bytes_base64"].as_str().expect("base64"))
            .expect("decode"),
        bytes
    );
    assert_eq!(
        search(
            &store,
            &artifact.id,
            "SECRET",
            /*offset*/ 0,
            /*limit*/ 10,
            MAX_MATCHES,
        )
        .await
        .expect("byte search"),
        (vec![2], None)
    );

    for bytes in [vec![0x80, 0x00, b'a'], vec![0xc3, b'a'], vec![0xff, b'b']] {
        let artifact = store.store_bytes(&bytes).await.expect("store raw artifact");
        let value = read_bytes_value(&store, &artifact.id, /*offset*/ 0, bytes.len())
            .await
            .expect("invalid UTF-8 must use base64 at offset zero");
        assert_eq!(value["encoding"], "base64");
        assert_eq!(
            BASE64_STANDARD
                .decode(value["bytes_base64"].as_str().expect("base64"))
                .expect("decode base64"),
            bytes
        );
    }

    let bytes = vec![0xc2, 0x80, 0xff];
    let artifact = store.store_bytes(&bytes).await.expect("store raw artifact");
    let mut recovered = Vec::new();
    let mut offset = 0;
    loop {
        let value = read_bytes_value(&store, &artifact.id, offset, /*limit*/ 1)
            .await
            .expect("one-byte raw pages must remain recoverable");
        recovered.extend(
            BASE64_STANDARD
                .decode(value["bytes_base64"].as_str().expect("base64"))
                .expect("decode base64"),
        );
        let Some(next) = value["next_offset"].as_u64() else {
            break;
        };
        offset = next;
    }
    assert_eq!(recovered, bytes);

    let bytes = vec![b'a', 0x80, 0xff, b'z'];
    let artifact = store.store_bytes(&bytes).await.expect("store raw artifact");
    let value = read_bytes_value(&store, &artifact.id, /*offset*/ 1, /*limit*/ 2)
        .await
        .expect("continuation-looking bytes use base64");
    assert_eq!(value["encoding"], "base64");
    assert_eq!(
        BASE64_STANDARD
            .decode(value["bytes_base64"].as_str().expect("base64"))
            .expect("decode"),
        bytes[1..3].to_vec()
    );
    let next = value["next_offset"].as_u64().expect("raw continuation");
    let second = read_bytes_value(&store, &artifact.id, next, /*limit*/ 2)
        .await
        .expect("second raw page");
    let second_bytes = second
        .get("bytes_base64")
        .and_then(Value::as_str)
        .map(|encoded| BASE64_STANDARD.decode(encoded).expect("decode"))
        .or_else(|| {
            second
                .get("text")
                .and_then(Value::as_str)
                .map(str::as_bytes)
                .map(Vec::from)
        })
        .expect("second byte page");
    assert_eq!(second_bytes, bytes[3..].to_vec());
}

#[test]
fn bounded_search_continuation_starts_at_the_omitted_match() {
    let value = json!({
        "type": "tool_output_artifact_search",
        "mode": "search",
        "artifact_id": format!("out_{}", "0".repeat(64)),
        "byte_offsets": (0..64).map(|offset| offset * 10).collect::<Vec<_>>(),
        "next_offset": 640,
        "complete": false,
    });

    let rendered = bounded_result(value, /*max_bytes*/ 200);
    let bounded = serde_json::from_str::<Value>(&rendered).expect("valid bounded search JSON");
    let next = bounded["next_offset"].as_u64().expect("continuation");
    assert_eq!(next, 130);
}

#[test]
fn bounded_line_window_keeps_line_and_byte_continuations_distinct() {
    let value = json!({
        "type": "tool_output_artifact_window",
        "mode": "lines",
        "artifact_id": format!("out_{}", "0".repeat(64)),
        "start_line": 7,
        "start_byte": 100,
        "end_byte": 500,
        "text": "x".repeat(400),
        "next_offset": 8,
        "next_byte": 500,
        "scan_continues": false,
        "complete": false,
    });

    let rendered = bounded_result(value, /*max_bytes*/ 240);
    let bounded = serde_json::from_str::<Value>(&rendered).expect("valid bounded line JSON");
    assert_eq!(bounded["mode"], "lines");
    assert_eq!(bounded["next_offset"], 7);
    assert!(bounded["next_byte"].as_u64().unwrap_or_default() > 100);
    assert!(!bounded["text"].as_str().unwrap_or_default().is_empty());
}

#[test]
fn bounded_line_window_advances_line_cursor_for_retained_newlines() {
    let value = json!({
        "type": "tool_output_artifact_window",
        "mode": "lines",
        "artifact_id": format!("out_{}", "0".repeat(64)),
        "start_line": 7,
        "start_byte": 100,
        "end_byte": 500,
        "text": "first\nsecond\nthird".repeat(10),
        "next_offset": 10,
        "next_byte": 500,
        "scan_continues": false,
        "complete": false,
    });

    let rendered = bounded_result(value, /*max_bytes*/ 300);
    let bounded = serde_json::from_str::<Value>(&rendered).expect("valid bounded line JSON");
    let text = bounded["text"].as_str().expect("text");
    let retained_lines = text.bytes().filter(|byte| *byte == b'\n').count();
    assert_eq!(
        bounded["next_offset"],
        7 + u64::try_from(retained_lines).expect("line count")
    );
}

#[test]
fn low_policy_line_scan_continuation_keeps_a_resumable_cursor() {
    let value = json!({
        "type": "tool_output_artifact_window",
        "mode": "lines",
        "artifact_id": format!("out_{}", "0".repeat(64)),
        "start_line": 2,
        "start_byte": 524_288,
        "end_byte": 524_288,
        "text": "",
        "next_offset": 2,
        "next_byte": 524_288,
        "scan_continues": true,
        "scan_lines_remaining": 1,
        "complete": false,
    });

    let rendered = bounded_result(value, /*max_bytes*/ 200);
    assert!(rendered.len() <= 200);
    let bounded = serde_json::from_str::<Value>(&rendered).expect("valid continuation JSON");
    assert_eq!(bounded["next_byte"], 524_288);
    assert_eq!(bounded["scan_lines_remaining"], 1);
    assert_eq!(bounded["next_offset"], 2);
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
async fn line_windows_return_scan_continuation_at_the_search_scan_ceiling() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let first_line = "x".repeat(MAX_SEARCH_SCAN_BYTES + 1);
    let original = format!("{first_line}\ntarget line\n");
    let artifact = store.store_text(&original).await.expect("store artifact");

    let result = read_lines(
        &store,
        &artifact.id,
        /*start_line*/ 2,
        /*byte_offset*/ 0,
        /*scan_continuation*/ false,
        /*line_continuation*/ false,
        /*scan_lines_remaining*/ None,
        /*count*/ 1,
    )
    .await
    .expect("line indexing returns a bounded continuation");
    assert_eq!(
        result,
        (
            String::new(),
            MAX_SEARCH_SCAN_BYTES as u64,
            MAX_SEARCH_SCAN_BYTES as u64,
            Some(2),
            Some(MAX_SEARCH_SCAN_BYTES as u64),
            true,
            false,
            Some(1),
        )
    );

    let resumed = read_lines(
        &store,
        &artifact.id,
        /*start_line*/ 2,
        /*byte_offset*/ MAX_SEARCH_SCAN_BYTES as u64,
        /*scan_continuation*/ true,
        /*line_continuation*/ false,
        /*scan_lines_remaining*/ Some(1),
        /*count*/ 1,
    )
    .await
    .expect("line continuation should resume from its byte cursor");
    assert_eq!(resumed.0, "target line\n");
    assert_eq!(resumed.1, (MAX_SEARCH_SCAN_BYTES + 2) as u64);
}

#[tokio::test]
async fn line_scan_cursor_preserves_multiple_remaining_lines() {
    let temp = tempdir().expect("tempdir");
    let store = OutputArtifactStore::new(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("absolute tempdir")
            .join("tool_outputs/thread"),
    );
    let first_line = "x".repeat(MAX_SEARCH_SCAN_BYTES + 1);
    let original = format!("{first_line}\nsecond skipped line\ntarget line\n");
    let artifact = store.store_text(&original).await.expect("store artifact");

    let cursor = read_lines(
        &store,
        &artifact.id,
        /*start_line*/ 3,
        /*byte_offset*/ 0,
        /*scan_continuation*/ false,
        /*line_continuation*/ false,
        /*scan_lines_remaining*/ None,
        /*count*/ 1,
    )
    .await
    .expect("line indexing returns a cursor");
    assert_eq!(cursor.5, true);
    assert_eq!(cursor.6, false);
    assert_eq!(cursor.7, Some(2));

    let resumed = read_lines(
        &store,
        &artifact.id,
        /*start_line*/ 3,
        /*byte_offset*/ cursor.4.expect("cursor byte"),
        /*scan_continuation*/ true,
        /*line_continuation*/ false,
        /*scan_lines_remaining*/ cursor.7,
        /*count*/ 1,
    )
    .await
    .expect("line cursor should resume without rescanning");
    assert_eq!(resumed.0, "target line\n");
    assert_eq!(
        resumed.1,
        (MAX_SEARCH_SCAN_BYTES + 2 + "second skipped line\n".len()) as u64
    );
}
