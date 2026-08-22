"""Hard context-item limits and fixture-aware persisted-content accounting."""

import base64
import json
import re
from dataclasses import dataclass
from typing import Any, Iterable

from tool_output_savings_content import contains_across_strings
from tool_output_savings_content import base64_decoded_strings
from tool_output_savings_content import count_across_strings
from tool_output_savings_content import count_response_content_occurrences
from tool_output_savings_content import MAX_JSON_DECODE_DEPTH
from tool_output_savings_content import raw_response_content_chunks
from tool_output_savings_content import raw_response_content_strings
from tool_output_savings_content import response_content_chunks
from tool_output_savings_content import response_content_strings
from tool_output_savings_evidence import MAX_ROLLOUT_BYTES
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_fixture import prompt


CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN = 4
MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS = 10_000
MAX_EFFECTIVE_CONTEXT_ITEM_BYTES = (
    MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS * CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN
)
STAGE1_FIXTURE_CONTENT_BUDGET_BYTES = 16 * 1024
STAGE2_FIXTURE_CONTENT_OVERHEAD_BYTES = 2 * 1024
EXEMPT_RESPONSE_ITEM_TYPES = {
    "reasoning",
    "compaction",
    "compaction_summary",
    "context_compaction",
}
EFFECTIVE_HISTORY_RECORD_TYPES = {"compacted", "replacement_history"}
_STAGE1_LINE = re.compile(
    r"(?:emitter-line-[0-9]{5}-|SAVINGS_SHELL_(?:HEAD|MIDDLE|TAIL)_[0-9a-f]{16})"
)
_STAGE2_LINE = re.compile(r"(?:line-[0-9]{5} payload|SAVINGS_FILE_MIDDLE_[0-9a-f]{16})")


@dataclass(frozen=True)
class ContextBudgetMetrics:
    bounded_item_count: int
    bounded_item_bytes: int
    max_bounded_item_bytes: int
    exempt_item_count: int
    exempt_item_bytes: int
    max_exempt_item_bytes: int


@dataclass(frozen=True)
class FixtureContentMetrics:
    stage1_bytes: int
    stage1_budget_bytes: int
    stage2_bytes: int
    stage2_budget_bytes: int


class _ContextBudgetAccumulator:
    def __init__(self) -> None:
        self.bounded_sizes: list[int] = []
        self.exempt_sizes: list[int] = []

    def add_bounded(self, value: Any) -> None:
        size = _serialized_size(value)
        require(size <= MAX_EFFECTIVE_CONTEXT_ITEM_BYTES, "context_item_over_cap")
        self.bounded_sizes.append(size)

    def add_exempt(self, value: Any) -> None:
        self.exempt_sizes.append(_serialized_size(value))

    def metrics(self) -> ContextBudgetMetrics:
        return ContextBudgetMetrics(
            bounded_item_count=len(self.bounded_sizes),
            bounded_item_bytes=sum(self.bounded_sizes),
            max_bounded_item_bytes=max(self.bounded_sizes, default=0),
            exempt_item_count=len(self.exempt_sizes),
            exempt_item_bytes=sum(self.exempt_sizes),
            max_exempt_item_bytes=max(self.exempt_sizes, default=0),
        )


def _serialized_size(value: Any) -> int:
    try:
        return len(
            json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        )
    except (TypeError, ValueError) as error:
        raise HarnessError("context_item_unserializable") from error


def _sanitized_provider_item(
    item: dict[str, Any], budget: _ContextBudgetAccumulator
) -> dict[str, Any]:
    if item.get("type") not in EXEMPT_RESPONSE_ITEM_TYPES:
        return item
    sanitized = dict(item)
    if "encrypted_content" in item:
        encrypted_content = item["encrypted_content"]
        require(
            encrypted_content is None or isinstance(encrypted_content, str),
            "encrypted_content_invalid",
        )
        if encrypted_content is not None:
            budget.add_exempt(encrypted_content)
            sanitized["encrypted_content"] = "measured_unbounded_exemption"
    return sanitized


def _sanitized_compacted_record(
    record: dict[str, Any], budget: _ContextBudgetAccumulator
) -> dict[str, Any]:
    if record.get("type") != "compacted":
        return record
    payload = record.get("payload")
    if not isinstance(payload, dict):
        return record
    replacement_history = payload.get("replacement_history")
    if not isinstance(replacement_history, list):
        return record
    sanitized_payload = dict(payload)
    sanitized_payload["replacement_history"] = [
        _sanitized_provider_item(item, budget) if isinstance(item, dict) else item
        for item in replacement_history
    ]
    return {**record, "payload": sanitized_payload}


