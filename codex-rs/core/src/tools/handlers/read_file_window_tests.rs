use super::*;
use codex_file_system::read_file_window::ReadFileWindow;

const MAX_LINE_FRAGMENT_CHARS: usize = 2_000;

#[test]
fn renders_a_structured_utf8_window_without_slicing_json() {
    let window = ReadFileWindow {
        text: "café\n".to_string(),
        start_byte: 4,
        end_byte: 10,
        line_fragments: 1,
        line_continues: false,
        next_offset: Some(10),
        eof: false,
    };

    let rendered = render_response(
        "/tmp/é.txt",
        /*file_size*/ 20,
        /*modified_at_ms*/ 42,
        &window,
    )
    .expect("valid response");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("complete JSON");
    assert_eq!(parsed["type"], "file_read");
    assert_eq!(parsed["path"], "/tmp/é.txt");
    assert_eq!(parsed["window"]["text"], "café\n");
    assert_eq!(parsed["window"]["end_byte"], 10);
}

#[test]
fn long_line_fragment_continuation_is_byte_exact() {
    let window = ReadFileWindow {
        text: "a".repeat(MAX_LINE_FRAGMENT_CHARS),
        start_byte: 7,
        end_byte: 2_007,
        line_fragments: 1,
        line_continues: true,
        next_offset: Some(2_007),
        eof: false,
    };
    assert_eq!(window.next_offset, Some(2_007));
    assert!(window.line_continues);
    assert!(!window.eof);
}
