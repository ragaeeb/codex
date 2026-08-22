"""Semantic transcript, artifact, and content-free report analysis."""

import hashlib
import shlex
from pathlib import Path
from typing import Any

from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_evidence import MAX_RETRIEVAL_CALLS
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import ThreadRow
from tool_output_savings_evidence import assert_usage_agreement
from tool_output_savings_evidence import contains_string
from tool_output_savings_evidence import latest_token_count
from tool_output_savings_evidence import percentage_reduction
from tool_output_savings_evidence import require
from tool_output_savings_evidence import short_hash
from tool_output_savings_evidence import validate_exec_events
from tool_output_savings_evidence import validate_rollout_identity
from tool_output_savings_artifacts import artifact_ids_in
from tool_output_savings_artifacts import list_thread_artifacts
from tool_output_savings_artifacts import read_artifact
from tool_output_savings_artifacts import validate_artifact_id
from tool_output_savings_projection import require_artifact_envelope
from tool_output_savings_projection import require_retrievals
from tool_output_savings_content import canonical
from tool_output_savings_content import file_read_values
from tool_output_savings_context_budget import require_context_budget
from tool_output_savings_context_budget import require_fixture_content_budget
from tool_output_savings_context_budget import CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_BYTES
from tool_output_savings_transcript import ToolCall
from tool_output_savings_transcript import collect_tool_calls
from tool_output_savings_transcript import collect_tool_outputs
from tool_output_savings_transcript import complete_response_item_metrics
from tool_output_savings_transcript import persisted_record_metrics
from tool_output_savings_transcript import require_exact_tool_sequence
from tool_output_savings_transcript import require_transcript_causality
from tool_output_savings_transcript import tool_response_item_metrics


def _require_file_read_window(value: Any, fixture: Any) -> None:
    require(
        isinstance(value, dict) and value.get("type") == "file_read",
        "first_read_not_inline",
    )
    require(
        set(value)
        == {
            "type",
            "version",
            "path",
            "fingerprint",
            "window",
            "next_offset",
            "eof",
            "continuation",
        },
        "file_read_shape_invalid",
    )
    require(value.get("version") == 1, "file_read_version_invalid")
    require(value.get("path") == fixture.window_name, "file_read_path_invalid")
    window = value.get("window")
    fingerprint = value.get("fingerprint")
    require(
        isinstance(window, dict)
        and set(window)
        == {
            "start_byte",
            "end_byte",
            "text",
            "line_fragments",
            "line_continues",
        }
        and isinstance(fingerprint, dict)
        and set(fingerprint) == {"size_bytes", "modified_at_ms", "window_digest"},
        "file_read_shape_invalid",
    )
    text = window.get("text")
    start = window.get("start_byte")
    end = window.get("end_byte")
    require(
        isinstance(text, str)
        and type(start) is int
        and start == 0
        and type(end) is int
        and end > start,
        "file_read_window_invalid",
    )
    encoded = text.encode("utf-8")
    require(
        encoded == fixture.window_bytes[start:end], "file_read_window_bytes_mismatch"
    )
    require(
        fingerprint.get("size_bytes") == len(fixture.window_bytes),
        "file_read_size_mismatch",
    )
    require(
        fingerprint.get("window_digest")
        == f"sha256:{hashlib.sha256(encoded).hexdigest()}",
        "file_read_digest_mismatch",
    )
    require(
        type(fingerprint.get("size_bytes")) is int
        and type(fingerprint.get("modified_at_ms")) is int,
        "file_read_fingerprint_invalid",
    )
    require(
        fingerprint.get("modified_at_ms") == fixture.window_modified_at_ms,
        "file_read_mtime_mismatch",
    )
    require(
        fixture.window_middle_offset < end and fixture.window_middle in text,
        "file_marker_not_in_inline",
    )
    line_fragments = window.get("line_fragments")
    line_continues = window.get("line_continues")
    require(
        type(line_fragments) is int and line_fragments > 0,
        "file_read_lines_invalid",
    )
    require(type(line_continues) is bool, "file_read_lines_invalid")
    expected_fragments = text.count("\n") + int(bool(text) and not text.endswith("\n"))
    require(line_fragments == expected_fragments, "file_read_lines_invalid")
    require(
        line_fragments <= 2_000
        and len(max(text.splitlines() or [""], key=len)) <= 2_000,
        "file_read_line_bound",
    )
    eof = value.get("eof")
    next_offset = value.get("next_offset")
    require(isinstance(eof, bool), "file_read_eof_invalid")
    require(eof is (end == len(fixture.window_bytes)), "file_read_eof_invalid")
    require(
        line_continues is (end < len(fixture.window_bytes) and not text.endswith("\n")),
        "file_read_lines_invalid",
    )
    require((next_offset is None) == eof, "file_read_continuation_invalid")
    require(
        (value.get("continuation") is None) == eof
        and (
            value.get("continuation") is None
            or value.get("continuation")
            == "Call read_file again with offset=next_offset."
        ),
        "file_read_continuation_invalid",
    )
    if next_offset is not None:
        require(
            next_offset == end and next_offset > start, "file_read_next_offset_invalid"
        )


