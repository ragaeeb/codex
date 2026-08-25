"""Paired and live lifecycle runner for the Stage 3 argument-repair evaluation."""

import json
import math
import re
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any

from tool_output_savings_cleanup import delete_thread
from tool_output_savings_cleanup import resolve_database
from tool_output_savings_cleanup import verify_thread_deleted
from tool_output_savings_code_mode_build import CodeModeBuild
from tool_output_savings_code_mode_build import build_code_mode_release
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import measure_thread
from tool_output_savings_evidence import require
from tool_output_savings_fixture import build_fixture
from tool_output_savings_fixture import cli_version
from tool_output_savings_fixture import git_commit
from tool_output_savings_fixture import initialize_git_fixture
from tool_output_savings_fixture import is_repo_target_cli
from tool_output_savings_fixture import model_catalog_entry_digest
from tool_output_savings_fixture import model_tool_mode
from tool_output_savings_fixture import resolve_cli
from tool_output_savings_fixture import sha256_file
from tool_output_savings_fixture import tracked_working_tree_dirty
from tool_output_savings_fixture import validate_reasoning_effort
from tool_output_savings_fixture import working_tree_dirty
from tool_output_savings_fixture import working_tree_status_digest
from tool_output_savings_lifecycle import _remove_fixture
from tool_output_savings_lifecycle import _write_cleanup_receipt
from tool_output_savings_options import resolve_codex_home
from tool_output_savings_process import run_cli
from tool_output_savings_report import failure_report
from tool_output_savings_report import redacted_report
from tool_output_savings_runtime_provenance import (
    harness_digest as compute_harness_digest,
)
from tool_output_savings_runtime_provenance import stable_worktree_snapshot
from tool_output_savings_stage3 import STAGE3_EVALUATION_LANE
from tool_output_savings_stage3 import STAGE3_FEATURE_COHORT
from tool_output_savings_stage3 import analyze_stage3


def failure_rule(error: BaseException) -> str:
    """Return a content-free rule that still identifies unexpected failure classes."""
    if isinstance(error, HarnessError):
        return str(error)
    class_name = re.sub(r"(?<!^)(?=[A-Z])", "_", type(error).__name__).lower()
    class_name = re.sub(r"[^a-z0-9_]+", "_", class_name).strip("_")[:64]
    return f"stage3_harness_unexpected_failure_{class_name or 'unknown'}"


def paired_deltas(off: dict[str, Any], on: dict[str, Any]) -> dict[str, Any]:
    require(off["task_success"] and on["task_success"], "stage3_pair_task_success")
    require(off["model"] == on["model"], "stage3_pair_model_mismatch")
    require(
        off["reasoning_effort"] == on["reasoning_effort"],
        "stage3_pair_effort_mismatch",
    )
    require(
        off["model_request_count"] >= 1 and on["model_request_count"] >= 1,
        "stage3_pair_request_count",
    )
    require(
        off["model_request_count"] == on["model_request_count"] + 1,
        "stage3_pair_request_reduction",
    )
    fields = (
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    )
    token_deltas = {}
    for field in fields:
        before = off["token_totals"][field]
        after = on["token_totals"][field]
        token_deltas[field] = {
            "off": before,
            "on": after,
            "absolute_delta": after - before,
            "percent_delta": 0.0
            if before == 0
            else round((after - before) * 100 / before, 4),
        }
    off_net = off["net_new_input_tokens"]
    on_net = on["net_new_input_tokens"]
    return {
        "comparable": True,
        "first_attempt_dispatch_rate_percent": {
            "off": 100.0 if off["first_attempt_dispatch_success"] else 0.0,
            "on": 100.0 if on["first_attempt_dispatch_success"] else 0.0,
        },
        "model_request_count": {
            "off": off["model_request_count"],
            "on": on["model_request_count"],
        },
        "token_deltas": token_deltas,
        "net_new_input_tokens": {
            "off": off_net,
            "on": on_net,
            "absolute_delta": on_net - off_net,
            "percent_delta": 0.0
            if off_net == 0
            else round((on_net - off_net) * 100 / off_net, 4),
        },
        "task_success": {"off": True, "on": True},
        "activation": {"off": 0, "on": 1},
        "rule_counts": {"off": off["repair_rule_count"], "on": on["repair_rule_count"]},
    }


