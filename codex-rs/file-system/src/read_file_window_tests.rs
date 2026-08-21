use super::*;
use bytes::Bytes;
use futures::stream;
use std::io;

fn limits(bytes: usize, lines: usize, chars: usize) -> ReadFileWindowBounds {
    ReadFileWindowBounds {
        offset: 0,
        max_bytes: bytes,
        max_line_fragments: lines,
        max_line_fragment_chars: chars,
    }
}

fn read(
    chunks: Vec<io::Result<Bytes>>,
    bounds: ReadFileWindowBounds,
    file_size: usize,
) -> io::Result<ReadFileWindow> {
    futures::executor::block_on(read_file_window(
        stream::iter(chunks),
        bounds,
        file_size as u64,
    ))
}

#[test]
fn handles_empty_and_arbitrary_chunk_boundaries() {
    let source = "café🙂\nsecond\n";
    let chunks = source
        .as_bytes()
        .chunks(1)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    let result = read(
        chunks,
        limits(/*bytes*/ 128, /*lines*/ 10, /*chars*/ 100),
        source.len(),
    )
    .expect("window should read");
    assert_eq!(result.text, source);
    assert_eq!(result.end_byte, source.len() as u64);
    assert!(result.eof);

    let result = read(
        vec![
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"ok")),
            Ok(Bytes::new()),
        ],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 2,
    )
    .expect("empty chunks should not block progress");
    assert_eq!(result.text, "ok");
}

#[test]
fn preserves_multibyte_scalar_split_across_chunks() {
    let source = "aéz";
    let result = read(
        vec![
            Ok(Bytes::from_static(b"a\xc3")),
            Ok(Bytes::from_static(b"\xa9z")),
        ],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        source.len(),
    )
    .expect("split scalar should decode");
    assert_eq!(result.text, source);
}

#[test]
fn returns_stream_errors_without_replacing_them() {
    let error = read(
        vec![Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stream failed",
        ))],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 11,
    )
    .expect_err("stream error should be returned");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn rejects_incomplete_and_invalid_utf8() {
    let incomplete = read(
        vec![Ok(Bytes::from_static(b"a\xc3"))],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 2,
    )
    .expect_err("incomplete scalar should be rejected");
    assert_eq!(incomplete.kind(), io::ErrorKind::InvalidData);

    let invalid = read(
        vec![Ok(Bytes::from_static(b"\xff"))],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 1,
    )
    .expect_err("invalid UTF-8 should be rejected");
    assert_eq!(invalid.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn enforces_raw_byte_budget_without_slicing_a_scalar() {
    let source = "\\\"\n🙂";
    let result = read(
        vec![Ok(Bytes::copy_from_slice(source.as_bytes()))],
        limits(/*bytes*/ 2, /*lines*/ 10, /*chars*/ 100),
        source.len(),
    )
    .expect("the first escaped scalar should fit");
    assert_eq!(result.text, "\\\"");
    assert_eq!(result.end_byte, 2);
    assert_eq!(result.next_offset, Some(2));
}

#[test]
fn rejects_zero_limits_and_offsets_past_eof() {
    for bounds in [
        limits(/*bytes*/ 0, /*lines*/ 10, /*chars*/ 100),
        limits(/*bytes*/ 16, /*lines*/ 0, /*chars*/ 100),
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 0),
    ] {
        let error = read(
            vec![Ok(Bytes::from_static(b"x"))],
            bounds,
            /*file_size*/ 1,
        )
        .expect_err("zero window limits must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    let error = read(
        vec![],
        ReadFileWindowBounds {
            offset: 2,
            max_bytes: 16,
            max_line_fragments: 10,
            max_line_fragment_chars: 100,
        },
        /*file_size*/ 1,
    )
    .expect_err("past-EOF offsets must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn rejects_early_eof_and_unbounded_empty_chunks() {
    let early_eof = read(
        vec![Ok(Bytes::from_static(b"x"))],
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 2,
    )
    .expect_err("a short stream must not claim EOF");
    assert_eq!(early_eof.kind(), io::ErrorKind::UnexpectedEof);

    let empty_chunks = (0..1_025).map(|_| Ok(Bytes::new())).collect();
    let no_progress = read(
        empty_chunks,
        limits(/*bytes*/ 16, /*lines*/ 10, /*chars*/ 100),
        /*file_size*/ 1,
    )
    .expect_err("an empty stream must have a bounded work limit");
    assert_eq!(no_progress.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn enforces_line_fragment_and_scalar_limits_with_exact_continuation() {
    let source = "abcdef\nrest";
    let result = read(
        vec![Ok(Bytes::from_static(source.as_bytes()))],
        limits(/*bytes*/ 128, /*lines*/ 10, /*chars*/ 3),
        source.len(),
    )
    .expect("line fragment should be bounded");
    assert_eq!(result.text, "abc");
    assert_eq!(result.end_byte, 3);
    assert_eq!(result.next_offset, Some(3));
    assert!(result.line_continues);

    let line_limited = read(
        vec![Ok(Bytes::from_static(b"one\ntwo\nthree"))],
        limits(/*bytes*/ 128, /*lines*/ 2, /*chars*/ 100),
        /*file_size*/ 13,
    )
    .expect("line limit should be bounded");
    assert_eq!(line_limited.text, "one\ntwo\n");
    assert_eq!(line_limited.end_byte, 8);
    assert_eq!(line_limited.next_offset, Some(8));
}

#[test]
fn scalar_limit_is_independent_of_transport_chunk_boundaries() {
    let result = read(
        vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def\nrest")),
        ],
        limits(/*bytes*/ 128, /*lines*/ 10, /*chars*/ 3),
        /*file_size*/ 10,
    )
    .expect("the next transport chunk should complete the bounded window");
    assert_eq!(result.text, "abc");
    assert_eq!(result.end_byte, 3);
    assert_eq!(result.next_offset, Some(3));
    assert!(result.line_continues);
}

#[test]
fn line_limit_is_independent_of_transport_chunk_boundaries() {
    let result = read(
        vec![
            Ok(Bytes::from_static(b"first")),
            Ok(Bytes::from_static(b" line\nsecond")),
        ],
        limits(/*bytes*/ 128, /*lines*/ 1, /*chars*/ 100),
        /*file_size*/ 18,
    )
    .expect("split line should finish before the line limit");
    assert_eq!(result.text, "first line\n");
    assert_eq!(result.next_offset, Some(11));
    assert!(!result.eof);
}

#[test]
fn detects_offsets_inside_utf8_code_points() {
    let result = read(
        vec![Ok(Bytes::from_static(b"\xa9"))],
        ReadFileWindowBounds {
            offset: 1,
            max_bytes: 16,
            max_line_fragments: 10,
            max_line_fragment_chars: 100,
        },
        /*file_size*/ 2,
    );
    let error = result.expect_err("continuation-byte offset should be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