def _require_exec_arguments(call: ToolCall, fixture: Any) -> None:
    require(
        set(call.arguments) == {"cmd", "max_output_tokens"},
        "exec_command_arguments_invalid",
    )
    command = call.arguments.get("cmd")
    require(isinstance(command, str), "exec_command_arguments_invalid")
    try:
        parts = shlex.split(command)
    except ValueError as error:
        raise HarnessError("exec_command_arguments_invalid") from error
    require(
        parts == [fixture.python_executable, fixture.emitter_name],
        "exec_fixture_command_mismatch",
    )
    require(
        type(call.arguments.get("max_output_tokens")) is int
        and call.arguments.get("max_output_tokens") == 1_000,
        "exec_spill_limit_missing",
    )


def analyze(
    events: list[dict[str, Any]],
    rollout_text: str,
    records: list[dict[str, Any]],
    fixture: Any,
    codex_home: Path,
    thread_id: str,
    row: ThreadRow,
    terminal: dict[str, int],
    *,
    cli_version: str,
    model: str,
    reasoning_effort: str,
    binary_sha256: str,
    binary_built_in_run: bool,
    repository_commit: str | None,
    working_tree_dirty: bool,
    model_tool_mode: str,
    catalog_sha256: str = "unknown",
    catalog_entry_sha256: str = "unknown",
    harness_sha256: str = "unknown",
    binary_sha256_before: str = "unknown",
    binary_physically_rebuilt_in_run: bool | None = None,
    tracked_worktree_clean_before: bool | None = None,
    tracked_worktree_clean_after: bool | None = None,
    status_digest_before: str = "unknown",
    status_digest_after: str = "unknown",
) -> dict[str, Any]:
    require(isinstance(rollout_text, str) and records, "rollout_not_valid_jsonl")
    validate_rollout_identity(records, thread_id)
    validate_exec_events(events)
    rollout_usage = latest_token_count(records)
    assert_usage_agreement(
        row,
        rollout_usage,
        terminal,
        model=model,
        reasoning_effort=reasoning_effort,
        cli_version=cli_version,
    )
    calls = collect_tool_calls(records)
    outputs = collect_tool_outputs(records)
    require_transcript_causality(records, calls)
    require(
        set(call.name for call in calls)
        == {"exec_command", "read_file", "read_tool_output"},
        "unexpected_tool_family",
    )
    exec_calls = [call for call in calls if call.name == "exec_command"]
    require(len(exec_calls) == 1, "exec_call_count")
    _require_exec_arguments(exec_calls[0], fixture)
    read_calls = [call for call in calls if call.name == "read_file"]
    require(len(read_calls) == 2, "read_file_call_count")
    expected_read = {
        "path": fixture.window_name,
        "offset": 0,
        "max_bytes": 32_768,
        "max_lines": 2_000,
    }
    for call in read_calls:
        require(
            set(call.arguments) == set(expected_read), "read_file_arguments_mismatch"
        )
        for key, value in expected_read.items():
            require(
                type(call.arguments[key]) is type(value), "read_file_arguments_mismatch"
            )
            require(call.arguments[key] == value, "read_file_arguments_mismatch")
    retrieval_calls = [call for call in calls if call.name == "read_tool_output"]
    require(len(retrieval_calls) == MAX_RETRIEVAL_CALLS, "retrieval_call_count")
    by_call = {output.call_id: output for output in outputs}
    read_outputs = [by_call.get(call.call_id) for call in read_calls]
    require(
        all(output is not None for output in read_outputs), "read_file_output_missing"
    )
    inline_read = read_outputs[0]
    duplicate_read = read_outputs[1]
    require(
        isinstance(inline_read.value, dict)
        and inline_read.value.get("type") == "file_read",
        "first_read_not_inline",
    )
    require(
        isinstance(duplicate_read.value, dict)
        and duplicate_read.value.get("type") == "tool_output_artifact",
        "duplicate_not_artifact",
    )
    require(inline_read is not None, "first_read_not_inline")
    require(duplicate_read is not None, "duplicate_not_artifact")
    require(
        len(inline_read.raw.encode()) <= MAX_MODEL_VISIBLE_BYTES, "inline_read_over_cap"
    )
    _require_file_read_window(inline_read.value, fixture)
    duplicate_value = duplicate_read.value
    duplicate_id = validate_artifact_id(duplicate_value.get("artifact_id"))
    duplicate_raw = duplicate_read.raw
    inline_bytes = len(inline_read.raw.encode())
    require(
        len(duplicate_raw.encode()) < inline_bytes,
        "stage2_artifact_not_economical",
    )
    require(
        not contains_string(duplicate_value.get("preview"), fixture.window_middle),
        "file_marker_in_preview",
    )
    stage1_output = next(
        (
            output
            for output in outputs
            if output.call_id == exec_calls[0].call_id
            and isinstance(output.value, dict)
            and output.value.get("type") == "tool_output_artifact"
        ),
        None,
    )
    require(stage1_output is not None, "stage1_artifact_missing")
    stage1_id = validate_artifact_id(stage1_output.value.get("artifact_id"))
    managed_artifacts = list_thread_artifacts(codex_home, thread_id)
    all_artifact_ids = set().union(
        *(artifact_ids_in(output.value) for output in outputs),
        *(artifact_ids_in(call.arguments) for call in calls),
    )
    require(
        set(managed_artifacts) == all_artifact_ids,
        "unexpected_or_missing_managed_artifact",
    )
    stage1_data = read_artifact(codex_home, thread_id, stage1_id)
    stage2_data = read_artifact(codex_home, thread_id, duplicate_id)
    require(stage1_data == fixture.emitter_output_bytes, "stage1_artifact_content")
    for marker in (fixture.shell_head, fixture.shell_middle, fixture.shell_tail):
        require(stage1_data.count(marker.encode()) == 1, "stage1_sentinel_count")
    require(stage2_data == inline_read.raw.encode(), "stage2_artifact_content")
    require(
        stage2_data.count(fixture.window_middle.encode()) == 1, "stage2_marker_count"
    )
    require(
        not contains_string(stage1_output.value.get("preview"), fixture.shell_middle),
        "stage1_marker_in_preview",
    )
    require_artifact_envelope(
        stage1_output.value,
        stage1_id,
        stage1_data,
        "text/plain",
        fixture.shell_middle,
    )
    require_artifact_envelope(
        duplicate_value,
        duplicate_id,
        stage2_data,
        "application/vnd.codex.file-read+json",
        fixture.window_middle,
    )
    file_read_payloads = file_read_values(records)
    inline_digest = hashlib.sha256(canonical(inline_read.value).encode()).hexdigest()
    require(
        sum(
            hashlib.sha256(canonical(value).encode()).hexdigest() == inline_digest
            for value in file_read_payloads
        )
        == 1,
        "duplicated_inline_read_in_rollout",
    )
    window_text = inline_read.value["window"]["text"]
    fixture_content_metrics = require_fixture_content_budget(
        records, fixture, window_text
    )
    context_metrics = require_context_budget(
        records, fixture, "TOOL_OUTPUT_SAVINGS_E2E_SUCCESS"
    )
    stage1_retrieval_ids = require_retrievals(
        retrieval_calls,
        outputs,
        stage1_id,
        fixture.shell_middle,
        stage1_data,
        records=records,
        producer_call_id=exec_calls[0].call_id,
    )
    stage2_retrieval_ids = require_retrievals(
        retrieval_calls,
        outputs,
        duplicate_id,
        fixture.window_middle,
        stage2_data,
        records=records,
        producer_call_id=read_calls[1].call_id,
    )
    require_exact_tool_sequence(
        records,
        [
            exec_calls[0].call_id,
            *stage1_retrieval_ids,
            read_calls[0].call_id,
            read_calls[1].call_id,
            *stage2_retrieval_ids,
        ],
    )
    function_output_bytes = sum(len(output.raw.encode()) for output in outputs)
    tool_metrics = tool_response_item_metrics(records)
    complete_item_metrics = complete_response_item_metrics(records)
    persisted_metrics = persisted_record_metrics(records)
    max_output_bytes = max((len(output.raw.encode()) for output in outputs), default=0)
    unique_artifact_sizes = {
        artifact_id: len(data) for artifact_id, data in managed_artifacts.items()
    }
    require(
        len(stage1_output.raw.encode()) < len(stage1_data),
        "stage1_artifact_not_economical",
    )
    return {
        "status": "pass",
        "evaluation": "live_synthetic_integrity_and_byte_economics",
        "evaluation_lane": "direct_stage1_stage2_projection",
        "byte_economics_scope": "mechanism evidence only; not token-savings or corpus/workflow evidence",
        "model": model,
        "model_tool_mode": model_tool_mode,
        "model_visible_cap_bytes": MAX_MODEL_VISIBLE_BYTES,
        "model_visible_conservative_token_ceiling_tokens": MAX_MODEL_VISIBLE_BYTES,
        "tool_output_model_visible_cap_bytes": MAX_MODEL_VISIBLE_BYTES,
        "effective_context_item_approx_token_cap": MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS,
        "context_token_estimator_bytes_per_token": CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN,
        "effective_context_item_approx_byte_cap": MAX_EFFECTIVE_CONTEXT_ITEM_BYTES,
        "manual_review_threshold_tokens": 1_000,
        "manual_review_required": True,
        "manual_review_status": "external_review_required",
        "manual_review_evidence": "external signoff required; the harness does not self-attest acceptance",
        "reasoning_effort": reasoning_effort,
        "cli_version": cli_version,
        "binary_sha256": binary_sha256,
        "binary_sha256_before": binary_sha256_before,
        "binary_built_in_run": binary_built_in_run,
        "binary_physically_rebuilt_in_run": binary_physically_rebuilt_in_run,
        "catalog_sha256": catalog_sha256,
        "catalog_entry_sha256": catalog_entry_sha256,
        "harness_sha256": harness_sha256,
        "tracked_worktree_clean_before": tracked_worktree_clean_before,
        "tracked_worktree_clean_after": tracked_worktree_clean_after,
        "repository_status_digest_before": status_digest_before,
        "repository_status_digest_after": status_digest_after,
        "repository_commit": repository_commit
        if binary_built_in_run
        else "not_attributed",
        "repository_commit_attribution": "built_in_run"
        if binary_built_in_run
        else "not_claimed",
        "working_tree_dirty": working_tree_dirty,
        "thread_hash": short_hash(thread_id),
        "db_model": row.model,
        "db_reasoning_effort": row.reasoning_effort,
        "db_cli_version": row.cli_version,
        "db_indexed_total_tokens": row.tokens_used,
        "rollout_token_usage": rollout_usage.total,
        "rollout_last_token_usage": rollout_usage.last,
        "exec_terminal_usage": terminal,
        "usage_agreement": {
            "db_equals_rollout_total": True,
            "db_model_matches": True,
            "db_reasoning_effort_matches": True,
            "db_cli_version_matches": True,
            "exec_components_equal_rollout": True,
        },
        "stage1_original_artifact_bytes": len(stage1_data),
        "stage1_model_visible_envelope_bytes": len(stage1_output.raw.encode()),
        "stage1_envelope_byte_reduction_percent": percentage_reduction(
            len(stage1_data), len(stage1_output.raw.encode())
        ),
        "stage2_hypothetical_duplicate_inline_bytes": inline_bytes,
        "stage2_duplicate_envelope_bytes": len(duplicate_raw.encode()),
        "stage2_incremental_byte_reduction_percent": percentage_reduction(
            inline_bytes, len(duplicate_raw.encode())
        ),
        "rollout_output_body_bytes": function_output_bytes,
        "artifact_store_bytes": sum(unique_artifact_sizes.values()),
        "artifact_count": len(all_artifact_ids),
        "retrieval_count": len(retrieval_calls),
        "max_model_visible_function_output_bytes": max_output_bytes,
        "max_model_visible_tool_item_bytes": tool_metrics["max_item_bytes"],
        "model_visible_tool_item_bytes": tool_metrics["aggregate_bytes"],
        "model_visible_complete_response_item_bytes": complete_item_metrics[
            "aggregate_bytes"
        ],
        "max_complete_response_item_bytes": complete_item_metrics["max_item_bytes"],
        "complete_response_item_count": complete_item_metrics["item_count"],
        "bounded_effective_context_item_bytes": context_metrics.bounded_item_bytes,
        "bounded_effective_context_item_count": context_metrics.bounded_item_count,
        "max_bounded_effective_context_item_bytes": context_metrics.max_bounded_item_bytes,
        "exempt_reasoning_encrypted_item_bytes": context_metrics.exempt_item_bytes,
        "exempt_reasoning_encrypted_item_count": context_metrics.exempt_item_count,
        "max_exempt_reasoning_encrypted_item_bytes": context_metrics.max_exempt_item_bytes,
        "stage1_persisted_fixture_content_bytes": fixture_content_metrics.stage1_bytes,
        "stage1_persisted_fixture_content_budget_bytes": fixture_content_metrics.stage1_budget_bytes,
        "stage2_persisted_fixture_content_bytes": fixture_content_metrics.stage2_bytes,
        "stage2_persisted_fixture_content_budget_bytes": fixture_content_metrics.stage2_budget_bytes,
        "persisted_record_bytes": persisted_metrics["aggregate_bytes"],
        "persisted_record_count": persisted_metrics["record_count"],
        "max_persisted_record_bytes": persisted_metrics["max_record_bytes"],
        "invariants": {
            "terminal_events_and_usage": True,
            "state_db_rollout_exec_usage_agree": True,
            "expected_tool_families_and_arguments": True,
            "stage1_artifact_recoverable": True,
            "stage2_duplicate_artifact_recoverable": True,
            "duplicate_payload_not_repeated": True,
            "retrievals_exact_bounded_and_structured": True,
            "model_visible_outputs_bounded": True,
            "effective_non_reasoning_context_items_bounded": True,
            "reasoning_encrypted_items_measured_not_bounded": True,
            "complete_persisted_representation_measured": True,
            "artifact_ids_are_scoped_and_digest_checked": True,
            "full_stage1_payload_not_in_rollout": True,
            "success_sentinel_observed": True,
        },
    }
