"""Content-accounting helpers for complete persisted transcript inspection."""

import base64
import binascii
import json
import re
from collections.abc import Iterable
from typing import Any

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require


MAX_JSON_DECODE_DEPTH = 16
MAX_BASE64_STREAM_BYTES = 8 * 1024 * 1024
_BASE64_FRAGMENT = re.compile(r"[A-Za-z0-9+/=\s]+")
_EMBEDDED_BASE64_RUN = re.compile(r"[A-Za-z0-9+/=\s]{32,}")


def _decode_base64_candidate(candidate: str) -> str | None:
    candidate = "".join(candidate.split())
    if len(candidate) < 32:
        return None
    candidate = candidate[:-1] if len(candidate) % 4 == 1 else candidate
    try:
        decoded = base64.b64decode(
            candidate + "=" * (-len(candidate) % 4), validate=True
        )
        return decoded.decode("utf-8")
    except (binascii.Error, UnicodeError):
        return None


def canonical(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def file_read_values(value: Any) -> list[dict[str, Any]]:
    if isinstance(value, dict):
        found = [value] if value.get("type") == "file_read" else []
        for child in value.values():
            found.extend(file_read_values(child))
        return found
    if isinstance(value, list):
        found: list[dict[str, Any]] = []
        for child in value:
            found.extend(file_read_values(child))
        return found
    if isinstance(value, str):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError:
            return []
        return file_read_values(decoded)
    return []


def _raw_strings(
    value: Any, structural_keys: set[str], key: str | None = None
) -> Iterable[str]:
    if key in structural_keys:
        return
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for child_key, child in value.items():
            yield from _raw_strings(child, structural_keys, child_key)
    elif isinstance(value, list):
        for child in value:
            yield from _raw_strings(child, structural_keys)


def _decoded_strings(
    value: Any,
    structural_keys: set[str],
    key: str | None = None,
    decode_depth: int = 0,
) -> Iterable[str]:
    if key in structural_keys:
        return
    if isinstance(value, str):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError:
            yield value
            return
        if decoded == value:
            yield value
        else:
            if decode_depth >= MAX_JSON_DECODE_DEPTH:
                raise HarnessError("persisted_content_decode_depth")
            yield from _decoded_strings(
                decoded, structural_keys, decode_depth=decode_depth + 1
            )
    elif isinstance(value, dict):
        for child_key, child in value.items():
            yield from _decoded_strings(child, structural_keys, child_key, decode_depth)
    elif isinstance(value, list):
        for child in value:
            yield from _decoded_strings(
                child, structural_keys, decode_depth=decode_depth
            )


def raw_response_content_strings(
    records: Iterable[dict[str, Any]],
) -> Iterable[str]:
    """Yield raw persisted string leaves, including structural fields."""
    for record in records:
        yield from _raw_strings(record, set())


def response_content_strings(records: Iterable[dict[str, Any]]) -> Iterable[str]:
    """Yield terminal strings after recursively decoding JSON string leaves."""
    for record in records:
        yield from _decoded_strings(record, set())


def response_content_chunks(records: Iterable[dict[str, Any]]) -> Iterable[str]:
    """Yield decoded content leaves with structural keys removed for joining."""
    structural_keys = {"type", "role", "call_id", "name", "namespace", "id"}
    for record in records:
        yield from _decoded_strings(record, structural_keys)


def raw_response_content_chunks(
    records: Iterable[dict[str, Any]],
) -> Iterable[str]:
    """Yield raw content leaves with structural keys removed for joining."""
    structural_keys = {"type", "role", "call_id", "name", "namespace", "id"}
    for record in records:
        yield from _raw_strings(record, structural_keys)


def base64_decoded_strings(strings: Iterable[str]) -> Iterable[str]:
    """Yield bounded UTF-8 decodings of consecutive base64 string runs."""
    pending_parts: list[str] = []
    pending_length = 0
    pending_has_padding = False
    examined_bytes = 0

    def decoded_pending() -> str | None:
        if pending_length < 32:
            return None
        return _decode_base64_candidate("".join(pending_parts))

    for value in strings:
        examined_bytes += len(value.encode("utf-8"))
        require(
            examined_bytes <= MAX_BASE64_STREAM_BYTES,
            "persisted_content_stream_over_cap",
        )
        normalized = "".join(value.split())
        is_fragment = (
            bool(normalized)
            and _BASE64_FRAGMENT.fullmatch(value) is not None
            and "=" not in normalized[:-2]
            and (bool(pending_parts) or len(normalized) >= 32)
        )
        if not is_fragment or pending_has_padding:
            decoded = decoded_pending()
            if decoded is not None:
                yield decoded
            pending_parts = []
            pending_length = 0
            pending_has_padding = False
        if not is_fragment:
            for match in _EMBEDDED_BASE64_RUN.finditer(value):
                decoded = _decode_base64_candidate(match.group(0))
                if decoded is not None:
                    yield decoded
        if is_fragment:
            pending_parts.append(normalized)
            pending_length += len(normalized)
            pending_has_padding = "=" in normalized
            require(
                pending_length <= MAX_BASE64_STREAM_BYTES,
                "persisted_content_stream_over_cap",
            )
    decoded = decoded_pending()
    if decoded is not None:
        yield decoded


def count_response_content_occurrences(
    records: Iterable[dict[str, Any]], needle: str
) -> int:
    """Count literal or JSON-escaped payload occurrences across all fields."""
    escaped_variants = {
        json.dumps(needle, ensure_ascii=False)[1:-1],
        json.dumps(needle, ensure_ascii=True)[1:-1],
    }
    count = 0

    def visit(value: Any) -> Iterable[str]:
        if isinstance(value, str):
            yield value
        elif isinstance(value, dict):
            for child in value.values():
                yield from visit(child)
        elif isinstance(value, list):
            for child in value:
                yield from visit(child)

    for record in records:
        for value in visit(record):
            count += max(
                [value.count(needle)]
                + [value.count(escaped) for escaped in escaped_variants]
            )
    return count


def _stream_matches(strings: Iterable[str], needle: str) -> Iterable[bool]:
    prefix = [0] * len(needle)
    matched = 0
    for index in range(1, len(needle)):
        while matched and needle[index] != needle[matched]:
            matched = prefix[matched - 1]
        if needle[index] == needle[matched]:
            matched += 1
        prefix[index] = matched
    matched = 0
    for chunk in strings:
        for character in chunk:
            while matched and character != needle[matched]:
                matched = prefix[matched - 1]
            if character == needle[matched]:
                matched += 1
            if matched == len(needle):
                yield True
                matched = prefix[matched - 1]


def count_across_strings(strings: Iterable[str], needle: str) -> int:
    """Count a payload in linear time across persisted string chunks."""
    return 0 if not needle else sum(_stream_matches(strings, needle))


def contains_across_strings(strings: Iterable[str], needle: str) -> bool:
    """Detect a split payload with bounded linear-time matching."""
    return bool(needle) and next(_stream_matches(strings, needle), False)


def final_assistant_text(records: list[dict[str, Any]]) -> str:
    messages = []
    for record in records:
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        if (
            isinstance(payload, dict)
            and payload.get("type") == "message"
            and payload.get("role") == "assistant"
        ):
            messages.append(payload)
    require(messages, "success_sentinel_missing")
    require(len(messages) == 1, "assistant_message_count")
    content = messages[0].get("content")
    text_parts: list[str] = []

    def visit(value: Any) -> None:
        if isinstance(value, str):
            text_parts.append(value)
        elif isinstance(value, dict):
            if isinstance(value.get("text"), str):
                text_parts.append(value["text"])
            else:
                for child in value.values():
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    visit(content)
    return " ".join(" ".join(text_parts).split())
