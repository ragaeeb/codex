#!/usr/bin/env python3
"""Opt-in external E2E evaluation for bounded tool-output projections."""

import json
import math
import sys
import tempfile
import time
from pathlib import Path

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import measure_thread
from tool_output_savings_evidence import require
from tool_output_savings_fixture import build_release
from tool_output_savings_fixture import cli_version
from tool_output_savings_fixture import git_commit
from tool_output_savings_fixture import model_tool_mode
from tool_output_savings_fixture import model_catalog_entry_digest
from tool_output_savings_fixture import resolve_cli
from tool_output_savings_process import run_cli
from tool_output_savings_fixture import sha256_file
from tool_output_savings_fixture import build_fixture
from tool_output_savings_fixture import initialize_git_fixture
from tool_output_savings_fixture import is_repo_target_cli
from tool_output_savings_cleanup import delete_thread
from tool_output_savings_cleanup import resolve_database
from tool_output_savings_cleanup import verify_thread_deleted
from tool_output_savings_analysis import analyze
from tool_output_savings_report import failure_report
from tool_output_savings_report import mark_unattributed_build_provenance
from tool_output_savings_report import redacted_report
from tool_output_savings_lifecycle import _cleanup_report
from tool_output_savings_lifecycle import _remove_fixture
from tool_output_savings_lifecycle import _write_cleanup_receipt
from tool_output_savings_lifecycle import _write_content_free_report
from tool_output_savings_evidence import short_hash
from tool_output_savings_fixture import REPO_ROOT
from tool_output_savings_fixture import working_tree_dirty
from tool_output_savings_fixture import tracked_working_tree_dirty
from tool_output_savings_fixture import validate_reasoning_effort
from tool_output_savings_fixture import working_tree_status_digest
from tool_output_savings_code_mode_build import build_code_mode_release
from tool_output_savings_code_mode_lane import analyze_code_mode
from tool_output_savings_options import build_parser
from tool_output_savings_options import evaluation_lane as lane_for_tool_mode
from tool_output_savings_options import resolve_codex_home
from tool_output_savings_runtime_provenance import (
    harness_digest as compute_harness_digest,
)
from tool_output_savings_runtime_provenance import stable_worktree_snapshot


