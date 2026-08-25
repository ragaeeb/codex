"""Top-level retrieval evidence validation for the Luna Code Mode lane."""

import json
from typing import Any

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_projection import require_retrievals
from tool_output_savings_transcript import ToolCall
from tool_output_savings_transcript import ToolOutput
from tool_output_savings_transcript import response_items


def _decode_arguments(item: dict[str, Any]) -> dict[str, Any]:
    raw = item.get("arguments", item.get("input"))
    try:
        value = json.loads(raw) if isinstance(raw, str) else raw
    except json.JSONDecodeError as error:
        raise HarnessError("malformed_tool_call_arguments") from error
    require(isinstance(value, dict), "malformed_tool_call_arguments")
    return value


def _decode_output(item: dict[str, Any]) -> tuple[str, Any]:
    raw_value = item.get("output")
    if isinstance(raw_value, str):
        try:
            return raw_value, json.loads(raw_value)
        except json.JSONDecodeError as error:
            raise HarnessError("malformed_tool_output") from error
    try:
        return (
            json.dumps(raw_value, ensure_ascii=False, separators=(",", ":")),
            raw_value,
        )
    except (TypeError, ValueError) as error:
        raise HarnessError("malformed_tool_output") from error


def require_top_level_retrievals(
    records: list[dict[str, Any]],
    *,
    exec_call_id: str,
    exec_call_position: int,
    exec_output_position: int,
    artifact_id: str,
    marker: str,
    artifact_data: bytes,
) -> None:
    calls: list[ToolCall] = []
    outputs: list[ToolOutput] = []
    call_positions: dict[str, int] = {}
    output_positions: dict[str, int] = {}
    assistant_positions: list[int] = []
    seen_call_ids = {exec_call_id}
    seen_output_ids = {exec_call_id}
    for position, item in enumerate(response_items(records)):
        response_type = item.get("type")
        call_id = item.get("call_id")
        if response_type in {"function_call", "custom_tool_call"}:
            if call_id == exec_call_id:
                continue
            require(
                isinstance(call_id, str)
                and bool(call_id)
                and call_id not in seen_call_ids,
                "duplicate_or_malformed_tool_call_id",
            )
            require(item.get("name") == "read_tool_output", "unexpected_tool_family")
            seen_call_ids.add(call_id)
            call_positions[call_id] = position
            calls.append(ToolCall("read_tool_output", call_id, _decode_arguments(item)))
        elif response_type in {"function_call_output", "custom_tool_call_output"}:
            if call_id == exec_call_id:
                continue
            require(
                isinstance(call_id, str)
                and bool(call_id)
                and call_id not in seen_output_ids,
                "duplicate_or_malformed_tool_output_id",
            )
            seen_output_ids.add(call_id)
            output_positions[call_id] = position
            raw, value = _decode_output(item)
            outputs.append(ToolOutput(call_id, "read_tool_output", raw, value))
        elif response_type == "message" and item.get("role") == "assistant":
            assistant_positions.append(position)
    require(len(calls) == 2 and len(outputs) == 2, "retrieval_call_count")
    require(
        {call.call_id for call in calls} == {output.call_id for output in outputs},
        "retrieval_call_output_association",
    )
    search_id, bytes_id = require_retrievals(
        calls,
        outputs,
        artifact_id,
        marker,
        artifact_data,
    )
    require(len(assistant_positions) == 1, "assistant_message_count")
    ordered = [
        exec_call_position,
        exec_output_position,
        call_positions[search_id],
        output_positions[search_id],
        call_positions[bytes_id],
        output_positions[bytes_id],
        assistant_positions[0],
    ]
    require(
        all(left < right for left, right in zip(ordered, ordered[1:])),
        "code_mode_retrieval_order",
    )