def _exact_message_text(message: dict[str, Any], content_type: str) -> str | None:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if not (isinstance(content, list) and len(content) == 1):
        return None
    item = content[0]
    if not (
        isinstance(item, dict)
        and set(item) == {"type", "text"}
        and item.get("type") == content_type
        and isinstance(item.get("text"), str)
    ):
        return None
    return item["text"]


def require_context_budget(
    records: list[dict[str, Any]],
    fixture: Any,
    assistant_sentinel: str,
    *,
    expected_user_prompt: str | None = None,
) -> ContextBudgetMetrics:
    """Cap effective plaintext items and measure typed encrypted exemptions."""
    budget = _ContextBudgetAccumulator()
    user_messages: list[dict[str, Any]] = []
    assistant_messages: list[dict[str, Any]] = []
    for record in records:
        record_type = record.get("type")
        if record_type == "response_item":
            item = record.get("payload")
            require(isinstance(item, dict), "response_item_payload_invalid")
            budget.add_bounded(_sanitized_provider_item(item, budget))
            if item.get("type") == "message" and item.get("role") == "user":
                user_messages.append(item)
            if item.get("type") == "message" and item.get("role") == "assistant":
                assistant_messages.append(item)
        elif record_type in EFFECTIVE_HISTORY_RECORD_TYPES:
            budget.add_bounded(_sanitized_compacted_record(record, budget))

    require(assistant_messages, "success_sentinel_missing")
    require(len(assistant_messages) == 1, "assistant_message_count")
    require(
        _exact_message_text(assistant_messages[0], "output_text") == assistant_sentinel,
        "assistant_sentinel_mismatch",
    )
    expected_prompt = (
        expected_user_prompt if expected_user_prompt is not None else prompt(fixture)
    )
    evaluation_prompt_count = sum(
        _exact_message_text(message, "input_text") == expected_prompt
        for message in user_messages
    )
    require(evaluation_prompt_count > 0, "user_prompt_mismatch")
    require(evaluation_prompt_count == 1, "evaluation_prompt_count")
    return budget.metrics()


def _mapping_keys(value: Any, *, decode: bool) -> Iterable[str]:
    if isinstance(value, dict):
        for key, child in value.items():
            yield from _decoded_key(key) if decode else (key,)
            yield from _mapping_keys(child, decode=decode)
    elif isinstance(value, list):
        for child in value:
            yield from _mapping_keys(child, decode=decode)


def _decoded_key(key: str, decode_depth: int = 0) -> Iterable[str]:
    try:
        decoded = json.loads(key)
    except json.JSONDecodeError:
        yield key
        return
    if isinstance(decoded, str) and decoded != key:
        if decode_depth >= MAX_JSON_DECODE_DEPTH:
            raise HarnessError("persisted_content_decode_depth")
        yield from _decoded_key(decoded, decode_depth + 1)
    else:
        yield key


def _bounded_join(strings: Iterable[str]) -> str:
    parts: list[str] = []
    total_bytes = 0
    for value in strings:
        total_bytes += len(value.encode("utf-8"))
        require(total_bytes <= MAX_ROLLOUT_BYTES, "persisted_content_stream_over_cap")
        parts.append(value)
    return "".join(parts)


def _normalize_fixture_escapes(value: str) -> str:
    replacements = {
        r"\n": "\n",
        r"\r": "\r",
        r"\u00e9": "é",
        r"\u00E9": "é",
        r"\u6771": "東",
        r"\u4eac": "京",
        r"\u4EAC": "京",
    }
    for escaped, decoded in replacements.items():
        value = value.replace(escaped, decoded)
    return value


def _known_signature_bytes(
    strings: Iterable[str], pattern: re.Pattern[str], signatures: dict[str, int]
) -> int:
    content = _normalize_fixture_escapes(_bounded_join(strings))
    return sum(signatures.get(match.group(0), 0) for match in pattern.finditer(content))


def _line_signatures(lines: list[str], regular_prefix_words: int) -> dict[str, int]:
    signatures: dict[str, int] = {}
    for line in lines:
        words = line.split(" ", regular_prefix_words)
        if line.startswith("emitter-line-"):
            signature = line[: len("emitter-line-00000-")]
        elif line.startswith("line-"):
            signature = " ".join(words[:regular_prefix_words])
        else:
            signature = line.split(" ", 1)[0]
        signatures[signature] = len(line.encode("utf-8")) + 1
    return signatures


