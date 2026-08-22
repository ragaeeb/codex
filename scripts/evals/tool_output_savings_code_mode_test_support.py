"""Synthetic Luna Code Mode rollout builder for offline harness tests."""

import json
from pathlib import Path
from typing import Any

from tool_output_savings_artifacts import list_thread_artifacts
from tool_output_savings_code_mode_lane import CODE_MODE_RESULT_TYPE
from tool_output_savings_code_mode_lane import SUCCESS_SENTINEL
from tool_output_savings_code_mode_lane import code_mode_program
from tool_output_savings_code_mode_lane import code_mode_prompt
from tool_output_savings_test_support import THREAD_ID
from tool_output_savings_test_support import synthetic_case
from tool_output_savings_test_support import token_info


def _payloads(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        record["payload"]
        for record in records
        if record.get("type") == "response_item"
        and isinstance(record.get("payload"), dict)
    ]


def synthetic_code_mode_case(root: Path) -> tuple[Any, Path, list, list, Any, dict]:
    fixture, home, events, direct_records, row, usage = synthetic_case(root)
    payloads = _payloads(direct_records)
    calls = {
        item["call_id"]: item
        for item in payloads
        if item.get("type") in {"function_call", "custom_tool_call"}
    }
    outputs = {
        item["call_id"]: json.loads(item["output"])
        for item in payloads
        if item.get("type") in {"function_call_output", "custom_tool_call_output"}
    }
    stage1 = outputs["exec"]
    artifact_id = stage1["artifact_id"]
    search_call = next(
        call
        for call in calls.values()
        if call["name"] == "read_tool_output"
        and json.loads(call["arguments"]).get("artifact_id") == artifact_id
        and json.loads(call["arguments"]).get("mode") == "search"
    )
    bytes_call = next(
        call
        for call in calls.values()
        if call["name"] == "read_tool_output"
        and json.loads(call["arguments"]).get("artifact_id") == artifact_id
        and json.loads(call["arguments"]).get("mode") == "bytes"
    )
    inline = outputs["read-a"]
    inline_raw = json.dumps(inline, ensure_ascii=False, separators=(",", ":"))
    summary = {
        "type": CODE_MODE_RESULT_TYPE,
        "version": 1,
        "sentinel": SUCCESS_SENTINEL,
        "stage1": {"envelope": stage1},
        "stage2": {
            "behavior": "inline_by_design",
            "first_type": inline["type"],
            "duplicate_type": inline["type"],
            "exact_equal": True,
            "path": str(fixture.root / fixture.window_name),
            "size_bytes": inline["fingerprint"]["size_bytes"],
            "modified_at_ms": inline["fingerprint"]["modified_at_ms"],
            "window_digest": inline["fingerprint"]["window_digest"],
            "start_byte": inline["window"]["start_byte"],
            "end_byte": inline["window"]["end_byte"],
            "window_bytes": len(inline["window"]["text"].encode("utf-8")),
            "marker_present": fixture.window_middle in inline["window"]["text"],
            "next_offset": inline["next_offset"],
            "eof": inline["eof"],
            "first_serialized_chars": len(inline_raw),
            "duplicate_serialized_chars": len(inline_raw),
        },
    }
    for managed_id in list_thread_artifacts(home, THREAD_ID):
        if managed_id != artifact_id:
            (home / "tool_outputs" / THREAD_ID / f"{managed_id}.txt").unlink()
    records = [
        {
            "type": "session_meta",
            "payload": {"id": THREAD_ID, "source": "synthetic-test"},
        },
        {
            "type": "event_msg",
            "payload": {"type": "token_count", "info": token_info(100)},
        },
        {
            "type": "event_msg",
            "payload": {"type": "turn_context", "metadata": {"bounded": True}},
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": code_mode_prompt(fixture)}],
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "code-mode-exec",
                "name": "exec",
                "input": code_mode_program(fixture),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "code-mode-exec",
                "output": [
                    {
                        "type": "input_text",
                        "text": "Script completed\nWall time 0.1 seconds\nOutput:\n",
                    },
                    {
                        "type": "input_text",
                        "text": json.dumps(
                            summary, ensure_ascii=False, separators=(",", ":")
                        ),
                    },
                ],
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "stage1-search",
                "name": "read_tool_output",
                "arguments": json.dumps(
                    {
                        "artifact_id": artifact_id,
                        "mode": "search",
                        "query": fixture.shell_middle,
                        "limit": 4,
                    },
                    separators=(",", ":"),
                ),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "stage1-search",
                "output": json.dumps(
                    outputs[search_call["call_id"]], separators=(",", ":")
                ),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "stage1-bytes",
                "name": "read_tool_output",
                "arguments": json.dumps(
                    {
                        "artifact_id": artifact_id,
                        "mode": "bytes",
                        "offset": outputs[search_call["call_id"]]["byte_offsets"][0],
                        "limit": 256,
                    },
                    separators=(",", ":"),
                ),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "stage1-bytes",
                "output": json.dumps(
                    outputs[bytes_call["call_id"]],
                    ensure_ascii=False,
                    separators=(",", ":"),
                ),
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": SUCCESS_SENTINEL}],
            },
        },
    ]
    return fixture, home, events, records, row, usage
