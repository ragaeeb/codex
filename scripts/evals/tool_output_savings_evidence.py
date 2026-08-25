"""Read-only state/rollout evidence primitives for the external evaluation."""

import gzip
import hashlib
import json
import errno
import os
import sqlite3
import stat
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable
from urllib.parse import quote

MAX_MODEL_VISIBLE_BYTES = 8 * 1024
MAX_RETRIEVAL_CALLS = 4
MAX_FUNCTION_OUTPUT_AGGREGATE_BYTES = 64 * 1024
MAX_ROLLOUT_BYTES = 8 * 1024 * 1024
USAGE_FIELDS = (
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
)
TOTAL_USAGE_FIELDS = (*USAGE_FIELDS, "total_tokens")
THREAD_QUERY = """SELECT id, rollout_path, tokens_used, model, reasoning_effort, cli_version, updated_at, updated_at_ms FROM threads WHERE id = ?"""


class HarnessError(RuntimeError):
    """A bounded, content-free, user-actionable failure."""


@dataclass(frozen=True)
class ThreadRow:
    id: str
    rollout_path: str
    tokens_used: int
    model: str | None
    reasoning_effort: str | None
    cli_version: str | None
    updated_at: int | None
    updated_at_ms: int | None


@dataclass(frozen=True)
class TokenEvidence:
    total: dict[str, int]
    last: dict[str, int] | None


def require(condition: bool, rule: str) -> None:
    if not condition:
        raise HarnessError(rule)


