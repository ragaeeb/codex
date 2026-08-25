"""Exception-safe cleanup and private diagnostic lifecycle for the live eval."""

import argparse
import json
import os
import secrets
import shlex
import shutil
import sys
from pathlib import Path
from typing import Any

from tool_output_savings_cleanup import delete_thread
from tool_output_savings_cleanup import verify_thread_deleted
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_evidence import short_hash
from tool_output_savings_fixture import Fixture


MAX_PRIVATE_REPORT_BYTES = 64 * 1024


def _write_private_json(root: Path, name: str, value: dict[str, Any]) -> Path:
    """Replace a diagnostic without following an agent-created symlink."""
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    require(len(encoded) <= MAX_PRIVATE_REPORT_BYTES, "diagnostic_over_cap")
    require(
        sys.platform == "darwin"
        and hasattr(os, "O_DIRECTORY")
        and hasattr(os, "O_NOFOLLOW")
        and os.open in getattr(os, "supports_dir_fd", set()),
        "diagnostic_platform_unsupported",
    )
    require(
        os.rename in getattr(os, "supports_dir_fd", set())
        and os.unlink in getattr(os, "supports_dir_fd", set()),
        "diagnostic_platform_unsupported",
    )
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    directory_fd = os.open(root, flags)
    temporary_name = f".{name}.tmp-{secrets.token_hex(8)}"
    temporary_fd = -1
    try:
        temporary_fd = os.open(
            temporary_name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o600,
            dir_fd=directory_fd,
        )
        view = memoryview(encoded)
        while view:
            view = view[os.write(temporary_fd, view) :]
        os.fsync(temporary_fd)
        os.close(temporary_fd)
        temporary_fd = -1
        os.rename(
            temporary_name,
            name,
            src_dir_fd=directory_fd,
            dst_dir_fd=directory_fd,
        )
    finally:
        if temporary_fd >= 0:
            os.close(temporary_fd)
        try:
            os.unlink(temporary_name, dir_fd=directory_fd)
        except FileNotFoundError:
            pass
        os.close(directory_fd)
    return root / name


def _write_content_free_report(root: Path, report: dict[str, Any]) -> Path:
    return _write_private_json(root, "report.json", report)


def _write_cleanup_receipt(
    root: Path,
    thread_id: str | None,
    codex_home: Path,
    database: Path | None,
    cli: Path | None = None,
    sqlite_home: Path | None = None,
    failure_detail: str | None = None,
) -> Path:
    state_home = sqlite_home or (database.parent if database is not None else None)
    if sqlite_home is not None and database is not None:
        require(
            database.parent == sqlite_home.expanduser().resolve(),
            "state_db_configuration_conflict",
        )
    argv = (
        [
            str(cli),
            "-c",
            f"sqlite_home={json.dumps(str(state_home))}",
            "delete",
            thread_id,
            "--force",
        ]
        if cli is not None and state_home is not None and thread_id is not None
        else None
    )
    command = (
        f"CODEX_HOME={shlex.quote(str(codex_home))} {shlex.join(argv)}"
        if argv is not None
        else None
    )
    try:
        root_metadata = root.stat(follow_symlinks=False)
    except OSError:
        root_metadata = None
    receipt_root = (
        root
        if root_metadata is not None
        and not os.path.islink(root)
        and root_metadata.st_mode & 0o170000 == 0o040000
        else root.parent
    )
    try:
        receipt_metadata = receipt_root.stat(follow_symlinks=False)
    except OSError as error:
        raise HarnessError("cleanup_receipt_unavailable") from error
    require(
        receipt_metadata.st_mode & 0o170000 == 0o040000
        and not os.path.islink(receipt_root),
        "cleanup_receipt_unavailable",
    )
    receipt_name = (
        "cleanup-receipt.json"
        if receipt_root == root and thread_id is not None
        else f"cleanup-receipt-{short_hash(thread_id)}.json"
        if thread_id is not None
        else "cleanup-receipt-unresolved.json"
    )
    return _write_private_json(
        receipt_root,
        receipt_name,
        {
            "thread_id": thread_id,
            "codex_home": str(codex_home),
            "state_home": str(state_home) if state_home is not None else None,
            "state_database": str(database) if database is not None else None,
            "fixture_root": str(root),
            "cleanup_command": command,
            "cleanup_argv": argv,
            "cleanup_environment": {"CODEX_HOME": str(codex_home)},
            "failure_detail": failure_detail,
        },
    )


def _remove_fixture(root: Path) -> None:
    shutil.rmtree(root)
    require(not root.exists(), "fixture_cleanup_not_verified")


def _cleanup_report(
    report: dict[str, Any],
    *,
    args: argparse.Namespace,
    cli: Path,
    home: Path,
    fixture: Fixture,
    database: Path,
    thread_id: str,
    sqlite_home: Path | None,
    rollout_path: Path | None = None,
) -> tuple[dict[str, Any], bool]:
    report["cleanup"] = {
        "fixture": "pending_exact_fixture_removal",
        "thread": "pending_exact_thread_delete",
        "thread_hash": short_hash(thread_id),
    }
    if args.keep:
        report["cleanup"] = {
            "fixture": "retained_by_keep",
            "thread": "retained_by_keep",
            "thread_hash": short_hash(thread_id),
        }
        _write_content_free_report(fixture.root, report)
        return report, True
    try:
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
    except BaseException as error:
        try:
            receipt = _write_cleanup_receipt(
                fixture.root,
                thread_id,
                home,
                database,
                cli,
                sqlite_home,
                failure_detail=str(error),
            )
        except BaseException:
            receipt = None
        report["cleanup"] = {
            "fixture": "retained_cleanup_receipt",
            "thread": "cleanup_failed",
            "thread_hash": short_hash(thread_id),
        }
        report["status"] = "fail"
        report["cleanup_failure_rule"] = "thread_cleanup_failed"
        report.setdefault("failure_rule", "cleanup_failed")
        try:
            _write_content_free_report(fixture.root, report)
        except BaseException:
            pass
        if receipt is not None:
            print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
        return report, False
    report["cleanup"] = {
        "fixture": "pending_exact_fixture_removal",
        "thread": "deleted_exact_thread",
        "thread_hash": short_hash(thread_id),
    }
    try:
        _write_content_free_report(fixture.root, report)
        _remove_fixture(fixture.root)
    except BaseException as error:
        try:
            receipt = _write_cleanup_receipt(
                fixture.root,
                thread_id,
                home,
                database,
                cli,
                sqlite_home,
                failure_detail=str(error),
            )
        except BaseException:
            receipt = None
        report["cleanup"]["fixture"] = "retained_cleanup_receipt"
        report["status"] = "fail"
        report["cleanup_failure_rule"] = "fixture_cleanup_failed"
        report.setdefault("failure_rule", "cleanup_failed")
        try:
            _write_content_free_report(fixture.root, report)
        except BaseException:
            pass
        if receipt is not None:
            print(f"Cleanup receipt retained at {receipt}", file=sys.stderr)
        return report, False
    report["cleanup"]["fixture"] = "removed_exact_fixture"
    return report, True
