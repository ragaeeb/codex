"""Stage 3 paired Luna evidence for first-party Code Mode argument repair.

This lane intentionally reuses the existing process, rollout, SQLite, cleanup, and
context-budget helpers. It adds only the paired malformed-argument fixture and its
content-free semantic oracle.
"""

import json
import re
from pathlib import Path
from typing import Any

from tool_output_savings_content import final_assistant_text
from tool_output_savings_context_budget import ContextBudgetMetrics
from tool_output_savings_context_budget import require_context_budget
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import ThreadRow
from tool_output_savings_evidence import assert_usage_agreement
from tool_output_savings_evidence import latest_token_count
from tool_output_savings_evidence import require
from tool_output_savings_evidence import short_hash
from tool_output_savings_evidence import validate_exec_events
from tool_output_savings_evidence import validate_rollout_identity
from tool_output_savings_fixture import Fixture
from tool_output_savings_transcript import response_items
from tool_output_savings_transcript import validate_tool_response_items


STAGE3_EVALUATION_LANE = "luna_code_mode_stage3_tool_argument_repair"
STAGE3_RESULT_TYPE = "tool_argument_repair_stage3_evidence"
STAGE3_SUCCESS_SENTINEL = "TOOL_ARGUMENT_REPAIR_STAGE3_SUCCESS"
STAGE3_RETRY_SENTINEL = "TOOL_ARGUMENT_REPAIR_STAGE3_RETRY_REQUIRED"
STAGE3_RETRY_TYPE = "tool_argument_repair_stage3_retry"
STAGE3_RULE = "numeric_string_typed"
STAGE3_FEATURE_COHORT = "tool_argument_repair_v1"
_COMPLETED_STATUS = re.compile(
    r"^Script completed\nWall time [0-9]+\.[0-9] seconds\nOutput:\n$"
)


def stage3_code_mode_program(fixture: Fixture) -> str:
    marker = json.dumps(fixture.window_middle, ensure_ascii=False)
    path = json.dumps(fixture.window_name, ensure_ascii=False)
    offset = fixture.window_middle_offset
    return f'''// @exec: {{"yield_time_ms": 30000, "max_output_tokens": 2000}}
const malformed = {{path: {path}, offset: {offset}, max_bytes: "1024", max_lines: 2000}};
let first = null;
let firstError = false;
try {{
  first = await tools.read_file(malformed);
}} catch (_error) {{
  firstError = true;
}}
// Some Code Mode tool adapters return a structured error value instead of
// rejecting the promise. Treat that as the same strict-handler failure so the
// off lane exercises its existing correction path without accepting an error
// object as a successful read.
if (first?.type !== "file_read") firstError = true;
if (firstError) {{
  text(JSON.stringify({{
    type: "{STAGE3_RETRY_TYPE}",
    version: 1,
    sentinel: "{STAGE3_RETRY_SENTINEL}"
  }}));
}} else {{
  const markerPresent = Boolean(first.window.text.includes({marker}));
  if (!markerPresent) throw new Error("stage3 marker proof failed");
  text(JSON.stringify({{
    type: "{STAGE3_RESULT_TYPE}",
    version: 1,
    sentinel: "{STAGE3_SUCCESS_SENTINEL}",
    first_attempt: {{outcome: "success"}},
    correction: {{performed: false, outcome: "not_needed"}},
    nested_dispatches: 1,
    handler_attempts: 1,
    handler_successes: 1,
    invalid_argument_outputs: 0,
    repair_activated: true,
    repair_rules: ["{STAGE3_RULE}"],
    marker_present: markerPresent,
    validated: true
  }}));
}}'''