def paired_tldr(off: dict[str, Any], on: dict[str, Any], deltas: dict[str, Any]) -> str:
    total_delta = deltas["token_deltas"]["total_tokens"]
    net_delta = deltas["net_new_input_tokens"]
    return (
        "Stage 3 activated on 1/2 live Luna runs; first-attempt dispatch rose "
        f"{deltas['first_attempt_dispatch_rate_percent']['off']:.0f}% -> "
        f"{deltas['first_attempt_dispatch_rate_percent']['on']:.0f}%; model requests fell "
        f"{deltas['model_request_count']['off']} -> {deltas['model_request_count']['on']}; total tokens changed "
        f"{total_delta['off']} -> {total_delta['on']} ({total_delta['percent_delta']:.4f}%); net-new input changed "
        f"{net_delta['off']} -> {net_delta['on']} ({net_delta['percent_delta']:.4f}%); task success changed 1 -> 1. "
        "Synthetic-trigger evidence, n=1 pair; natural-corpus activation/savings: not measured "
        "(one authorized synthetic pair only)."
    )


def run_stage3_pair(
    args: Any,
    *,
    home: Path,
    home_overridden: bool,
    cli: Path,
    code_mode_build: CodeModeBuild,
    catalog_digest: str,
    catalog_entry_digest: str,
    harness_digest: str,
    commit_before: str,
    status_digest_before: str,
    tracked_clean_before: bool,
    timeout: float,
) -> dict[str, Any]:
    """Run exactly one fresh off/on pair; callers own build and provenance checks."""
    reports: dict[str, dict[str, Any]] = {}
    for label, enabled in (("off", False), ("on", True)):
        fixture_root = Path(tempfile.mkdtemp(prefix=f"codex-stage3-{label}-"))
        fixture_root.chmod(0o700)
        fixture = build_fixture(fixture_root, suffix="stage3-paired-fixture")
        initialize_git_fixture(fixture)
        sqlite_home = Path(tempfile.mkdtemp(prefix=f"codex-stage3-sqlite-{label}-"))
        sqlite_home.chmod(0o700)
        database = resolve_database(
            None,
            home,
            home_overridden,
            cwd=fixture.root,
            sqlite_home_override=sqlite_home,
            ignore_user_config=True,
            require_existing=False,
        )
        thread_id = None
        rollout_path = None
        report = None
        try:
            events, thread_id, terminal, _, _ = run_cli(
                cli,
                fixture,
                model=args.model,
                reasoning_effort=args.reasoning_effort,
                model_tool_mode_value="code_mode_only",
                codex_home=home,
                sqlite_home=sqlite_home,
                timeout=timeout,
                tool_argument_repair=enabled,
                stage3=True,
            )
            version = cli_version(cli, min(timeout, 30.0))
            row, rollout_path, rollout_text, records, _ = measure_thread(
                database,
                thread_id,
                timeout=min(timeout, 20.0),
                terminal_usage=terminal,
                model=args.model,
                reasoning_effort=args.reasoning_effort,
                cli_version=version,
            )
            report = analyze_stage3(
                events,
                rollout_text,
                records,
                fixture,
                home,
                thread_id,
                row,
                terminal,
                feature_enabled=enabled,
                cli_version=version,
                model=args.model,
                reasoning_effort=args.reasoning_effort,
                binary_sha256=sha256_file(cli),
                binary_built_in_run=True,
                binary_physically_rebuilt_in_run=True,
                code_mode_host_sha256=code_mode_build.code_mode_host_sha256,
                catalog_sha256=catalog_digest,
                catalog_entry_sha256=catalog_entry_digest,
                harness_sha256=harness_digest,
            )
            reports[label] = report
        except BaseException as error:
            if thread_id is None:
                recovered_thread_id = getattr(error, "thread_id", None)
                if isinstance(recovered_thread_id, str):
                    thread_id = recovered_thread_id
            if isinstance(error, HarnessError):
                raise HarnessError(f"stage3_{label}_arm_{error}") from error
            raise
        finally:
            if not args.keep:
                try:
                    if thread_id is not None:
                        targets = delete_thread(
                            cli,
                            thread_id,
                            home,
                            sqlite_home,
                            timeout,
                            database=database,
                            rollout_path=rollout_path,
                        )
                        verify_thread_deleted(targets)
                except BaseException as cleanup_error:
                    try:
                        receipt = _write_cleanup_receipt(
                            fixture_root,
                            thread_id,
                            home,
                            database,
                            cli,
                            sqlite_home,
                            failure_detail=str(cleanup_error),
                        )
                        print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
                    except BaseException:
                        pass
                    raise
                if fixture_root.exists():
                    _remove_fixture(fixture_root)
                if sqlite_home.exists():
                    shutil.rmtree(sqlite_home)
                    require(
                        not sqlite_home.exists(), "stage3_sqlite_cleanup_not_verified"
                    )
    deltas = paired_deltas(reports["off"], reports["on"])
    return {
        "status": "pass",
        "evaluation": "live_stage3_tool_argument_repair_paired_ab",
        "evaluation_lane": STAGE3_EVALUATION_LANE,
        "sample_size": 2,
        "synthetic_trigger": True,
        "repair_cohort": STAGE3_FEATURE_COHORT,
        "runs": reports,
        "paired": deltas,
        "tldr": paired_tldr(reports["off"], reports["on"], deltas),
        "natural_corpus": {
            "status": "not_measured",
            "activation_frequency": None,
            "savings": "inconclusive",
            "reason": "one_authorized_synthetic_pair_only",
        },
        "source_provenance": {
            "repository_commit": commit_before,
            "status_digest_before": status_digest_before,
            "tracked_worktree_clean_before": tracked_clean_before,
            "working_tree_dirty": working_tree_dirty(),
            "cli_sha256": sha256_file(cli),
            "code_mode_host_sha256": code_mode_build.code_mode_host_sha256,
            "catalog_sha256": catalog_digest,
            "catalog_entry_sha256": catalog_entry_digest,
            "harness_sha256": harness_digest,
        },
    }