def short_hash(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()[:12]


def safe_int(value: Any) -> int | None:
    return None if isinstance(value, bool) or not isinstance(value, int) else value


def parse_json_lines(text: str) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise HarnessError(f"invalid_jsonl_line_{line_number}") from error
        require(isinstance(value, dict), "jsonl_record_not_object")
        records.append(value)
    return records


def _rollout_path_for_open(path: Path) -> Path:
    expanded = Path(os.path.abspath(os.path.expanduser(str(path))))
    if len(expanded.parts) >= 2 and expanded.parts[1] == "var":
        return Path("/private/var").joinpath(*expanded.parts[2:])
    return expanded


def _open_rollout(path: Path) -> int:
    require(
        hasattr(os, "O_DIRECTORY")
        and hasattr(os, "O_NOFOLLOW")
        and os.open in getattr(os, "supports_dir_fd", set()),
        "rollout_scope_platform_unsupported",
    )
    expanded = _rollout_path_for_open(path)
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    descriptor = os.open(os.sep, directory_flags)
    try:
        for component in expanded.parts[1:-1]:
            next_descriptor = os.open(component, directory_flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = next_descriptor
        file_descriptor = os.open(
            expanded.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=descriptor
        )
    except OSError as error:
        if error.errno == errno.ENOENT:
            raise HarnessError("rollout_missing") from error
        if error.errno in {errno.ELOOP, errno.ENOTDIR}:
            raise HarnessError("rollout_scope_invalid") from error
        raise HarnessError("rollout_unreadable") from error
    finally:
        os.close(descriptor)
    return file_descriptor


def _read_opened_rollout(descriptor: int, path: Path) -> bytes:
    before = os.fstat(descriptor)
    require(stat.S_ISREG(before.st_mode), "rollout_scope_invalid")
    require(before.st_size <= MAX_ROLLOUT_BYTES, "rollout_over_cap")
    if path.name.endswith(".gz"):
        with os.fdopen(os.dup(descriptor), "rb") as compressed:
            with gzip.GzipFile(fileobj=compressed, mode="rb") as stream:
                data = stream.read(MAX_ROLLOUT_BYTES + 1)
    else:
        chunks: list[bytes] = []
        total = 0
        while total <= MAX_ROLLOUT_BYTES:
            chunk = os.read(descriptor, min(64 * 1024, MAX_ROLLOUT_BYTES + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
        data = b"".join(chunks)
    after = os.fstat(descriptor)
    require(
        (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
        == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns),
        "rollout_changed_during_read",
    )
    require(len(data) <= MAX_ROLLOUT_BYTES, "rollout_over_cap")
    return data


def read_jsonl(path: Path) -> tuple[str, list[dict[str, Any]]]:
    descriptor = -1
    try:
        if path.name.endswith(".zst"):
            raise HarnessError("rollout_zstd_unsupported_stdlib")
        descriptor = _open_rollout(path)
        text = _read_opened_rollout(descriptor, path).decode("utf-8")
    except HarnessError:
        raise
    except (EOFError, OSError, UnicodeError, gzip.BadGzipFile) as error:
        raise HarnessError("rollout_unreadable") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    return text, parse_json_lines(text)


def _sqlite_uri(path: Path) -> str:
    return f"file:{quote(str(path), safe='/:-_')}?mode=ro"


def read_thread_row(db_path: Path, thread_id: str) -> ThreadRow | None:
    if not db_path.is_file():
        raise HarnessError("state_db_missing")
    try:
        connection = sqlite3.connect(_sqlite_uri(db_path), uri=True, timeout=0.05)
        try:
            row = connection.execute(THREAD_QUERY, (thread_id,)).fetchone()
        finally:
            connection.close()
    except sqlite3.OperationalError as error:
        if any(word in str(error).lower() for word in ("locked", "busy")):
            raise HarnessError("state_db_busy") from error
        raise HarnessError("state_db_query_failed") from error
    except sqlite3.Error as error:
        raise HarnessError("state_db_query_failed") from error
    if row is None:
        return None
    require(len(row) == 8, "state_db_row_shape")
    tokens_used = safe_int(row[2])
    require(tokens_used is not None, "state_db_tokens_not_integer")
    return ThreadRow(
        str(row[0]),
        str(row[1]),
        tokens_used,
        None if row[3] is None else str(row[3]),
        None if row[4] is None else str(row[4]),
        None if row[5] is None else str(row[5]),
        safe_int(row[6]),
        safe_int(row[7]),
    )


def poll_thread_row(
    db_path: Path, thread_id: str, *, timeout: float = 12.0, require_tokens: bool = True
) -> ThreadRow:
    deadline = time.monotonic() + timeout
    last_rule = "state_db_row_missing"
    while time.monotonic() < deadline:
        try:
            row = read_thread_row(db_path, thread_id)
        except HarnessError as error:
            last_rule = str(error)
            if last_rule not in {"state_db_busy", "state_db_missing"}:
                raise
            row = None
        if row is not None:
            if not require_tokens or row.tokens_used > 0:
                return row
            last_rule = "state_db_tokens_pending"
        time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))
    raise HarnessError(last_rule)


def selected_rollout(
    row: ThreadRow, db_path: Path, timeout: float
) -> tuple[Path, str, list[dict[str, Any]]]:
    path = Path(row.rollout_path).expanduser()
    if not path.is_absolute():
        path = db_path.parent / path
    path = Path(os.path.abspath(path))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            text, records = read_jsonl(path)
            return path, text, records
        except HarnessError as error:
            if str(error) != "rollout_missing":
                raise
        if path.suffix == ".zst":
            raise HarnessError("rollout_zstd_unsupported_stdlib")
        compressed_path = path.with_name(path.name + ".zst")
        compressed_descriptor = -1
        try:
            compressed_descriptor = _open_rollout(compressed_path)
        except HarnessError as error:
            if str(error) != "rollout_missing":
                raise
        finally:
            if compressed_descriptor >= 0:
                os.close(compressed_descriptor)
        if compressed_descriptor >= 0:
            raise HarnessError("rollout_zstd_unsupported_stdlib")
        time.sleep(0.1)
    raise HarnessError("rollout_missing")


def measure_thread(
    db_path: Path,
    thread_id: str,
    *,
    timeout: float = 20.0,
    terminal_usage: dict[str, int] | None = None,
    model: str | None = None,
    reasoning_effort: str | None = None,
    cli_version: str | None = None,
) -> tuple[ThreadRow, Path, str, list[dict[str, Any]], TokenEvidence]:
    deadline = time.monotonic() + timeout
    last_rule = "rollout_measurement_pending"
    retryable = {
        "state_db_busy",
        "state_db_missing",
        "state_db_row_missing",
        "state_db_tokens_pending",
        "rollout_missing",
        "rollout_changed_during_read",
        "rollout_token_count_missing",
        "db_rollout_total_pending",
    }
    while time.monotonic() < deadline:
        try:
            row = poll_thread_row(
                db_path,
                thread_id,
                timeout=min(0.5, max(0.05, deadline - time.monotonic())),
            )
            rollout_path, rollout_text, records = selected_rollout(
                row, db_path, timeout=min(0.5, max(0.05, deadline - time.monotonic()))
            )
            validate_rollout_identity(records, thread_id)
            usage = latest_token_count(records)
            if row.tokens_used != usage.total["total_tokens"]:
                raise HarnessError("db_rollout_total_pending")
            metadata = (model, reasoning_effort, cli_version)
            if terminal_usage is not None or any(item is not None for item in metadata):
                require(
                    terminal_usage is not None
                    and all(item is not None for item in metadata),
                    "measurement_metadata_missing",
                )
                assert_usage_agreement(
                    row,
                    usage,
                    terminal_usage,
                    model=model,
                    reasoning_effort=reasoning_effort,
                    cli_version=cli_version,
                )
            return row, rollout_path, rollout_text, records, usage
        except HarnessError as error:
            last_rule = str(error)
            if not (
                last_rule in retryable
                or last_rule.startswith("db_")
                or last_rule.startswith("terminal_rollout_")
            ):
                raise
            time.sleep(0.1)
    raise HarnessError(last_rule)


def parse_usage(value: Any, fields: Iterable[str]) -> dict[str, int] | None:
    if not isinstance(value, dict):
        return None
    parsed: dict[str, int] = {}
    for field in fields:
        number = safe_int(value.get(field))
        if number is None or number < 0:
            return None
        parsed[field] = number
    if parsed.get("cached_input_tokens", 0) > parsed.get("input_tokens", 0):
        return None
    if "total_tokens" in parsed:
        if parsed["total_tokens"] < parsed.get("input_tokens", 0) + parsed.get(
            "output_tokens", 0
        ):
            return None
        if parsed.get("reasoning_output_tokens", 0) > parsed.get("output_tokens", 0):
            return None
    return parsed


def latest_token_count(records: Iterable[dict[str, Any]]) -> TokenEvidence:
    latest: TokenEvidence | None = None
    for record in records:
        if record.get("type") != "event_msg":
            continue
        payload = record.get("payload")
        if not (isinstance(payload, dict) and payload.get("type") == "token_count"):
            continue
        info = payload.get("info")
        if info is None:
            require(latest is None, "rollout_token_count_invalid")
            continue
        require(isinstance(info, dict), "rollout_token_count_invalid")
        total = parse_usage(info.get("total_token_usage"), TOTAL_USAGE_FIELDS)
        require(total is not None, "rollout_token_count_invalid")
        last_value = info.get("last_token_usage")
        last = (
            None if last_value is None else parse_usage(last_value, TOTAL_USAGE_FIELDS)
        )
        require(
            last_value is None or last is not None,
            "rollout_last_token_usage_invalid",
        )
        if last is not None:
            require(
                all(last[field] <= total[field] for field in TOTAL_USAGE_FIELDS),
                "rollout_last_token_usage_invalid",
            )
        if latest is not None:
            require(
                all(
                    total[field] >= latest.total[field] for field in TOTAL_USAGE_FIELDS
                ),
                "rollout_token_count_non_monotonic",
            )
        latest = TokenEvidence(total, last)
    require(latest is not None, "rollout_token_count_missing")
    return latest


def validate_rollout_identity(
    records: Iterable[dict[str, Any]], thread_id: str
) -> None:
    metas = [record for record in records if record.get("type") == "session_meta"]
    require(len(metas) == 1, "rollout_session_meta_count")
    payload = metas[0].get("payload")
    require(
        isinstance(payload, dict) and payload.get("id") == thread_id,
        "rollout_session_identity_mismatch",
    )


def assert_usage_agreement(
    row: ThreadRow,
    rollout_usage: TokenEvidence,
    terminal_usage: dict[str, int],
    *,
    model: str,
    reasoning_effort: str,
    cli_version: str,
) -> None:
    require(
        row.tokens_used == rollout_usage.total["total_tokens"],
        "db_rollout_total_mismatch",
    )
    require(row.model == model, "db_model_missing_or_mismatch")
    require(
        row.reasoning_effort == reasoning_effort,
        "db_reasoning_effort_missing_or_mismatch",
    )
    require(row.cli_version == cli_version, "db_cli_version_missing_or_mismatch")
    for field in USAGE_FIELDS:
        require(
            terminal_usage.get(field) == rollout_usage.total[field],
            f"terminal_rollout_{field}_mismatch",
        )


def validate_exec_events(events: list[dict[str, Any]]) -> tuple[str, dict[str, int]]:
    started = [event for event in events if event.get("type") == "thread.started"]
    completed = [event for event in events if event.get("type") == "turn.completed"]
    require(len(started) == 1, "thread_started_count")
    require(len(completed) == 1, "turn_completed_count")
    require(
        events.index(started[0]) < events.index(completed[0]),
        "terminal_event_order",
    )
    require(events and events[-1] is completed[0], "terminal_event_not_final")
    require(
        not any(event.get("type") in {"error", "turn.failed"} for event in events),
        "terminal_error",
    )
    thread_id = started[0].get("thread_id")
    require(isinstance(thread_id, str), "thread_id_invalid")
    try:
        uuid.UUID(thread_id)
    except ValueError as error:
        raise HarnessError("thread_id_invalid") from error
    usage = parse_usage(completed[0].get("usage"), USAGE_FIELDS)
    require(usage is not None, "terminal_usage_missing")
    require(
        usage["input_tokens"] > 0 and usage["output_tokens"] > 0, "terminal_usage_zero"
    )
    return thread_id, usage


def recover_thread_id(events: Iterable[dict[str, Any]]) -> str | None:
    """Recover exactly one valid thread ID from a partial CLI event stream."""
    candidates = [
        event.get("thread_id")
        for event in events
        if event.get("type") == "thread.started"
    ]
    if not candidates:
        return None
    valid: set[str] = set()
    for thread_id in candidates:
        require(isinstance(thread_id, str), "thread_id_invalid")
        try:
            uuid.UUID(thread_id)
        except ValueError as error:
            raise HarnessError("thread_id_invalid") from error
        valid.add(thread_id)
    require(len(valid) == 1, "thread_id_ambiguous")
    return valid.pop()


def contains_string(value: Any, needle: str) -> bool:
    if isinstance(value, str):
        return needle in value
    if isinstance(value, dict):
        return any(contains_string(child, needle) for child in value.values())
    if isinstance(value, list):
        return any(contains_string(child, needle) for child in value)
    return False


def percentage_reduction(baseline: int, actual: int) -> float:
    return 0.0 if baseline <= 0 else round((baseline - actual) * 100 / baseline, 4)