def _fixture_content_bytes(
    records: list[dict[str, Any]], pattern: re.Pattern[str], signatures: dict[str, int]
) -> int:
    plain_streams = (
        raw_response_content_strings(records),
        raw_response_content_chunks(records),
        response_content_strings(records),
        response_content_chunks(records),
        _mapping_keys(records, decode=False),
        _mapping_keys(records, decode=True),
    )
    decoded_base64_streams = (
        base64_decoded_strings(raw_response_content_strings(records)),
        base64_decoded_strings(raw_response_content_chunks(records)),
        base64_decoded_strings(response_content_strings(records)),
        base64_decoded_strings(response_content_chunks(records)),
        base64_decoded_strings(_mapping_keys(records, decode=False)),
        base64_decoded_strings(_mapping_keys(records, decode=True)),
    )
    return max(
        (
            _known_signature_bytes(stream, pattern, signatures)
            for stream in (*plain_streams, *decoded_base64_streams)
        ),
        default=0,
    )


def require_fixture_content_budget(
    records: list[dict[str, Any]], fixture: Any, window_text: str
) -> FixtureContentMetrics:
    """Reject exact, transformed, or partial fixture copies that erase savings."""
    emitter_text = fixture.emitter_output_bytes.decode("utf-8")
    encoded_emitter = base64.b64encode(fixture.emitter_output_bytes).decode("ascii")
    for strings in (
        raw_response_content_strings(records),
        raw_response_content_chunks(records),
        response_content_strings(records),
        response_content_chunks(records),
    ):
        require(
            not contains_across_strings(strings, emitter_text),
            "full_stage1_payload_in_rollout",
        )
    for strings in (
        raw_response_content_strings(records),
        raw_response_content_chunks(records),
        response_content_strings(records),
        response_content_chunks(records),
        _mapping_keys(records, decode=False),
        _mapping_keys(records, decode=True),
    ):
        require(
            not contains_across_strings(strings, encoded_emitter),
            "encoded_stage1_payload_in_rollout",
        )
    for strings in (
        base64_decoded_strings(raw_response_content_strings(records)),
        base64_decoded_strings(raw_response_content_chunks(records)),
        base64_decoded_strings(response_content_strings(records)),
        base64_decoded_strings(response_content_chunks(records)),
        base64_decoded_strings(_mapping_keys(records, decode=False)),
        base64_decoded_strings(_mapping_keys(records, decode=True)),
    ):
        require(
            not contains_across_strings(strings, emitter_text),
            "encoded_stage1_payload_in_rollout",
        )

    stage1_bytes = _fixture_content_bytes(
        records,
        _STAGE1_LINE,
        _line_signatures(emitter_text.splitlines(), 1),
    )
    require(
        stage1_bytes <= STAGE1_FIXTURE_CONTENT_BUDGET_BYTES,
        "stage1_fixture_content_over_budget",
    )
    require(
        count_response_content_occurrences(records, window_text) == 1,
        "duplicated_file_read_window_in_rollout",
    )
    require(
        count_across_strings(response_content_strings(records), window_text) == 1,
        "duplicated_file_read_window_in_rollout",
    )
    require(
        count_across_strings(response_content_chunks(records), window_text) == 1,
        "duplicated_file_read_window_in_rollout",
    )
    encoded_window = base64.b64encode(window_text.encode("utf-8")).decode("ascii")
    for strings in (
        raw_response_content_strings(records),
        raw_response_content_chunks(records),
        response_content_strings(records),
        response_content_chunks(records),
    ):
        require(
            not contains_across_strings(strings, encoded_window),
            "duplicated_file_read_window_in_rollout",
        )
    decoded_base64_window_counts = [
        count_across_strings(strings, window_text)
        for strings in (
            base64_decoded_strings(raw_response_content_strings(records)),
            base64_decoded_strings(raw_response_content_chunks(records)),
            base64_decoded_strings(response_content_strings(records)),
            base64_decoded_strings(response_content_chunks(records)),
        )
    ]
    require(
        max(decoded_base64_window_counts, default=0) == 0,
        "duplicated_file_read_window_in_rollout",
    )
    stage2_budget = (
        len(window_text.encode("utf-8")) + STAGE2_FIXTURE_CONTENT_OVERHEAD_BYTES
    )
    stage2_bytes = _fixture_content_bytes(
        records,
        _STAGE2_LINE,
        _line_signatures(window_text.splitlines(), 2),
    )
    require(
        stage2_bytes <= stage2_budget,
        "stage2_fixture_content_over_budget",
    )
    return FixtureContentMetrics(
        stage1_bytes=stage1_bytes,
        stage1_budget_bytes=STAGE1_FIXTURE_CONTENT_BUDGET_BYTES,
        stage2_bytes=stage2_bytes,
        stage2_budget_bytes=stage2_budget,
    )