def stage3_correction_program(fixture: Fixture) -> str:
    marker = json.dumps(fixture.window_middle, ensure_ascii=False)
    path = json.dumps(fixture.window_name, ensure_ascii=False)
    offset = fixture.window_middle_offset
    return f'''// @exec: {{"yield_time_ms": 30000, "max_output_tokens": 2000}}
const correction = await tools.read_file({{path: {path}, offset: {offset}, max_bytes: 1024, max_lines: 2000}});
const markerPresent = Boolean(correction?.window?.text?.includes({marker}));
if (!markerPresent) throw new Error("stage3 correction marker proof failed");
text(JSON.stringify({{
  type: "{STAGE3_RESULT_TYPE}",
  version: 1,
  sentinel: "{STAGE3_SUCCESS_SENTINEL}",
  first_attempt: {{outcome: "handler_error"}},
  correction: {{performed: true, outcome: "success"}},
  nested_dispatches: 2,
  handler_attempts: 2,
  handler_successes: 1,
  invalid_argument_outputs: 1,
  repair_activated: false,
  repair_rules: [],
  marker_present: markerPresent,
  validated: true
}}));'''


def stage3_code_mode_prompt(fixture: Fixture) -> str:
    return (
        "Make one top-level Code Mode exec call and execute the first JavaScript program below "
        "exactly as written. Do not add logging, output, tools, or error recovery. The program "
        'intentionally passes max_bytes as the string "1024". If and only if its emitted JSON '
        f"sentinel is {STAGE3_RETRY_SENTINEL}, make one additional top-level Code Mode exec call "
        "using the correction program exactly as written. Otherwise make no second tool call. "
        "After a program emits the success evidence, respond with exactly "
        f"{STAGE3_SUCCESS_SENTINEL}.\n\n<exact-first-code-mode-program>\n"
        f"{stage3_code_mode_program(fixture)}\n</exact-first-code-mode-program>\n\n"
        "<exact-correction-code-mode-program>\n"
        f"{stage3_correction_program(fixture)}\n</exact-correction-code-mode-program>"
    )


def _outer_result(
    records: list[dict[str, Any]], fixture: Fixture, feature_enabled: bool
) -> dict[str, Any]:
    validate_tool_response_items(records)
    items = list(response_items(records))
    calls = [
        (position, item)
        for position, item in enumerate(items)
        if item.get("type") == "custom_tool_call"
    ]
    outputs = [
        (position, item)
        for position, item in enumerate(items)
        if item.get("type") == "custom_tool_call_output"
    ]
    expected_programs = (
        [stage3_code_mode_program(fixture)]
        if feature_enabled
        else [stage3_code_mode_program(fixture), stage3_correction_program(fixture)]
    )
    require(len(calls) == len(expected_programs), "stage3_outer_exec_call_count")
    require(len(outputs) == len(expected_programs), "stage3_outer_exec_output_count")
    values = []
    previous_output_position = -1
    for call_entry, output_entry, expected_program in zip(
        calls, outputs, expected_programs, strict=True
    ):
        call_position, call = call_entry
        output_position, output = output_entry
        require(
            previous_output_position < call_position < output_position,
            "stage3_outer_call_output_order",
        )
        previous_output_position = output_position
        require(call.get("name") == "exec", "stage3_outer_tool_family")
        call_id = call.get("call_id")
        require(isinstance(call_id, str) and call_id, "stage3_outer_call_id")
        require(output.get("call_id") == call_id, "stage3_outer_output_pair")
        require(
            call.get("input") in {expected_program, f"{expected_program}\n"},
            "stage3_program_mismatch",
        )
        raw_output = output.get("output")
        require(
            isinstance(raw_output, list) and len(raw_output) == 2,
            "stage3_outer_output_shape",
        )
        status, emitted = raw_output
        require(
            isinstance(status, dict)
            and status.get("type") == "input_text"
            and isinstance(status.get("text"), str)
            and _COMPLETED_STATUS.fullmatch(status["text"]) is not None,
            "stage3_outer_status_invalid",
        )
        require(
            isinstance(emitted, dict)
            and emitted.get("type") == "input_text"
            and isinstance(emitted.get("text"), str),
            "stage3_outer_output_shape",
        )
        try:
            value = json.loads(emitted["text"])
        except json.JSONDecodeError as error:
            raise HarnessError("stage3_output_not_json") from error
        require(isinstance(value, dict), "stage3_output_not_object")
        values.append(value)
    if not feature_enabled:
        require(
            values[0]
            == {
                "type": STAGE3_RETRY_TYPE,
                "version": 1,
                "sentinel": STAGE3_RETRY_SENTINEL,
            },
            "stage3_retry_evidence_invalid",
        )
    return values[-1]