CATALOG_PATH = REPO_ROOT / "codex-rs/models-manager/models.json"


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if not args.live:
        parser.error(
            "--live is required; offline helpers are covered by sibling unittest modules"
        )
    started = time.monotonic()
    fixture_root: Path | None = None
    thread_id: str | None = None
    row = None
    rollout_usage = None
    terminal = None
    rollout_path: Path | None = None
    database: Path | None = None
    cli: Path | None = None
    built_cli: Path | None = None
    sqlite_home: Path | None = None
    home, home_overridden = resolve_codex_home(args.codex_home)
    model_mode = "unknown"
    version = "unknown"
    binary_digest = "unknown"
    observed_source_head: str | None = None
    catalog_digest = "unknown"
    catalog_entry_digest = "unknown"
    harness_digest = "unknown"
    binary_digest_before = "unknown"
    tracked_clean_before = None
    tracked_clean_after = None
    status_digest_before = "unknown"
    status_digest_after = "unknown"
    binary_built_in_run = False
    binary_physically_rebuilt_in_run = False
    evaluation_lane = "unresolved"
    code_mode_build = None
    code_mode_host_digest_before = "unknown"
    try:
        require(sys.platform == "darwin", "live_eval_scope_macos_only")
        require(
            math.isfinite(args.timeout) and 0 < args.timeout <= 900.0,
            "timeout_invalid",
        )
        catalog_digest = sha256_file(CATALOG_PATH)
        catalog_entry_digest = model_catalog_entry_digest(CATALOG_PATH, args.model)
        validate_reasoning_effort(CATALOG_PATH, args.model, args.reasoning_effort)
        harness_digest = compute_harness_digest(Path(__file__).parent)
        commit_before = git_commit()
        observed_source_head = commit_before
        status_digest_before = working_tree_status_digest()
        tracked_clean_before = not tracked_working_tree_dirty()
        model_mode = model_tool_mode(CATALOG_PATH, args.model)
        evaluation_lane = lane_for_tool_mode(model_mode)
        require(args.build, "live_source_catalog_requires_build")
        build_started_ns = time.time_ns()
        if model_mode == "code_mode_only":
            code_mode_build = build_code_mode_release()
            built_cli = code_mode_build.cli
            code_mode_host_digest_before = code_mode_build.code_mode_host_sha256
        else:
            built_cli = build_release()
        binary_built_in_run = True
        cli = resolve_cli(args.codex_cli if args.codex_cli is not None else built_cli)
        require(
            is_repo_target_cli(cli),
            "cli_provenance_unverified_use_repo_target_or_build",
        )
        require(cli == built_cli, "binary_provenance_unverified_use_built_cli")
        version = cli_version(cli, min(args.timeout, 30.0))
        binary_digest_before = sha256_file(cli)
        binary_digest = binary_digest_before
        binary_physically_rebuilt_in_run = (
            built_cli.stat().st_mtime_ns >= build_started_ns
            and (
                code_mode_build is None
                or code_mode_build.code_mode_host.stat().st_mtime_ns >= build_started_ns
            )
        )
        status_digest_after_build = working_tree_status_digest()
        require(
            git_commit() == commit_before
            and stable_worktree_snapshot(
                status_digest_before,
                tracked_clean_before,
                status_digest_after_build,
                not tracked_working_tree_dirty(),
            )
            and sha256_file(CATALOG_PATH) == catalog_digest
            and model_catalog_entry_digest(CATALOG_PATH, args.model)
            == catalog_entry_digest,
            "source_provenance_changed_during_build",
        )
        fixture_root = Path(tempfile.mkdtemp(prefix="codex-savings-e2e-"))
        fixture_root.chmod(0o700)
        fixture = build_fixture(fixture_root)
        initialize_git_fixture(fixture)
        sqlite_home = (
            args.sqlite_home.expanduser().resolve()
            if args.sqlite_home is not None
            else None
        )
        database = resolve_database(
            args.codex_db,
            home,
            home_overridden,
            cwd=fixture.root,
            sqlite_home_override=sqlite_home,
            ignore_user_config=True,
            require_existing=False,
        )
        sqlite_home = database.parent
        events, thread_id, terminal, _, _ = run_cli(
            cli,
            fixture,
            model=args.model,
            reasoning_effort=args.reasoning_effort,
            model_tool_mode_value=model_mode,
            codex_home=home,
            sqlite_home=sqlite_home,
            timeout=args.timeout,
        )
        binary_digest = sha256_file(cli)
        code_mode_host_digest_after = (
            sha256_file(code_mode_build.code_mode_host)
            if code_mode_build is not None
            else "unknown"
        )
        tracked_clean_after = not tracked_working_tree_dirty()
        status_digest_after = working_tree_status_digest()
        require(binary_digest == binary_digest_before, "binary_changed_during_run")
        require(
            code_mode_host_digest_after == code_mode_host_digest_before,
            "code_mode_host_changed_during_run",
        )
        require(
            git_commit() == commit_before
            and stable_worktree_snapshot(
                status_digest_before,
                tracked_clean_before,
                status_digest_after,
                tracked_clean_after,
            )
            and sha256_file(CATALOG_PATH) == catalog_digest
            and model_catalog_entry_digest(CATALOG_PATH, args.model)
            == catalog_entry_digest
            and compute_harness_digest(Path(__file__).parent) == harness_digest,
            "source_provenance_changed_during_run",
        )
        row, rollout_path, rollout_text, records, rollout_usage = measure_thread(
            database,
            thread_id,
            timeout=min(args.timeout, 20.0),
            terminal_usage=terminal,
            model=args.model,
            reasoning_effort=args.reasoning_effort,
            cli_version=version,
        )
        analyzer = analyze_code_mode if model_mode == "code_mode_only" else analyze
        report = analyzer(
            events,
            rollout_text,
            records,
            fixture,
            home,
            thread_id,
            row,
            terminal,
            cli_version=version,
            model=args.model,
            reasoning_effort=args.reasoning_effort,
            binary_sha256=binary_digest,
            binary_built_in_run=binary_built_in_run,
            binary_physically_rebuilt_in_run=binary_physically_rebuilt_in_run,
            repository_commit=None,
            working_tree_dirty=working_tree_dirty(),
            model_tool_mode=model_mode,
            catalog_sha256=catalog_digest,
            catalog_entry_sha256=catalog_entry_digest,
            harness_sha256=harness_digest,
            binary_sha256_before=binary_digest_before,
            tracked_worktree_clean_before=tracked_clean_before,
            tracked_worktree_clean_after=tracked_clean_after,
            status_digest_before=status_digest_before,
            status_digest_after=status_digest_after,
        )
        if code_mode_build is not None:
            report.update(
                {
                    "code_mode_host_sha256": code_mode_host_digest_after,
                    "code_mode_host_sha256_before": code_mode_host_digest_before,
                    "code_mode_host_built_in_run": binary_built_in_run,
                    "code_mode_host_physically_rebuilt_in_run": code_mode_build.code_mode_host.stat().st_mtime_ns
                    >= build_started_ns,
                    "rusty_v8_version": code_mode_build.rusty_v8_version,
                    "rust_target": code_mode_build.rust_target,
                    "rusty_v8_archive_sha256": code_mode_build.rusty_v8_archive_sha256,
                    "rusty_v8_bindings_sha256": code_mode_build.rusty_v8_bindings_sha256,
                }
            )
        report = mark_unattributed_build_provenance(report, observed_source_head)
        report["duration_seconds"] = round(time.monotonic() - started, 3)
        report = redacted_report(report)
        report, cleanup_ok = _cleanup_report(
            report,
            args=args,
            cli=cli,
            home=home,
            fixture=fixture,
            database=database,
            thread_id=thread_id,
            sqlite_home=sqlite_home,
            rollout_path=rollout_path,
        )
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        print(
            f"{'PASS' if cleanup_ok else 'FAIL'} live synthetic integrity and byte-economics evaluation: db_total={report['db_indexed_total_tokens']} rollout_total={report['rollout_token_usage']['total_tokens']} exec_input={report['exec_terminal_usage']['input_tokens']} exec_output={report['exec_terminal_usage']['output_tokens']}"
        )
        if args.keep:
            print(
                f"KEEP: fixture={fixture.root} rollout={rollout_path} thread_id={thread_id}"
            )
            print(
                "KEEP WARNING: these locations contain model/tool traffic and generated fixtures."
            )
        return 0 if cleanup_ok else 1
    except BaseException as error:
        recovered_thread_id = getattr(error, "thread_id", None)
        process_cleanup_confirmed = (
            getattr(error, "process_cleanup_confirmed", True) is True
        )
        process_started = getattr(error, "process_started", True) is True
        if thread_id is None and isinstance(recovered_thread_id, str):
            thread_id = recovered_thread_id
        rule = (
            str(error)
            if isinstance(error, HarnessError)
            else "harness_unexpected_failure"
        )
        if not process_cleanup_confirmed:
            rule = "process_cleanup_unverified"
        report = failure_report(
            args.model,
            args.reasoning_effort,
            rule,
            started,
            model_tool_mode=model_mode,
            cli_version=version,
            binary_sha256=binary_digest,
            binary_built_in_run=binary_built_in_run,
            binary_physically_rebuilt_in_run=binary_physically_rebuilt_in_run,
            observed_source_head=observed_source_head,
            thread_id=thread_id,
            row=row,
            rollout_usage=rollout_usage,
            terminal=terminal,
        )
        report["evaluation_lane"] = evaluation_lane
        if fixture_root is not None:
            if not process_cleanup_confirmed:
                report["cleanup"] = {
                    "fixture": "retained_cleanup_receipt",
                    "thread": "retained_process_cleanup_unverified",
                    "thread_hash": short_hash(thread_id)
                    if thread_id
                    else "unavailable",
                }
                report["cleanup_failure_rule"] = "process_cleanup_unverified"
                try:
                    receipt = _write_cleanup_receipt(
                        fixture_root,
                        thread_id,
                        home,
                        database,
                        cli,
                        sqlite_home,
                        failure_detail="process_cleanup_unverified",
                    )
                    print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
                except BaseException:
                    report["cleanup"]["fixture"] = "retained_process_cleanup_unverified"
                try:
                    _write_content_free_report(fixture_root, report)
                except BaseException:
                    pass
            elif thread_id is not None and cli is not None and not args.keep:
                try:
                    require(
                        database is not None, "thread_cleanup_state_database_missing"
                    )
                    targets = delete_thread(
                        cli,
                        thread_id,
                        home,
                        sqlite_home,
                        args.timeout,
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
                    except BaseException:
                        receipt = None
                    report["cleanup"] = {
                        "fixture": "retained_cleanup_receipt",
                        "thread": "cleanup_failed",
                        "thread_hash": short_hash(thread_id),
                    }
                    report["cleanup_failure_rule"] = "thread_cleanup_failed"
                    report.setdefault("failure_rule", "cleanup_failed")
                    try:
                        _write_content_free_report(fixture_root, report)
                    except BaseException:
                        pass
                    if receipt is not None:
                        print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
                else:
                    report["cleanup"] = {
                        "fixture": "pending_exact_fixture_removal",
                        "thread": "deleted_exact_thread",
                        "thread_hash": short_hash(thread_id),
                    }
                    try:
                        _write_content_free_report(fixture_root, report)
                        _remove_fixture(fixture_root)
                        report["cleanup"]["fixture"] = "removed_exact_fixture"
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
                        except BaseException:
                            receipt = None
                        report["cleanup"]["fixture"] = "retained_cleanup_receipt"
                        report["cleanup_failure_rule"] = "fixture_cleanup_failed"
                        report.setdefault("failure_rule", "cleanup_failed")
                        try:
                            _write_content_free_report(fixture_root, report)
                        except BaseException:
                            pass
                        if receipt is not None:
                            print(
                                f"Cleanup receipt retained at {receipt}",
                                file=sys.stderr,
                            )
            elif not process_started and not args.keep:
                report["cleanup"] = {
                    "fixture": "pending_exact_fixture_removal",
                    "thread": "not_started",
                    "thread_hash": "unavailable",
                }
                try:
                    _write_content_free_report(fixture_root, report)
                    _remove_fixture(fixture_root)
                    report["cleanup"]["fixture"] = "removed_exact_fixture"
                except BaseException as cleanup_error:
                    try:
                        receipt = _write_cleanup_receipt(
                            fixture_root,
                            None,
                            home,
                            database,
                            cli,
                            sqlite_home,
                            failure_detail=str(cleanup_error),
                        )
                    except BaseException:
                        receipt = None
                    report["cleanup"]["fixture"] = "retained_cleanup_receipt"
                    report["cleanup_failure_rule"] = "fixture_cleanup_failed"
                    try:
                        _write_content_free_report(fixture_root, report)
                    except BaseException:
                        pass
                    if receipt is not None:
                        print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
            elif args.keep:
                report["cleanup"] = {
                    "fixture": "retained_by_keep",
                    "thread": "retained_by_keep" if thread_id else "not_started",
                    "thread_hash": short_hash(thread_id)
                    if thread_id
                    else "unavailable",
                }
                _write_content_free_report(fixture_root, report)
                print(
                    f"KEEP: fixture={fixture_root}"
                    + (f" rollout={rollout_path}" if rollout_path else "")
                    + (f" thread_id={thread_id}" if thread_id else ""),
                    file=sys.stderr,
                )
                print(
                    "KEEP WARNING: these locations contain model/tool traffic and generated fixtures.",
                    file=sys.stderr,
                )
            else:
                report["cleanup"] = {
                    "fixture": "retained_unresolved_thread_identity",
                    "thread": "unresolved_thread_identity",
                    "thread_hash": short_hash(thread_id)
                    if thread_id
                    else "unavailable",
                }
                try:
                    receipt = _write_cleanup_receipt(
                        fixture_root,
                        thread_id,
                        home,
                        database,
                        cli,
                        sqlite_home,
                        failure_detail="thread identity could not be safely recovered",
                    )
                    print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
                except BaseException:
                    report["cleanup_failure_rule"] = "cleanup_receipt_unavailable"
                try:
                    _write_content_free_report(fixture_root, report)
                except BaseException:
                    report["cleanup_failure_rule"] = "diagnostic_write_failed"
        print(json.dumps(report, sort_keys=True, separators=(",", ":")))
        print(
            f"FAIL live synthetic integrity and byte-economics evaluation: {rule}",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
