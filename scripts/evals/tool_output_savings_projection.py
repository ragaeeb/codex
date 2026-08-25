"""Exact bounded retrieval and semantic projection checks."""

import base64
import binascii
import hashlib
import json
from collections.abc import Iterable
from typing import Any

from tool_output_savings_artifacts import validate_artifact_id
from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_transcript import ToolCall
from tool_output_savings_transcript import ToolOutput


def _response_item_positions(
    records: Iterable[dict[str, Any]],
) -> Iterable[tuple[int, dict[str, Any]]]:
    for position, record in enumerate(records):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        require(isinstance(payload, dict), "response_item_payload_invalid")
        yield position, payload


def _output_contains_artifact(raw: str, artifact_id: str) -> bool:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return False
    return (
        isinstance(value, dict)
        and value.get("type") == "tool_output_artifact"
        and value.get("artifact_id") == artifact_id
    )


def _strict_arguments(
    actual: dict[str, Any], expected: dict[str, Any], rule: str
) -> None:
    require(set(actual) == set(expected), rule)
    for key, expected_value in expected.items():
        actual_value = actual[key]
        require(type(actual_value) is type(expected_value), rule)
        require(actual_value == expected_value, rule)


def require_retrievals(
    calls: list[ToolCall],
    outputs: list[ToolOutput],
    artifact_id: str,
    marker: str,
    artifact_data: bytes,
    *,
    records: list[dict[str, Any]] | None = None,
    producer_call_id: str | None = None,
) -> tuple[str, str]:
    selected = [
        call for call in calls if call.arguments.get("artifact_id") == artifact_id
    ]
    require(len(selected) == 2, "retrieval_call_count")
    require(
        sum(call.arguments.get("mode") == "search" for call in selected) == 1,
        "retrieval_search_count",
    )
    require(
        sum(call.arguments.get("mode") == "bytes" for call in selected) == 1,
        "retrieval_window_count",
    )
    search = next(call for call in selected if call.arguments.get("mode") == "search")
    window = next(call for call in selected if call.arguments.get("mode") == "bytes")
    expected_offset = artifact_data.find(marker.encode("utf-8"))
    require(expected_offset >= 0, "retrieval_marker_not_in_artifact")
    _strict_arguments(
        search.arguments,
        {
            "artifact_id": artifact_id,
            "mode": "search",
            "query": marker,
            "limit": 4,
        },
        "retrieval_search_arguments_invalid",
    )
    require(
        set(window.arguments) == {"artifact_id", "mode", "offset", "limit"},
        "retrieval_window_arguments_invalid",
    )
    require(
        window.arguments["artifact_id"] == artifact_id
        and type(window.arguments["artifact_id"]) is str
        and window.arguments["mode"] == "bytes"
        and type(window.arguments["mode"]) is str,
        "retrieval_window_arguments_invalid",
    )
    require(type(window.arguments["offset"]) is int, "retrieval_window_offset_missing")
    require(
        window.arguments["offset"] == expected_offset,
        "retrieval_window_offset_mismatch",
    )
    require(type(window.arguments["limit"]) is int, "retrieval_window_limit_invalid")
    require(window.arguments["limit"] == 256, "retrieval_window_limit_invalid")
    selected_ids = {call.call_id for call in selected}
    selected_outputs = [output for output in outputs if output.call_id in selected_ids]
    require(len(selected_outputs) == 2, "retrieval_output_count")
    by_output_id = {output.call_id: output for output in selected_outputs}
    search_output = by_output_id.get(search.call_id)
    window_output = by_output_id.get(window.call_id)
    require(search_output is not None, "retrieval_search_output_missing")
    require(window_output is not None, "retrieval_window_output_missing")

    if records is not None:
        call_positions: dict[str, int] = {}
        output_positions: dict[str, int] = {}
        envelope_positions: list[int] = []
        for position, item in _response_item_positions(records):
            call_id = item.get("call_id")
            if not isinstance(call_id, str):
                continue
            if item.get("type") in {"function_call", "custom_tool_call"}:
                call_positions[call_id] = position
            elif item.get("type") in {
                "function_call_output",
                "custom_tool_call_output",
            }:
                output_positions[call_id] = position
                raw = item.get("output")
                if isinstance(raw, str) and _output_contains_artifact(raw, artifact_id):
                    envelope_positions.append(position)
        require(envelope_positions, "retrieval_without_artifact_envelope")
        if producer_call_id is not None:
            producer_output_position = output_positions.get(producer_call_id)
            require(
                producer_output_position is not None
                and producer_output_position in envelope_positions,
                "retrieval_producer_not_envelope",
            )
            envelope_positions = [producer_output_position]
        search_call_position = call_positions.get(search.call_id)
        search_output_position = output_positions.get(search.call_id)
        window_call_position = call_positions.get(window.call_id)
        window_output_position = output_positions.get(window.call_id)
        require(
            search_call_position is not None
            and search_output_position is not None
            and window_call_position is not None
            and window_output_position is not None,
            "retrieval_causality_missing",
        )
        require(
            min(envelope_positions)
            < search_call_position
            < search_output_position
            < window_call_position
            < window_output_position,
            "retrieval_causality_invalid",
        )

    for output in (search_output, window_output):
        require(
            len(output.raw.encode("utf-8")) <= MAX_MODEL_VISIBLE_BYTES,
            "retrieval_output_over_cap",
        )
        require(isinstance(output.value, dict), "retrieval_output_not_json")
        require(
            validate_artifact_id(output.value.get("artifact_id")) == artifact_id,
            "retrieval_artifact_id_mismatch",
        )

    search_value = search_output.value
    require(
        set(search_value)
        == {"type", "artifact_id", "byte_offsets", "next_offset", "complete"},
        "retrieval_search_shape_invalid",
    )
    require(
        search_value.get("type") == "tool_output_artifact_search",
        "retrieval_search_output_wrong_type",
    )
    offsets = search_value.get("byte_offsets")
    require(
        isinstance(offsets, list)
        and all(type(offset) is int and offset >= 0 for offset in offsets),
        "retrieval_search_offsets_invalid",
    )
    require(offsets == [expected_offset], "retrieval_match_offsets_not_exact")
    require(
        search_value.get("next_offset") is None,
        "retrieval_search_continuation_invalid",
    )
    require(search_value.get("complete") is True, "retrieval_search_incomplete")

    window_value = window_output.value
    require(
        window_value.get("type") == "tool_output_artifact_window",
        "retrieval_window_output_wrong_type",
    )
    require(window_value.get("mode") == "bytes", "retrieval_window_mode_invalid")
    has_text = "text" in window_value
    has_base64 = "bytes_base64" in window_value
    require(has_text != has_base64, "retrieval_window_encoding_ambiguous")
    if has_text:
        require(
            set(window_value)
            == {
                "type",
                "mode",
                "artifact_id",
                "start_byte",
                "end_byte",
                "text",
                "next_offset",
                "complete",
            },
            "retrieval_window_shape_invalid",
        )
        require(
            isinstance(window_value["text"], str),
            "retrieval_window_encoding_invalid",
        )
    else:
        require(
            set(window_value)
            == {
                "type",
                "mode",
                "artifact_id",
                "encoding",
                "start_byte",
                "end_byte",
                "bytes_base64",
                "next_offset",
                "complete",
            }
            and window_value.get("encoding") == "base64"
            and isinstance(window_value["bytes_base64"], str),
            "retrieval_window_shape_invalid",
        )
    start = window_value.get("start_byte")
    end = window_value.get("end_byte")
    require(
        type(start) is int
        and type(end) is int
        and start == expected_offset
        and start < end,
        "retrieval_window_offset_mismatch",
    )
    require(
        offsets == [start],
        "retrieval_search_window_offset_mismatch",
    )
    require(end <= len(artifact_data), "retrieval_window_end_invalid")
    require(end - start <= 256, "retrieval_window_over_requested_limit")
    if has_text:
        recovered = window_value["text"].encode("utf-8")
    else:
        try:
            recovered = base64.b64decode(window_value["bytes_base64"], validate=True)
        except (ValueError, binascii.Error) as error:
            raise HarnessError("retrieval_window_encoding_invalid") from error
    require(recovered == artifact_data[start:end], "retrieval_window_bytes_mismatch")
    requested = artifact_data[
        start : min(start + window.arguments["limit"], len(artifact_data))
    ]
    try:
        requested.decode("utf-8")
    except UnicodeDecodeError as error:
        require(
            error.reason == "unexpected end of data" and error.end == len(requested),
            "retrieval_window_encoding_invalid",
        )
        requested = requested[: error.start]
    require(recovered == requested, "retrieval_window_not_maximal")
    require(end == start + len(requested), "retrieval_window_not_maximal")
    require(marker.encode("utf-8") in recovered, "retrieval_marker_unrecovered")
    expected_next = end if end < len(artifact_data) else None
    require(
        window_value.get("next_offset") == expected_next,
        "retrieval_continuation_invalid",
    )
    require(
        window_value.get("complete") is (expected_next is None),
        "retrieval_complete_flag_invalid",
    )
    return search.call_id, window.call_id