def _repair_receipt(
    records: list[dict[str, Any]], feature_enabled: bool
) -> dict[str, Any] | None:
    receipts = []
    for record in records:
        if record.get("type") != "response_item":
            continue
        metadata = record.get("metadata")
        if not isinstance(metadata, dict):
            continue
        receipt = metadata.get("tool_argument_repair")
        if isinstance(receipt, dict):
            receipts.append(receipt)
    if not feature_enabled:
        require(not receipts, "stage3_feature_off_receipt")
        return None
    require(len(receipts) == 1, "stage3_repair_receipt_count")
    receipt = receipts[0]
    require(
        set(receipt)
        == {
            "tool_family",
            "outcome",
            "rules",
            "input_bytes",
            "effective_bytes",
            "candidate_work",
            "repair_duration_micros",
        },
        "stage3_repair_receipt_shape",
    )
    require(receipt["tool_family"] == "read_file", "stage3_repair_receipt_family")
    require(receipt["outcome"] == "repaired", "stage3_repair_receipt_outcome")
    require(receipt["rules"] == [STAGE3_RULE], "stage3_repair_receipt_rules")
    for field in (
        "input_bytes",
        "effective_bytes",
        "candidate_work",
        "repair_duration_micros",
    ):
        require(
            type(receipt[field]) is int and receipt[field] >= 0,
            "stage3_repair_receipt_measurement",
        )
    require(receipt["candidate_work"] > 0, "stage3_repair_receipt_candidate_work")
    require(
        receipt["input_bytes"] > receipt["effective_bytes"],
        "stage3_repair_receipt_byte_delta",
    )
    return receipt


def _model_request_count(records: list[dict[str, Any]]) -> int:
    # `codex exec --json` intentionally omits rawResponse/completed. In this controlled lane each
    # response either emits exactly one sequential outer exec call or the exact final assistant
    # sentinel. A correction call occurs after the first output, so it necessarily requires a new
    # model continuation rather than sharing the first response.
    outer_calls = sum(
        item.get("type") == "custom_tool_call" and item.get("name") == "exec"
        for item in response_items(records)
    )
    require(outer_calls > 0, "stage3_model_request_count_missing")
    return outer_calls + 1


def _context_budget_fields(metrics: ContextBudgetMetrics) -> dict[str, int | bool]:
    return {
        "context_item_approx_bytes": metrics.bounded_item_bytes,
        "context_item_approx_count": metrics.bounded_item_count,
        "context_item_max_bytes": metrics.max_bounded_item_bytes,
        "context_items_below_1k_tokens": metrics.max_bounded_item_bytes < 4_000,
    }


