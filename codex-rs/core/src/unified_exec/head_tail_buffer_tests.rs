use super::HeadTailBuffer;

use pretty_assertions::assert_eq;

#[test]
fn keeps_prefix_and_suffix_when_over_budget() {
    let mut buf = HeadTailBuffer::<10>::default();

    buf.push_chunk(b"0123456789");
    assert_eq!(buf.omitted_bytes(), 0);

    // Exceeds max by 2; we should keep head+tail and omit the middle.
    buf.push_chunk(b"ab");
    assert!(buf.omitted_bytes() > 0);

    let rendered = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(rendered.starts_with("01234"));
    assert!(rendered.ends_with("89ab"));
    assert_eq!(
        String::from_utf8_lossy(&buf.to_bytes_with_omission_marker()),
        "01234\n... 2 bytes omitted ...\n789ab"
    );
}

#[test]
fn max_bytes_zero_drops_everything() {
    let mut buf = HeadTailBuffer::<0>::default();
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 3);
    assert_eq!(buf.to_bytes(), b"");
}

#[test]
fn head_budget_zero_keeps_only_last_byte_in_tail() {
    let mut buf = HeadTailBuffer::<1>::default();
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 1);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"c");
}

#[test]
fn recoverable_capture_is_opt_in_and_hard_capped() {
    let mut preview_only = HeadTailBuffer::<4>::default();
    preview_only.push_chunk(b"abcdef");
    assert_eq!(preview_only.take_complete_bytes(), None);
    assert!(!preview_only.capture_limit_exceeded());

    let mut recoverable = HeadTailBuffer::<4>::new_recoverable(6);
    recoverable.push_chunk(b"abcdef");
    assert_eq!(recoverable.take_complete_bytes(), Some(b"abcdef".to_vec()));

    let mut over_limit = HeadTailBuffer::<4>::new_recoverable(5);
    over_limit.push_chunk(b"abcdef");
    assert_eq!(over_limit.take_complete_bytes(), None);
    assert!(over_limit.capture_limit_exceeded());
}

#[test]
fn draining_preserves_complete_capture_for_push_buffer() {
    let mut buf = HeadTailBuffer::<10>::new_recoverable(32);
    buf.push_chunk(b"0123456789");
    buf.push_chunk(b"ab");

    let drained = buf.drain();
    let mut collected = HeadTailBuffer::<10>::new_recoverable(32);
    collected.push_buffer(drained);

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 0);
    assert_eq!(collected.to_bytes(), b"01234789ab");
    assert_eq!(collected.omitted_bytes(), 2);
    assert_eq!(collected.take_complete_bytes(), Some(b"0123456789ab".to_vec()));
}

#[test]
fn chunk_larger_than_tail_budget_keeps_only_tail_end() {
    let mut buf = HeadTailBuffer::<10>::default();
    buf.push_chunk(b"0123456789");

    // Tail budget is 5 bytes. This chunk should replace the tail and keep only its last 5 bytes.
    buf.push_chunk(b"ABCDEFGHIJK");

    let out = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(out.starts_with("01234"));
    assert!(out.ends_with("GHIJK"));
    assert!(buf.omitted_bytes() > 0);
}

#[test]
fn fills_head_then_tail_across_multiple_chunks() {
    let mut buf = HeadTailBuffer::<10>::default();

    // Fill the 5-byte head budget across multiple chunks.
    buf.push_chunk(b"01");
    buf.push_chunk(b"234");
    assert_eq!(buf.to_bytes(), b"01234");

    // Then fill the 5-byte tail budget.
    buf.push_chunk(b"567");
    buf.push_chunk(b"89");
    assert_eq!(buf.to_bytes(), b"0123456789");
    assert_eq!(buf.omitted_bytes(), 0);

    // One more byte causes the tail to drop its oldest byte.
    buf.push_chunk(b"a");
    assert_eq!(buf.to_bytes(), b"012346789a");
    assert_eq!(buf.omitted_bytes(), 1);
}

#[test]
fn empty_and_tiny_chunks_have_bounded_metadata() {
    let mut buf = HeadTailBuffer::<10>::default();

    for byte in b"0123456789ab" {
        buf.push_chunk(&[]);
        buf.push_chunk(&[*byte]);
    }

    assert_eq!(buf.retained_bytes(), 10);
    assert_eq!(buf.omitted_bytes(), 2);
}
