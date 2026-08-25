"""Luna Code Mode prompt and outer-transcript evidence analysis."""

import json
import re
import shlex
from pathlib import Path
from typing import Any

from tool_output_savings_artifacts import artifact_ids_in
from tool_output_savings_artifacts import list_thread_artifacts
from tool_output_savings_artifacts import read_artifact
from tool_output_savings_artifacts import validate_artifact_id
from tool_output_savings_content import base64_decoded_strings
from tool_output_savings_content import contains_across_strings
from tool_output_savings_content import final_assistant_text
from tool_output_savings_content import response_content_strings
from tool_output_savings_context_budget import require_context_budget
from tool_output_savings_context_budget import CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_BYTES
from tool_output_savings_code_mode_retrieval import require_top_level_retrievals
from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_evidence import ThreadRow
from tool_output_savings_evidence import assert_usage_agreement
from tool_output_savings_evidence import latest_token_count
from tool_output_savings_evidence import percentage_reduction
from tool_output_savings_evidence import require
from tool_output_savings_evidence import short_hash
from tool_output_savings_evidence import validate_exec_events
from tool_output_savings_evidence import validate_rollout_identity
from tool_output_savings_projection import require_artifact_envelope
from tool_output_savings_transcript import complete_response_item_metrics
from tool_output_savings_transcript import persisted_record_metrics
from tool_output_savings_transcript import response_items
from tool_output_savings_transcript import tool_response_item_metrics
from tool_output_savings_transcript import validate_tool_response_items


CODE_MODE_EVALUATION_LANE = "luna_code_mode_stage1_and_usage"
CODE_MODE_RESULT_TYPE = "tool_output_savings_code_mode_evidence"
SUCCESS_SENTINEL = "TOOL_OUTPUT_SAVINGS_E2E_SUCCESS"
_COMPLETED_STATUS = re.compile(
    r"^Script completed\nWall time [0-9]+\.[0-9] seconds\nOutput:\n$"
)


def code_mode_program(fixture: Any) -> str:
    command = shlex.join([fixture.python_executable, fixture.emitter_name])
    command_json = json.dumps(command, ensure_ascii=False)
    file_marker_json = json.dumps(fixture.window_middle, ensure_ascii=False)
    file_json = json.dumps(fixture.window_name, ensure_ascii=False)
    return f"""// @exec: {{"yield_time_ms": 30000, "max_output_tokens": 2000}}
const shell = await tools.exec_command({{cmd: {command_json}, max_output_tokens: 1000}});
const envelope = JSON.parse(shell.output);
const first = await tools.read_file({{path: {file_json}, offset: 0, max_bytes: 32768, max_lines: 2000}});
const duplicate = await tools.read_file({{path: {file_json}, offset: 0, max_bytes: 32768, max_lines: 2000}});
const firstJson = JSON.stringify(first);
const duplicateJson = JSON.stringify(duplicate);
text(JSON.stringify({{
  type: "{CODE_MODE_RESULT_TYPE}",
  version: 1,
  sentinel: "{SUCCESS_SENTINEL}",
  stage1: {{envelope}},
  stage2: {{
    behavior: "inline_by_design",
    first_type: first.type,
    duplicate_type: duplicate.type,
    exact_equal: firstJson === duplicateJson,
    path: first.path,
    size_bytes: first.fingerprint.size_bytes,
    modified_at_ms: first.fingerprint.modified_at_ms,
    window_digest: first.fingerprint.window_digest,
    start_byte: first.window.start_byte,
    end_byte: first.window.end_byte,
    window_bytes: first.window.end_byte - first.window.start_byte,
    marker_present: first.window.text.includes({file_marker_json}),
    next_offset: first.next_offset,
    eof: first.eof,
    first_serialized_chars: firstJson.length,
    duplicate_serialized_chars: duplicateJson.length
  }}
}}));"""