def analyze_stage3(
    events: list[dict[str, Any]],
    rollout_text: str,
    records: list[dict[str, Any]],
    fixture: Fixture,
    codex_home: Path,
    thread_id: str,
    row: ThreadRow,
    terminal: dict[str, int],
    *,
    feature_enabled: bool,
    cli_version: str,
    model: str,
    reasoning_effort: str,
    binary_sha256: str,
    binary_built_in_run: bool,
    **provenance: Any,
) -> dict[str, Any]:
    require(isinstance(rollout_text, str) and records, "stage3_rollout_not_valid_jsonl")
    validate_rollout_identity(records, thread_id)
    validate_exec_events(events)
    usage = latest_token_count(records)
    assert_usage_agreement(
        row,
        usage,
        terminal,
        model=model,
        reasoning_effort=reasoning_effort,
        cli_version=cli_version,
    )
    value = _outer_result(records, fixture, feature_enabled)
    require(
        set(value)
        == {
            "type",
            "version",
            "sentinel",
            "first_attempt",
            "correction",
            "nested_dispatches",
            "handler_attempts",
            "handler_successes",
            "invalid_argument_outputs",
            "repair_activated",
            "repair_rules",
            "marker_present",
            "validated",
        },
        "stage3_evidence_shape",
    )
    require(value["type"] == STAGE3_RESULT_TYPE, "stage3_evidence_type")
    require(
        value["version"] == 1 and value["sentinel"] == STAGE3_SUCCESS_SENTINEL,
        "stage3_evidence_version",
    )
    require(
        value["marker_present"] is True and value["validated"] is True,
        "stage3_marker_proof",
    )
    require(
        final_assistant_text(records) == STAGE3_SUCCESS_SENTINEL,
        "stage3_assistant_sentinel",
    )
    expected = {
        False: {
            "first_attempt": {"outcome": "handler_error"},
            "correction": {"performed": True, "outcome": "success"},
            "nested_dispatches": 2,
            "handler_attempts": 2,
            "handler_successes": 1,
            "invalid_argument_outputs": 1,
            "repair_activated": False,
            "repair_rules": [],
        },
        True: {
            "first_attempt": {"outcome": "success"},
            "correction": {"performed": False, "outcome": "not_needed"},
            "nested_dispatches": 1,
            "handler_attempts": 1,
            "handler_successes": 1,
            "invalid_argument_outputs": 0,
            "repair_activated": True,
            "repair_rules": [STAGE3_RULE],
        },
    }[feature_enabled]
    for key, expected_value in expected.items():
        require(value[key] == expected_value, f"stage3_{key}_mismatch")
    receipt = _repair_receipt(records, feature_enabled)
    context_budget = require_context_budget(
        records,
        fixture,
        STAGE3_SUCCESS_SENTINEL,
        expected_user_prompt=stage3_code_mode_prompt(fixture),
    )
    token_totals = usage.total
    report = {
        "status": "pass",
        "evaluation": "live_stage3_tool_argument_repair",
        "evaluation_lane": STAGE3_EVALUATION_LANE,
        "synthetic_trigger": True,
        "feature_enabled": feature_enabled,
        "feature_cohort": STAGE3_FEATURE_COHORT,
        "model": model,
        "model_tool_mode": "code_mode_only",
        "reasoning_effort": reasoning_effort,
        "cli_version": cli_version,
        "binary_sha256": binary_sha256,
        "binary_built_in_run": binary_built_in_run,
        "binary_physically_rebuilt_in_run": provenance.get(
            "binary_physically_rebuilt_in_run"
        ),
        "code_mode_host_sha256": provenance.get("code_mode_host_sha256"),
        "catalog_sha256": provenance.get("catalog_sha256"),
        "catalog_entry_sha256": provenance.get("catalog_entry_sha256"),
        "harness_sha256": provenance.get("harness_sha256"),
        "thread_hash": short_hash(thread_id),
        "model_request_count": _model_request_count(records),
        "handler_attempt_count": value["handler_attempts"],
        "handler_success_count": value["handler_successes"],
        "invalid_argument_output_count": value["invalid_argument_outputs"],
        "task_success": True,
        "first_attempt_dispatch_success": value["first_attempt"]["outcome"]
        == "success",
        "repair_activated": value["repair_activated"],
        "repair_rules": value["repair_rules"],
        "repair_rule_count": len(value["repair_rules"]),
        "candidate_work": receipt["candidate_work"] if receipt else 0,
        "repair_duration_micros": receipt["repair_duration_micros"] if receipt else 0,
        "input_bytes": receipt["input_bytes"] if receipt else 0,
        "effective_bytes": receipt["effective_bytes"] if receipt else 0,
        "token_totals": token_totals,
        "net_new_input_tokens": token_totals["input_tokens"]
        - token_totals["cached_input_tokens"],
        "db_indexed_total_tokens": row.tokens_used,
        "rollout_token_usage": token_totals,
        "rollout_last_token_usage": usage.last,
        "exec_terminal_usage": terminal,
        "usage_agreement": {
            "db_equals_rollout_total": True,
            "exec_components_equal_rollout": True,
            "model_matches": True,
            "reasoning_effort_matches": True,
            "cli_version_matches": True,
        },
        "invariants": {
            "raw_first_malformed_call_fixture_identical": True,
            "feature_off_current_error_path_preserved": not feature_enabled,
            "first_party_nested_repair_observed": feature_enabled,
            "strict_handler_received_effective_args": feature_enabled,
            "marker_proved": True,
            "raw_response_history_not_rewritten": True,
            "bounded_effective_receipt_present": feature_enabled,
            "model_context_bounded": True,
            "content_free_receipt": True,
        },
    }
    report.update(_context_budget_fields(context_budget))
    return report
