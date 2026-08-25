"""Decoded response-item validation and tool transcript accounting."""

import json
from dataclasses import dataclass
from typing import Any, Iterable

from tool_output_savings_evidence import MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES
from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_evidence import MAX_ROLLOUT_BYTES
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require


@dataclass(frozen=True)
class ToolCall:
    name: str
    call_id: str
    arguments: dict[str, Any]


@dataclass(frozen=True)
class ToolOutput:
    call_id: str
    name: str
    raw: str
    value: Any


SUPPORTED_TOOL_RESPONSE_TYPES = {
    "function_call",
    "custom_tool_call",
    "function_call_output",
    "custom_tool_call_output",
}


def response_items(records: Iterable[dict[str, Any]]) -> Iterable[dict[str, Any]]:
    for record in records:
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        require(isinstance(payload, dict), "response_item_payload_invalid")
        yield payload


def _looks_tool_like(response_type: Any) -> bool:
    if not isinstance(response_type, str):
        return False
    return (
        response_type in SUPPORTED_TOOL_RESPONSE_TYPES
        or response_type.endswith("_call")
        or response_type.endswith("_call_output")
        or response_type.startswith(("tool_", "web_", "image_", "computer_"))
        or response_type.startswith("local_shell_")
    )


def _item_is_tool_like(item: dict[str, Any]) -> bool:
    return _looks_tool_like(item.get("type")) or (
        item.get("type") == "message" and item.get("role") == "tool"
    )


def _typed_tool_items(value: Any) -> Iterable[dict[str, Any]]:
    """Yield every typed tool-like object in the decoded persisted tree."""
    if isinstance(value, dict):
        if _item_is_tool_like(value):
            yield value
        for child in value.values():
            yield from _typed_tool_items(child)
    elif isinstance(value, list):
        for child in value:
            yield from _typed_tool_items(child)


def validate_tool_response_items(records: Iterable[dict[str, Any]]) -> None:
    """Fail closed on every tool-like response item before semantic filtering."""
    records = list(records)
    authoritative_items = {
        id(item) for item in response_items(records) if _item_is_tool_like(item)
    }
    aggregate_bytes = 0
    for item in _typed_tool_items(records):
        require(
            id(item) in authoritative_items,
            "tool_response_item_not_authoritative",
        )
        response_type = item.get("type")
        require(
            isinstance(response_type, str) and response_type,
            "response_item_type_invalid",
        )
        if response_type in {"function_call_output", "custom_tool_call_output"}:
            raw_value = item.get("output")
            try:
                output_bytes = len(
                    raw_value.encode("utf-8")
                    if isinstance(raw_value, str)
                    else json.dumps(
                        raw_value, ensure_ascii=False, separators=(",", ":")
                    ).encode("utf-8")
                )
            except (TypeError, ValueError) as error:
                raise HarnessError("malformed_tool_output") from error
            require(output_bytes <= MAX_MODEL_VISIBLE_BYTES, "function_output_over_cap")
            aggregate_bytes += output_bytes
            require(
                aggregate_bytes <= MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES,
                "function_output_aggregate_over_cap",
            )
            continue
        try:
            item_bytes = len(
                json.dumps(item, ensure_ascii=False, separators=(",", ":")).encode()
            )
        except (TypeError, ValueError) as error:
            raise HarnessError("malformed_tool_response_item") from error
        require(item_bytes <= MAX_MODEL_VISIBLE_BYTES, "tool_response_item_over_cap")
        aggregate_bytes += item_bytes
        require(
            aggregate_bytes <= MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES,
            "tool_response_aggregate_over_cap",
        )
        require(
            response_type in SUPPORTED_TOOL_RESPONSE_TYPES,
            "unexpected_tool_like_response_item",
        )


def collect_tool_calls(records: Iterable[dict[str, Any]]) -> list[ToolCall]:
    records = list(records)
    validate_tool_response_items(records)
    calls: list[ToolCall] = []
    seen_call_ids: set[str] = set()
    for item in response_items(records):
        if item.get("type") not in {"function_call", "custom_tool_call"}:
            continue
        name, call_id = item.get("name"), item.get("call_id")
        raw = item.get("arguments", item.get("input"))
        require(isinstance(name, str) and name, "malformed_tool_call")
        require(isinstance(call_id, str) and call_id, "malformed_tool_call")
        require(call_id not in seen_call_ids, "duplicate_tool_call_id")
        seen_call_ids.add(call_id)
        try:
            arguments = json.loads(raw) if isinstance(raw, str) else raw
        except json.JSONDecodeError:
            raise HarnessError("malformed_tool_call_arguments")
        require(isinstance(arguments, dict), "malformed_tool_call_arguments")
        calls.append(ToolCall(name, call_id, arguments))
    return calls


