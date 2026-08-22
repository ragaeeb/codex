"""Shared synthetic transcript builders for offline savings-harness tests."""

import hashlib
import json
import shlex
import sqlite3
import sys
from pathlib import Path

from tool_output_savings_evidence import ThreadRow
from tool_output_savings_fixture import build_fixture
from tool_output_savings_fixture import prompt


THREAD_ID = "00000000-0000-0000-0000-000000000001"


def token_info(total: int) -> dict[str, dict[str, int]]:
    return {
        "total_token_usage": {
            "input_tokens": 60,
            "cached_input_tokens": 10,
            "cache_write_input_tokens": 0,
            "output_tokens": 20,
            "reasoning_output_tokens": 10,
            "total_tokens": total,
        },
        "last_token_usage": {
            "input_tokens": 1,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 1,
            "reasoning_output_tokens": 0,
            "total_tokens": 2,
        },
    }


def make_db(path: Path, target: str, rollout_path: str, tokens: int = 100) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, tokens_used INTEGER NOT NULL, model TEXT, reasoning_effort TEXT, cli_version TEXT, updated_at INTEGER, updated_at_ms INTEGER)"
        )
        connection.execute(
            "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            ("unrelated", "/unrelated.jsonl", 999, "other", "low", "other", 1, 1000),
        )
        connection.execute(
            "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (target, rollout_path, tokens, "gpt-5.6-luna", "medium", "1.2.3", 1, 1),
        )
        connection.commit()
    finally:
        connection.close()


def _call(call_id: str, name: str, arguments: dict[str, object]) -> dict[str, object]:
    return {
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": json.dumps(
                arguments, ensure_ascii=False, separators=(",", ":")
            ),
        },
    }


def _output(call_id: str, value: object) -> dict[str, object]:
    return {
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": json.dumps(value, ensure_ascii=False, separators=(",", ":")),
        },
    }