def _line_count(data: bytes) -> int:
    return data.count(b"\n") + int(bool(data) and not data.endswith(b"\n"))


def require_artifact_envelope(
    value: Any,
    artifact_id: str,
    artifact_data: bytes,
    content_type: str,
    marker: str,
) -> None:
    require(isinstance(value, dict), "artifact_envelope_invalid")
    require(value.get("type") == "tool_output_artifact", "artifact_envelope_type")
    require(value.get("version") == 1, "artifact_envelope_version")
    require(
        set(value)
        == {
            "type",
            "version",
            "artifact_id",
            "content_type",
            "original_bytes",
            "original_lines",
            "approximate_tokens",
            "digest",
            "preview",
            "retrieval",
        },
        "artifact_envelope_shape",
    )
    require(value.get("artifact_id") == artifact_id, "artifact_envelope_id")
    require(
        value.get("content_type") == content_type,
        "artifact_envelope_content_type",
    )
    for key in ("original_bytes", "original_lines", "approximate_tokens"):
        require(
            type(value.get(key)) is int and value[key] >= 0, "artifact_metadata_invalid"
        )
    require(
        value.get("original_bytes") == len(artifact_data), "artifact_original_bytes"
    )
    require(
        value.get("original_lines") == _line_count(artifact_data),
        "artifact_original_lines",
    )
    require(
        value.get("approximate_tokens") == (len(artifact_data) + 3) // 4,
        "artifact_approximate_tokens",
    )
    require(
        value.get("digest") == f"sha256:{hashlib.sha256(artifact_data).hexdigest()}",
        "artifact_envelope_digest",
    )
    preview = value.get("preview")
    require(
        isinstance(preview, dict)
        and set(preview) == {"head", "tail"}
        and isinstance(preview.get("head"), str)
        and isinstance(preview.get("tail"), str)
        and bool(preview.get("head"))
        and bool(preview.get("tail")),
        "artifact_preview_invalid",
    )
    try:
        decoded = artifact_data.decode("utf-8")
        head = preview["head"].encode("utf-8")
        tail = preview["tail"].encode("utf-8")
    except UnicodeDecodeError as error:
        raise HarnessError("artifact_preview_not_text") from error
    require(len(head) <= 384 and len(tail) <= 384, "artifact_preview_over_cap")
    head_boundary = artifact_data[:384].decode("utf-8", errors="ignore")
    tail_boundary = artifact_data[-384:].decode("utf-8", errors="ignore")
    require(head == head_boundary.encode("utf-8"), "artifact_preview_head_mismatch")
    require(tail == tail_boundary.encode("utf-8"), "artifact_preview_tail_mismatch")
    require(artifact_data.startswith(head), "artifact_preview_head_mismatch")
    require(artifact_data.endswith(tail), "artifact_preview_tail_mismatch")
    retrieval = value.get("retrieval")
    require(
        isinstance(retrieval, str)
        and retrieval
        == "Use read_tool_output with artifact_id and mode bytes, lines, or search; follow next_offset or next_byte to continue.",
        "artifact_retrieval_instruction_invalid",
    )
    require(
        marker not in preview["head"] and marker not in preview["tail"],
        "artifact_marker_in_preview",
    )
    require(
        decoded.startswith(preview["head"]) and decoded.endswith(preview["tail"]),
        "artifact_preview_boundary",
    )