def collect_tool_outputs(records: Iterable[dict[str, Any]]) -> list[ToolOutput]:
    records = list(records)
    validate_tool_response_items(records)
    calls = {call.call_id: call.name for call in collect_tool_calls(records)}
    outputs: list[ToolOutput] = []
    seen_output_ids: set[str] = set()
    aggregate_bytes = 0
    for item in response_items(records):
        if item.get("type") not in {
            "function_call_output",
            "custom_tool_call_output",
        }:
            continue
        require("output" in item, "malformed_tool_output")
        call_id, raw_value = item.get("call_id"), item.get("output")
        require(isinstance(call_id, str) and call_id, "malformed_tool_output")
        require(call_id not in seen_output_ids, "duplicate_tool_output_id")
        seen_output_ids.add(call_id)
        if isinstance(raw_value, str):
            raw = raw_value
            try:
                value = json.loads(raw_value)
            except json.JSONDecodeError:
                value = None
        else:
            try:
                raw = json.dumps(raw_value, ensure_ascii=False, separators=(",", ":"))
            except (TypeError, ValueError) as error:
                raise HarnessError("malformed_tool_output") from error
            value = raw_value
        output_bytes = len(raw.encode("utf-8"))
        require(output_bytes <= MAX_MODEL_VISIBLE_BYTES, "function_output_over_cap")
        aggregate_bytes += output_bytes
        require(
            aggregate_bytes <= MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES,
            "function_output_aggregate_over_cap",
        )
        require(call_id in calls, "orphan_tool_output")
        outputs.append(ToolOutput(call_id, calls[call_id], raw, value))
    return outputs


def tool_response_item_metrics(records: Iterable[dict[str, Any]]) -> dict[str, int]:
    """Measure complete serialized tool-like response items after validation."""
    aggregate = 0
    maximum = 0
    count = 0
    for item in _typed_tool_items(list(records)):
        size = len(
            json.dumps(item, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        )
        aggregate += size
        maximum = max(maximum, size)
        count += 1
    require(
        aggregate <= MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES,
        "tool_response_aggregate_over_cap",
    )
    return {
        "item_count": count,
        "aggregate_bytes": aggregate,
        "max_item_bytes": maximum,
    }


def complete_response_item_metrics(records: Iterable[dict[str, Any]]) -> dict[str, int]:
    """Measure every complete response item, including non-tool reasoning."""
    aggregate = 0
    maximum = 0
    count = 0
    for item in response_items(records):
        try:
            size = len(
                json.dumps(item, ensure_ascii=False, separators=(",", ":")).encode(
                    "utf-8"
                )
            )
        except (TypeError, ValueError) as error:
            raise HarnessError("response_item_unserializable") from error
        aggregate += size
        maximum = max(maximum, size)
        count += 1
        require(aggregate <= MAX_ROLLOUT_BYTES, "response_item_aggregate_over_cap")
    return {
        "item_count": count,
        "aggregate_bytes": aggregate,
        "max_item_bytes": maximum,
    }


def persisted_record_metrics(records: Iterable[dict[str, Any]]) -> dict[str, int]:
    """Measure the complete decoded persisted representation without logging it."""
    aggregate = 0
    maximum = 0
    count = 0
    for record in records:
        try:
            size = len(
                json.dumps(record, ensure_ascii=False, separators=(",", ":")).encode(
                    "utf-8"
                )
            )
        except (TypeError, ValueError) as error:
            raise HarnessError("persisted_record_unserializable") from error
        aggregate += size
        maximum = max(maximum, size)
        count += 1
        require(aggregate <= MAX_ROLLOUT_BYTES, "persisted_representation_over_cap")
    return {
        "record_count": count,
        "aggregate_bytes": aggregate,
        "max_record_bytes": maximum,
    }


def require_transcript_causality(
    records: list[dict[str, Any]], calls: list[ToolCall]
) -> None:
    """Require chronological call/output pairs and a final assistant response."""
    call_positions: dict[str, int] = {}
    output_positions: dict[str, int] = {}
    assistant_positions: list[int] = []
    for position, record in enumerate(records):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        require(isinstance(payload, dict), "response_item_payload_invalid")
        call_id = payload.get("call_id")
        if payload.get("type") in {"function_call", "custom_tool_call"}:
            require(isinstance(call_id, str), "malformed_tool_call")
            call_positions[call_id] = position
        elif payload.get("type") in {
            "function_call_output",
            "custom_tool_call_output",
        }:
            require(isinstance(call_id, str), "malformed_tool_output")
            output_positions[call_id] = position
        elif payload.get("type") == "message" and payload.get("role") == "assistant":
            assistant_positions.append(position)
    for call in calls:
        require(call.call_id in call_positions, "tool_call_not_in_rollout")
        require(call.call_id in output_positions, "tool_output_missing")
        require(
            call_positions[call.call_id] < output_positions[call.call_id],
            "tool_call_output_order",
        )
    require(assistant_positions, "success_sentinel_missing")
    require(len(assistant_positions) == 1, "assistant_message_count")
    require(
        max(output_positions.values(), default=-1) < assistant_positions[0],
        "success_sentinel_order",
    )


def require_exact_tool_sequence(
    records: list[dict[str, Any]], call_ids: list[str]
) -> None:
    """Require each checklist call/output pair to finish before the next begins."""
    positions: dict[tuple[str, str], int] = {}
    for position, record in enumerate(records):
        if record.get("type") != "response_item":
            continue
        item = record.get("payload")
        require(isinstance(item, dict), "response_item_payload_invalid")
        call_id = item.get("call_id")
        if not isinstance(call_id, str) or call_id not in call_ids:
            continue
        response_type = item.get("type")
        if response_type in {"function_call", "custom_tool_call"}:
            positions[(call_id, "call")] = position
        elif response_type in {
            "function_call_output",
            "custom_tool_call_output",
        }:
            positions[(call_id, "output")] = position
    ordered_positions: list[int] = []
    for call_id in call_ids:
        call_position = positions.get((call_id, "call"))
        output_position = positions.get((call_id, "output"))
        require(
            call_position is not None and output_position is not None,
            "tool_checklist_order",
        )
        ordered_positions.extend((call_position, output_position))
    require(
        all(
            left < right
            for left, right in zip(ordered_positions, ordered_positions[1:])
        ),
        "tool_checklist_order",
    )