def code_mode_prompt(fixture: Any) -> str:
    return (
        "Make exactly one top-level Code Mode exec call and execute the JavaScript below exactly "
        "as written. Do not add logging, output, tools, or error recovery. After exec succeeds, "
        "read the emitted stage1.envelope.artifact_id and make exactly two top-level "
        f"read_tool_output calls: first mode=search query={fixture.shell_middle!r} limit=4, then "
        "mode=bytes using exactly the returned byte_offsets[0] as offset and limit=256. Make no "
        "wait call and no other tool call. After both retrievals succeed, respond with exactly "
        f"{SUCCESS_SENTINEL}.\n\n<exact-code-mode-program>\n{code_mode_program(fixture)}"
        "\n</exact-code-mode-program>"
    )


def _outer_code_mode_result(
    records: list[dict[str, Any]], fixture: Any
) -> tuple[str, dict[str, Any], str, int, int]:
    validate_tool_response_items(records)
    calls = []
    outputs = []
    for position, item in enumerate(response_items(records)):
        if item.get("type") == "custom_tool_call":
            calls.append((position, item))
        elif item.get("type") == "custom_tool_call_output":
            outputs.append((position, item))
    require(len(calls) == 1, "code_mode_exec_call_count")
    require(len(outputs) == 1, "code_mode_exec_output_count")
    call_position, call = calls[0]
    output_position, output = outputs[0]
    require(call.get("name") == "exec", "unexpected_tool_family")
    call_id = call.get("call_id")
    require(isinstance(call_id, str) and bool(call_id), "malformed_tool_call")
    expected_program = code_mode_program(fixture)
    require(not expected_program.endswith("\n"), "code_mode_expected_program_invalid")
    require(
        call.get("input") in {expected_program, f"{expected_program}\n"},
        "code_mode_program_mismatch",
    )
    require(
        output.get("call_id") == call_id and call_position < output_position,
        "tool_call_output_order",
    )
    raw = output.get("output")
    require(isinstance(raw, list) and len(raw) == 2, "code_mode_output_shape")
    status, emitted = raw
    require(
        isinstance(status, dict)
        and set(status) == {"type", "text"}
        and status.get("type") == "input_text"
        and isinstance(status.get("text"), str)
        and _COMPLETED_STATUS.fullmatch(status["text"]) is not None,
        "code_mode_status_invalid",
    )
    require(
        isinstance(emitted, dict)
        and set(emitted) == {"type", "text"}
        and emitted.get("type") == "input_text"
        and isinstance(emitted.get("text"), str),
        "code_mode_output_shape",
    )
    text = emitted["text"]
    require(
        len(text.encode("utf-8")) <= MAX_MODEL_VISIBLE_BYTES,
        "code_mode_output_over_cap",
    )
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        from tool_output_savings_evidence import HarnessError

        raise HarnessError("code_mode_output_not_json") from error
    require(isinstance(value, dict), "code_mode_output_not_json")
    return text, value, call_id, call_position, output_position


def _require_stage2_summary(value: Any, fixture: Any) -> int:
    expected_keys = {
        "behavior",
        "first_type",
        "duplicate_type",
        "exact_equal",
        "path",
        "size_bytes",
        "modified_at_ms",
        "window_digest",
        "start_byte",
        "end_byte",
        "window_bytes",
        "marker_present",
        "next_offset",
        "eof",
        "first_serialized_chars",
        "duplicate_serialized_chars",
    }
    require(
        isinstance(value, dict) and set(value) == expected_keys,
        "code_mode_stage2_shape",
    )
    require(value.get("behavior") == "inline_by_design", "code_mode_stage2_behavior")
    require(
        value.get("first_type") == "file_read"
        and value.get("duplicate_type") == "file_read"
        and value.get("exact_equal") is True,
        "code_mode_duplicate_not_exact_inline",
    )
    expected_path = str(fixture.root / fixture.window_name)
    require(value.get("path") == expected_path, "file_read_path_invalid")
    require(
        value.get("size_bytes") == len(fixture.window_bytes), "file_read_size_mismatch"
    )
    require(
        value.get("modified_at_ms") == fixture.window_modified_at_ms,
        "file_read_mtime_mismatch",
    )
    start, end, window_bytes = (
        value.get("start_byte"),
        value.get("end_byte"),
        value.get("window_bytes"),
    )
    require(
        type(start) is int
        and start == 0
        and type(end) is int
        and type(window_bytes) is int
        and end == window_bytes
        and fixture.window_middle_offset < end <= 32_768,
        "file_read_window_invalid",
    )
    require(value.get("marker_present") is True, "file_marker_not_in_inline")
    require(
        value.get("eof") is False and value.get("next_offset") == end,
        "file_read_continuation_invalid",
    )
    digest = value.get("window_digest")
    require(
        isinstance(digest, str)
        and digest.startswith("sha256:")
        and len(digest) == len("sha256:") + 64,
        "file_read_digest_invalid",
    )
    first_chars = value.get("first_serialized_chars")
    duplicate_chars = value.get("duplicate_serialized_chars")
    require(
        type(first_chars) is int
        and 0 < first_chars <= MAX_MODEL_VISIBLE_BYTES
        and duplicate_chars == first_chars,
        "code_mode_inline_size_invalid",
    )
    return first_chars