def synthetic_case(
    root: Path,
) -> tuple[
    object,
    Path,
    list[dict[str, object]],
    list[dict[str, object]],
    ThreadRow,
    dict[str, int],
]:
    fixture_root = root / "fixture"
    fixture_root.mkdir()
    fixture = build_fixture(fixture_root, python_executable=sys.executable)
    home = root / "home"
    scope = home / "tool_outputs" / THREAD_ID
    scope.mkdir(parents=True)
    (home / "tool_outputs").chmod(0o700)
    scope.chmod(0o700)
    text = fixture.window_bytes.decode("utf-8")[:5_500]
    inline = {
        "type": "file_read",
        "version": 1,
        "path": fixture.window_name,
        "fingerprint": {
            "size_bytes": len(fixture.window_bytes),
            "modified_at_ms": fixture.window_modified_at_ms,
            "window_digest": f"sha256:{hashlib.sha256(text.encode()).hexdigest()}",
        },
        "window": {
            "start_byte": 0,
            "end_byte": len(text.encode()),
            "text": text,
            "line_fragments": text.count("\n") + 1,
            "line_continues": True,
        },
        "next_offset": len(text.encode()),
        "eof": False,
        "continuation": "Call read_file again with offset=next_offset.",
    }
    inline_raw = json.dumps(inline, ensure_ascii=False, separators=(",", ":"))
    stage1_data = fixture.emitter_output_bytes
    stage1_id = "out_" + hashlib.sha256(stage1_data).hexdigest()
    stage2_id = "out_" + hashlib.sha256(inline_raw.encode()).hexdigest()
    stage2_data = inline_raw.encode()
    stage1_offset = stage1_data.find(fixture.shell_middle.encode())
    stage2_offset = stage2_data.find(fixture.window_middle.encode())
    assert stage1_offset >= 0 and stage2_offset >= 0
    (scope / f"{stage1_id}.txt").write_bytes(stage1_data)
    (scope / f"{stage2_id}.txt").write_bytes(stage2_data)
    (scope / f"{stage1_id}.txt").chmod(0o600)
    (scope / f"{stage2_id}.txt").chmod(0o600)

    def byte_window(data: bytes, offset: int) -> tuple[int, str]:
        requested = data[offset : offset + 256]
        try:
            text = requested.decode("utf-8")
        except UnicodeDecodeError as error:
            assert error.reason == "unexpected end of data"
            requested = requested[: error.start]
            text = requested.decode("utf-8")
        return offset + len(requested), text

    stage1_end, stage1_text = byte_window(stage1_data, stage1_offset)
    stage2_end, stage2_text = byte_window(stage2_data, stage2_offset)

    def preview(data: bytes) -> tuple[str, str]:
        head_end = 384
        while head_end < len(data) and head_end > 0 and (data[head_end] & 0xC0) == 0x80:
            head_end -= 1
        tail_start = max(0, len(data) - 384)
        while tail_start < len(data) and (data[tail_start] & 0xC0) == 0x80:
            tail_start += 1
        head = data[:head_end].decode("utf-8")
        tail = data[tail_start:].decode("utf-8")
        return head, tail

    stage1_head, stage1_tail = preview(stage1_data)
    stage2_head, stage2_tail = preview(stage2_data)
    stage1_envelope = {
        "type": "tool_output_artifact",
        "version": 1,
        "artifact_id": stage1_id,
        "content_type": "text/plain",
        "original_bytes": len(stage1_data),
        "original_lines": stage1_data.count(b"\n"),
        "approximate_tokens": (len(stage1_data) + 3) // 4,
        "digest": f"sha256:{stage1_id[4:]}",
        "preview": {"head": stage1_head, "tail": stage1_tail},
        "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
    }
    stage2_envelope = {
        "type": "tool_output_artifact",
        "version": 1,
        "artifact_id": stage2_id,
        "content_type": "application/vnd.codex.file-read+json",
        "original_bytes": len(stage2_data),
        "original_lines": 1,
        "approximate_tokens": (len(stage2_data) + 3) // 4,
        "digest": f"sha256:{stage2_id[4:]}",
        "preview": {"head": stage2_head, "tail": stage2_tail},
        "retrieval": "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
    }
    expected_read = {
        "path": fixture.window_name,
        "offset": 0,
        "max_bytes": 32_768,
        "max_lines": 2_000,
    }
    records: list[dict[str, object]] = [
        _call(
            "exec",
            "exec_command",
            {
                "cmd": shlex.join([fixture.python_executable, fixture.emitter_name]),
                "max_output_tokens": 1_000,
            },
        ),
        _call("read-a", "read_file", expected_read),
        _call("read-b", "read_file", expected_read),
        _call(
            "s1-search",
            "read_tool_output",
            {
                "artifact_id": stage1_id,
                "mode": "search",
                "query": fixture.shell_middle,
                "limit": 4,
            },
        ),
        _call(
            "s1-bytes",
            "read_tool_output",
            {
                "artifact_id": stage1_id,
                "mode": "bytes",
                "offset": stage1_offset,
                "limit": 256,
            },
        ),
        _call(
            "s2-search",
            "read_tool_output",
            {
                "artifact_id": stage2_id,
                "mode": "search",
                "query": fixture.window_middle,
                "limit": 4,
            },
        ),
        _call(
            "s2-bytes",
            "read_tool_output",
            {
                "artifact_id": stage2_id,
                "mode": "bytes",
                "offset": stage2_offset,
                "limit": 256,
            },
        ),
        _output("exec", stage1_envelope),
        _output("read-b", stage2_envelope),
        _output("read-a", inline),
        _output(
            "s1-search",
            {
                "type": "tool_output_artifact_search",
                "artifact_id": stage1_id,
                "byte_offsets": [stage1_offset],
                "next_offset": None,
                "complete": True,
            },
        ),
        _output(
            "s1-bytes",
            {
                "type": "tool_output_artifact_window",
                "mode": "bytes",
                "artifact_id": stage1_id,
                "start_byte": stage1_offset,
                "end_byte": stage1_end,
                "text": stage1_text,
                "next_offset": stage1_end,
                "complete": False,
            },
        ),
        _output(
            "s2-search",
            {
                "type": "tool_output_artifact_search",
                "artifact_id": stage2_id,
                "byte_offsets": [stage2_offset],
                "next_offset": None,
                "complete": True,
            },
        ),
        _output(
            "s2-bytes",
            {
                "type": "tool_output_artifact_window",
                "mode": "bytes",
                "artifact_id": stage2_id,
                "start_byte": stage2_offset,
                "end_byte": stage2_end,
                "text": stage2_text,
                "next_offset": stage2_end,
                "complete": False,
            },
        ),
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": prompt(fixture),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "TOOL_OUTPUT_SAVINGS_E2E_SUCCESS"}
                ],
            },
        },
    ]
    calls_by_id = {
        record["payload"]["call_id"]: record
        for record in records[:7]
        if isinstance(record.get("payload"), dict)
    }
    outputs_by_id = {
        record["payload"]["call_id"]: record
        for record in records[7:14]
        if isinstance(record.get("payload"), dict)
    }
    records = [
        {
            "type": "session_meta",
            "payload": {"id": THREAD_ID, "source": "synthetic-test"},
        },
        {
            "type": "event_msg",
            "payload": {"type": "turn_context", "metadata": {"bounded": True}},
        },
        calls_by_id["exec"],
        outputs_by_id["exec"],
        calls_by_id["s1-search"],
        outputs_by_id["s1-search"],
        calls_by_id["s1-bytes"],
        outputs_by_id["s1-bytes"],
        calls_by_id["read-a"],
        outputs_by_id["read-a"],
        calls_by_id["read-b"],
        outputs_by_id["read-b"],
        calls_by_id["s2-search"],
        outputs_by_id["s2-search"],
        calls_by_id["s2-bytes"],
        outputs_by_id["s2-bytes"],
        {
            "type": "compacted",
            "payload": {"message": "bounded synthetic compaction metadata"},
        },
        {
            "type": "replacement_history",
            "payload": {"items": [{"type": "metadata", "value": "bounded"}]},
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": prompt(fixture),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "TOOL_OUTPUT_SAVINGS_E2E_SUCCESS"}
                ],
            },
        },
    ]
    usage = {
        "input_tokens": 60,
        "cached_input_tokens": 10,
        "cache_write_input_tokens": 0,
        "output_tokens": 20,
        "reasoning_output_tokens": 10,
    }
    events = [
        {"type": "thread.started", "thread_id": THREAD_ID},
        {"type": "turn.completed", "usage": usage},
    ]
    records.insert(
        1,
        {
            "type": "event_msg",
            "payload": {"type": "token_count", "info": token_info(100)},
        },
    )
    row = ThreadRow(
        THREAD_ID,
        str(root / "rollout.jsonl"),
        100,
        "gpt-5.6-luna",
        "medium",
        "1.2.3",
        1,
        1,
    )
    rollout_text = "".join(
        json.dumps(record, ensure_ascii=False) + "\n" for record in records
    )
    (root / "rollout.jsonl").write_text(rollout_text, encoding="utf-8")
    return fixture, home, events, records, row, usage