def run_stage3_live(args: Any) -> int:
    """Build the local Code Mode release and run the one authorized Luna pair."""
    started = __import__("time").monotonic()
    home, home_overridden = resolve_codex_home(args.codex_home)
    catalog = (
        Path(__file__).resolve().parents[2] / "codex-rs/models-manager/models.json"
    )
    model_mode = "unknown"
    cli = None
    code_mode_build = None
    observed_source_head = None
    binary_digest = "unknown"
    try:
        require(args.live, "live_eval_required")
        require(sys.platform == "darwin", "live_eval_scope_macos_only")
        require(
            math.isfinite(args.timeout) and 0 < args.timeout <= 900.0,
            "timeout_invalid",
        )
        model_mode = model_tool_mode(catalog, args.model)
        require(model_mode == "code_mode_only", "stage3_requires_code_mode_only_model")
        catalog_digest = sha256_file(catalog)
        catalog_entry_digest = model_catalog_entry_digest(catalog, args.model)
        validate_reasoning_effort(catalog, args.model, args.reasoning_effort)
        harness_digest = compute_harness_digest(Path(__file__).parent)
        commit_before = git_commit()
        observed_source_head = commit_before
        status_digest_before = working_tree_status_digest()
        tracked_clean_before = not tracked_working_tree_dirty()
        require(args.build, "live_source_catalog_requires_build")
        code_mode_build = build_code_mode_release()
        cli = resolve_cli(
            args.codex_cli if args.codex_cli is not None else code_mode_build.cli
        )
        require(
            is_repo_target_cli(cli),
            "cli_provenance_unverified_use_repo_target_or_build",
        )
        require(
            cli == code_mode_build.cli,
            "binary_provenance_unverified_use_built_cli",
        )
        version = cli_version(cli, min(args.timeout, 30.0))
        binary_digest = sha256_file(cli)
        require(
            stable_worktree_snapshot(
                status_digest_before,
                tracked_clean_before,
                working_tree_status_digest(),
                not tracked_working_tree_dirty(),
            )
            and git_commit() == commit_before
            and sha256_file(catalog) == catalog_digest
            and model_catalog_entry_digest(catalog, args.model) == catalog_entry_digest,
            "source_provenance_changed_during_build",
        )
        report = run_stage3_pair(
            args,
            home=home,
            home_overridden=home_overridden,
            cli=cli,
            code_mode_build=code_mode_build,
            catalog_digest=catalog_digest,
            catalog_entry_digest=catalog_entry_digest,
            harness_digest=harness_digest,
            commit_before=commit_before,
            status_digest_before=status_digest_before,
            tracked_clean_before=tracked_clean_before,
            timeout=args.timeout,
        )
        require(sha256_file(cli) == binary_digest, "binary_changed_during_run")
        require(
            sha256_file(code_mode_build.code_mode_host)
            == code_mode_build.code_mode_host_sha256,
            "code_mode_host_changed_during_run",
        )
        require(
            git_commit() == commit_before
            and stable_worktree_snapshot(
                status_digest_before,
                tracked_clean_before,
                working_tree_status_digest(),
                not tracked_working_tree_dirty(),
            )
            and sha256_file(catalog) == catalog_digest
            and model_catalog_entry_digest(catalog, args.model) == catalog_entry_digest
            and compute_harness_digest(Path(__file__).parent) == harness_digest,
            "source_provenance_changed_during_run",
        )
        report.update(
            {
                "model": args.model,
                "model_tool_mode": model_mode,
                "reasoning_effort": args.reasoning_effort,
                "cli_version": version,
                "binary_sha256": binary_digest,
                "binary_sha256_before": binary_digest,
                "binary_built_in_run": True,
                "binary_physically_rebuilt_in_run": True,
                "code_mode_host_sha256": code_mode_build.code_mode_host_sha256,
                "code_mode_host_sha256_before": code_mode_build.code_mode_host_sha256,
                "rusty_v8_version": code_mode_build.rusty_v8_version,
                "rust_target": code_mode_build.rust_target,
                "rusty_v8_archive_sha256": code_mode_build.rusty_v8_archive_sha256,
                "rusty_v8_bindings_sha256": code_mode_build.rusty_v8_bindings_sha256,
                "observed_source_head": observed_source_head,
                "working_tree_dirty": working_tree_dirty(),
                "duration_seconds": round(__import__("time").monotonic() - started, 3),
            }
        )
        report = redacted_report(report)
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        return 0
    except BaseException as error:
        rule = failure_rule(error)
        report = failure_report(
            args.model,
            args.reasoning_effort,
            rule,
            started,
            model_tool_mode=model_mode,
            cli_version="unknown",
            binary_sha256=binary_digest,
            binary_built_in_run=code_mode_build is not None,
            observed_source_head=observed_source_head,
        )
        report.update(
            {
                "evaluation": "live_stage3_tool_argument_repair_paired_ab",
                "evaluation_lane": STAGE3_EVALUATION_LANE,
                "sample_size": 0,
                "synthetic_trigger": True,
                "natural_corpus": {
                    "status": "not_measured",
                    "activation_frequency": None,
                    "savings": "inconclusive",
                    "reason": "stage3_pair_blocked",
                },
            }
        )
        print(
            json.dumps(redacted_report(report), sort_keys=True, separators=(",", ":"))
        )
        return 1