def _require_fixture_payloads_absent(
    records: list[dict[str, Any]], fixture: Any
) -> None:
    streams = (
        response_content_strings(records),
        base64_decoded_strings(response_content_strings(records)),
    )
    for stream in streams:
        values = list(stream)
        require(
            not contains_across_strings(
                iter(values), fixture.emitter_output_bytes.decode("utf-8")
            ),
            "full_stage1_payload_in_rollout",
        )
        require(
            not contains_across_strings(
                iter(values), fixture.window_bytes.decode("utf-8")
            ),
            "full_stage2_payload_in_rollout",
        )


def analyze_code_mode(
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
    **provenance: Any,
) -> dict[str, Any]:
    require(isinstance(rollout_text, str) and records, "rollout_not_valid_jsonl")
    require(model_tool_mode == "code_mode_only", "code_mode_lane_mode_invalid")
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
    output_raw, result, exec_call_id, exec_call_position, exec_output_position = (
        _outer_code_mode_result(records, fixture)
    )
    require(
        set(result) == {"type", "version", "sentinel", "stage1", "stage2"}
        and result.get("type") == CODE_MODE_RESULT_TYPE
        and result.get("version") == 1
        and result.get("sentinel") == SUCCESS_SENTINEL,
        "code_mode_evidence_shape",
    )
    stage1 = result["stage1"]
    require(
        isinstance(stage1, dict) and set(stage1) == {"envelope"},
        "code_mode_stage1_shape",
    )
    envelope = stage1.get("envelope") if isinstance(stage1, dict) else None
    require(isinstance(envelope, dict), "stage1_artifact_missing")
    artifact_id = validate_artifact_id(envelope.get("artifact_id"))
    artifacts = list_thread_artifacts(codex_home, thread_id)
    require(set(artifacts) == {artifact_id}, "unexpected_or_missing_managed_artifact")
    require(artifact_ids_in(result) == {artifact_id}, "unexpected_artifact_reference")
    artifact_data = read_artifact(codex_home, thread_id, artifact_id)
    require(artifact_data == fixture.emitter_output_bytes, "stage1_artifact_content")
    require_artifact_envelope(
        envelope, artifact_id, artifact_data, "text/plain", fixture.shell_middle
    )
    require_top_level_retrievals(
        records,
        exec_call_id=exec_call_id,
        exec_call_position=exec_call_position,
        exec_output_position=exec_output_position,
        artifact_id=artifact_id,
        marker=fixture.shell_middle,
        artifact_data=artifact_data,
    )
    stage2_inline_chars = _require_stage2_summary(result["stage2"], fixture)
    _require_fixture_payloads_absent(records, fixture)
    require(
        final_assistant_text(records) == SUCCESS_SENTINEL, "assistant_sentinel_mismatch"
    )
    context_metrics = require_context_budget(
        records,
        fixture,
        SUCCESS_SENTINEL,
        expected_user_prompt=code_mode_prompt(fixture),
    )
    tool_metrics = tool_response_item_metrics(records)
    complete_metrics = complete_response_item_metrics(records)
    persisted_metrics = persisted_record_metrics(records)
    envelope_bytes = len(
        json.dumps(envelope, ensure_ascii=False, separators=(",", ":")).encode()
    )
    require(envelope_bytes < len(artifact_data), "stage1_artifact_not_economical")
    report = {
        "status": "pass",
        "evaluation": "live_synthetic_integrity_and_byte_economics",
        "evaluation_lane": CODE_MODE_EVALUATION_LANE,
        "byte_economics_scope": "Luna Code Mode Stage 1 mechanism and usage evidence; not broad workflow savings",
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
        "binary_built_in_run": binary_built_in_run,
        "binary_physically_rebuilt_in_run": provenance.get(
            "binary_physically_rebuilt_in_run"
        ),
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
        "stage1_original_artifact_bytes": len(artifact_data),
        "stage1_model_visible_envelope_bytes": envelope_bytes,
        "stage1_envelope_byte_reduction_percent": percentage_reduction(
            len(artifact_data), envelope_bytes
        ),
        "stage2_code_mode_scope": "duplicate reads inline by design; no Stage 2 artifact projection claim",
        "stage2_code_mode_inline_serialized_chars": stage2_inline_chars,
        "rollout_output_body_bytes": len(output_raw.encode()),
        "artifact_store_bytes": len(artifact_data),
        "artifact_count": 1,
        "retrieval_count": 2,
        "max_model_visible_function_output_bytes": len(output_raw.encode()),
        "max_model_visible_tool_item_bytes": tool_metrics["max_item_bytes"],
        "model_visible_tool_item_bytes": tool_metrics["aggregate_bytes"],
        "model_visible_complete_response_item_bytes": complete_metrics[
            "aggregate_bytes"
        ],
        "max_complete_response_item_bytes": complete_metrics["max_item_bytes"],
        "complete_response_item_count": complete_metrics["item_count"],
        "bounded_effective_context_item_bytes": context_metrics.bounded_item_bytes,
        "bounded_effective_context_item_count": context_metrics.bounded_item_count,
        "max_bounded_effective_context_item_bytes": context_metrics.max_bounded_item_bytes,
        "exempt_reasoning_encrypted_item_bytes": context_metrics.exempt_item_bytes,
        "exempt_reasoning_encrypted_item_count": context_metrics.exempt_item_count,
        "max_exempt_reasoning_encrypted_item_bytes": context_metrics.max_exempt_item_bytes,
        "persisted_record_bytes": persisted_metrics["aggregate_bytes"],
        "persisted_record_count": persisted_metrics["record_count"],
        "max_persisted_record_bytes": persisted_metrics["max_record_bytes"],
        "invariants": {
            "terminal_events_and_usage": True,
            "state_db_rollout_exec_usage_agree": True,
            "outer_code_mode_program_exact": True,
            "stage1_artifact_recoverable": True,
            "top_level_retrieval_exact_bounded_and_structured": True,
            "stage2_duplicate_inline_by_design": True,
            "stage2_artifact_projection_not_claimed": True,
            "model_visible_outputs_bounded": True,
            "full_fixture_payloads_not_in_rollout": True,
            "success_sentinel_observed": True,
        },
    }
    report.update(
        {
            "catalog_sha256": provenance.get("catalog_sha256", "unknown"),
            "catalog_entry_sha256": provenance.get("catalog_entry_sha256", "unknown"),
            "harness_sha256": provenance.get("harness_sha256", "unknown"),
            "binary_sha256_before": provenance.get("binary_sha256_before", "unknown"),
            "tracked_worktree_clean_before": provenance.get(
                "tracked_worktree_clean_before"
            ),
            "tracked_worktree_clean_after": provenance.get(
                "tracked_worktree_clean_after"
            ),
            "repository_status_digest_before": provenance.get(
                "status_digest_before", "unknown"
            ),
            "repository_status_digest_after": provenance.get(
                "status_digest_after", "unknown"
            ),
        }
    )
    return report
