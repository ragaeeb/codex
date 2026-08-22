"""Content-free report construction for the external savings evaluation."""

import time
from typing import Any

from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_evidence import USAGE_FIELDS
from tool_output_savings_evidence import ThreadRow
from tool_output_savings_evidence import TokenEvidence
from tool_output_savings_evidence import short_hash
from tool_output_savings_context_budget import CONTEXT_TOKEN_ESTIMATOR_BYTES_PER_TOKEN
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_APPROX_TOKENS
from tool_output_savings_context_budget import MAX_EFFECTIVE_CONTEXT_ITEM_BYTES


def redacted_report(report: dict[str, Any]) -> dict[str, Any]:
    allowed = {
        "status",
        "evaluation",
        "evaluation_lane",
        "byte_economics_scope",
        "model",
        "model_tool_mode",
        "model_visible_cap_bytes",
        "model_visible_conservative_token_ceiling_tokens",
        "tool_output_model_visible_cap_bytes",
        "effective_context_item_approx_token_cap",
        "context_token_estimator_bytes_per_token",
        "effective_context_item_approx_byte_cap",
        "manual_review_threshold_tokens",
        "manual_review_required",
        "manual_review_status",
        "manual_review_evidence",
        "reasoning_effort",
        "cli_version",
        "binary_sha256",
        "binary_sha256_before",
        "binary_built_in_run",
        "binary_physically_rebuilt_in_run",
        "catalog_sha256",
        "catalog_entry_sha256",
        "harness_sha256",
        "tracked_worktree_clean_before",
        "tracked_worktree_clean_after",
        "repository_status_digest_before",
        "repository_status_digest_after",
        "repository_commit",
        "repository_commit_attribution",
        "observed_source_head",
        "binary_reproducibility",
        "working_tree_dirty",
        "thread_hash",
        "db_model",
        "db_reasoning_effort",
        "db_cli_version",
        "db_indexed_total_tokens",
        "rollout_token_usage",
        "rollout_last_token_usage",
        "exec_terminal_usage",
        "usage_agreement",
        "stage1_original_artifact_bytes",
        "stage1_model_visible_envelope_bytes",
        "stage1_envelope_byte_reduction_percent",
        "stage2_hypothetical_duplicate_inline_bytes",
        "stage2_duplicate_envelope_bytes",
        "stage2_incremental_byte_reduction_percent",
        "stage2_code_mode_scope",
        "stage2_code_mode_inline_serialized_chars",
        "code_mode_host_sha256",
        "code_mode_host_sha256_before",
        "code_mode_host_built_in_run",
        "code_mode_host_physically_rebuilt_in_run",
        "rusty_v8_version",
        "rust_target",
        "rusty_v8_archive_sha256",
        "rusty_v8_bindings_sha256",
        "rollout_output_body_bytes",
        "artifact_store_bytes",
        "artifact_count",
        "retrieval_count",
        "max_model_visible_function_output_bytes",
        "max_model_visible_tool_item_bytes",
        "model_visible_tool_item_bytes",
        "model_visible_complete_response_item_bytes",
        "max_complete_response_item_bytes",
        "complete_response_item_count",
        "bounded_effective_context_item_bytes",
        "bounded_effective_context_item_count",
        "max_bounded_effective_context_item_bytes",
        "exempt_reasoning_encrypted_item_bytes",
        "exempt_reasoning_encrypted_item_count",
        "max_exempt_reasoning_encrypted_item_bytes",
        "stage1_persisted_fixture_content_bytes",
        "stage1_persisted_fixture_content_budget_bytes",
        "stage2_persisted_fixture_content_bytes",
        "stage2_persisted_fixture_content_budget_bytes",
        "persisted_record_bytes",
        "persisted_record_count",
        "max_persisted_record_bytes",
        "invariants",
        "cleanup",
        "failure_rule",
        "cleanup_failure_rule",
        "duration_seconds",
    }
    return {key: value for key, value in report.items() if key in allowed}


def mark_unattributed_build_provenance(
    report: dict[str, Any], observed_source_head: str | None
) -> dict[str, Any]:
    """Separate an observed checkout HEAD from unauthenticated build inputs."""
    report.update(
        {
            "observed_source_head": observed_source_head or "unavailable",
            "repository_commit": "not_attributed",
            "repository_commit_attribution": "not_claimed_external_build_inputs_unverified",
            "binary_reproducibility": "external_build_inputs_unverified",
        }
    )
    return report


def failure_report(
    model: str,
    reasoning_effort: str,
    rule: str,
    started: float,
    *,
    model_tool_mode: str = "unknown",
    cli_version: str = "unknown",
    binary_sha256: str = "unknown",
    binary_built_in_run: bool = False,
    binary_physically_rebuilt_in_run: bool | None = None,
    observed_source_head: str | None = None,
    thread_id: str | None = None,
    row: ThreadRow | None = None,
    rollout_usage: TokenEvidence | None = None,
    terminal: dict[str, int] | None = None,
) -> dict[str, Any]:
    agreement = {
        "db_equals_rollout_total": row is not None
        and rollout_usage is not None
        and row.tokens_used == rollout_usage.total["total_tokens"],
        "exec_components_equal_rollout": terminal is not None
        and rollout_usage is not None
        and all(
            terminal.get(field) == rollout_usage.total[field] for field in USAGE_FIELDS
        ),
    }
    return redacted_report(
        mark_unattributed_build_provenance(
            {
                "status": "fail",
                "evaluation": "live_synthetic_integrity_and_byte_economics",
                "byte_economics_scope": "mechanism evidence only; incomplete because the live lane failed",
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
                "binary_physically_rebuilt_in_run": binary_physically_rebuilt_in_run,
                "thread_hash": short_hash(thread_id) if thread_id else "unavailable",
                "db_model": row.model if row else None,
                "db_reasoning_effort": row.reasoning_effort if row else None,
                "db_cli_version": row.cli_version if row else None,
                "failure_rule": rule,
                "duration_seconds": round(time.monotonic() - started, 3),
                "db_indexed_total_tokens": row.tokens_used if row else None,
                "rollout_token_usage": rollout_usage.total if rollout_usage else None,
                "rollout_last_token_usage": rollout_usage.last
                if rollout_usage
                else None,
                "exec_terminal_usage": terminal,
                "usage_agreement": agreement,
                "invariants": {
                    "live_run_completed": False,
                    "state_db_rollout_exec_agreement": all(agreement.values()),
                },
            },
            observed_source_head,
        )
    )
