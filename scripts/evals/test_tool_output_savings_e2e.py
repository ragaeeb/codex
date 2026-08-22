import argparse
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import tool_output_savings_e2e as savings_e2e
from tool_output_savings_cleanup import ThreadDeletionTargets
from tool_output_savings_lifecycle import _cleanup_report
from tool_output_savings_lifecycle import _write_cleanup_receipt
from tool_output_savings_lifecycle import _write_content_free_report
from tool_output_savings_fixture import build_fixture
from tool_output_savings_evidence import HarnessError
from tool_output_savings_process import CliRunError
from tool_output_savings_report import failure_report
from tool_output_savings_options import build_parser
from tool_output_savings_runtime_provenance import stable_worktree_snapshot


THREAD_ID = "00000000-0000-0000-0000-000000000001"


def cleanup_args(*, keep: bool) -> argparse.Namespace:
    return argparse.Namespace(
        keep=keep,
        codex_home=Path("effective-codex-home"),
        timeout=1.0,
    )


class SavingsE2ETests(unittest.TestCase):
    def test_default_model_is_luna_code_mode_lane(self) -> None:
        args = build_parser().parse_args([])
        self.assertEqual(args.model, "gpt-5.6-luna")

    def test_stable_dirty_worktree_is_valid_build_provenance(self) -> None:
        self.assertTrue(
            stable_worktree_snapshot(
                "same-status",
                False,
                "same-status",
                False,
            )
        )
        self.assertFalse(
            stable_worktree_snapshot(
                "before",
                False,
                "after",
                False,
            )
        )

    def test_fresh_build_does_not_claim_reproducible_head(self) -> None:
        report = failure_report(
            "gpt-5.5",
            "medium",
            "forced_failure",
            0.0,
            binary_built_in_run=True,
            observed_source_head="b" * 40,
        )
        self.assertTrue(report["binary_built_in_run"])
        self.assertEqual(report["observed_source_head"], "b" * 40)
        self.assertEqual(report["repository_commit"], "not_attributed")
        self.assertEqual(
            report["repository_commit_attribution"],
            "not_claimed_external_build_inputs_unverified",
        )

    def test_unsupported_platform_fails_before_build_or_fixture_creation(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            patch.object(savings_e2e.sys, "argv", ["e2e", "--live", "--build"]),
            patch.object(savings_e2e.sys, "platform", "linux"),
            patch.object(savings_e2e, "build_release") as build,
            patch.object(savings_e2e.tempfile, "mkdtemp") as mkdtemp,
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(savings_e2e.main(), 1)
        build.assert_not_called()
        mkdtemp.assert_not_called()
        self.assertEqual(
            json.loads(stdout.getvalue())["failure_rule"],
            "live_eval_scope_macos_only",
        )

    @unittest.skipUnless(sys.platform == "darwin", "live scope is macOS-only")
    def test_alternate_cli_is_rejected_before_execution(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            built = root / "built-codex"
            alternate = root / "alternate-codex"
            built.write_bytes(b"built")
            alternate.write_bytes(b"alternate")
            argv = [
                "e2e",
                "--live",
                "--build",
                "--model",
                "gpt-5.5",
                "--codex-cli",
                str(alternate),
            ]
            with (
                patch.object(savings_e2e.sys, "argv", argv),
                patch.object(savings_e2e, "build_release", return_value=built),
                patch.object(savings_e2e, "resolve_cli", return_value=alternate),
                patch.object(savings_e2e, "is_repo_target_cli", return_value=True),
                patch.object(savings_e2e, "cli_version") as version,
                patch.object(savings_e2e, "git_commit", return_value="a" * 40),
                patch.object(
                    savings_e2e, "working_tree_status_digest", return_value="status"
                ),
                patch.object(
                    savings_e2e, "tracked_working_tree_dirty", return_value=False
                ),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                self.assertEqual(savings_e2e.main(), 1)
            version.assert_not_called()
            self.assertEqual(
                json.loads(stdout.getvalue())["failure_rule"],
                "binary_provenance_unverified_use_built_cli",
            )

    @unittest.skipUnless(sys.platform == "darwin", "live scope is macOS-only")
    def test_failure_report_uses_measured_binary_build_provenance(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            cli = root / "codex"
            cli.write_bytes(b"old release binary")
            cli.chmod(0o700)
            os.utime(cli, (1, 1))
            argv = [
                "e2e",
                "--live",
                "--build",
                "--model",
                "gpt-5.5",
                "--codex-home",
                str(root / "codex-home"),
            ]
            with (
                patch.object(savings_e2e.sys, "argv", argv),
                patch.object(savings_e2e, "build_release", return_value=cli),
                patch.object(savings_e2e, "resolve_cli", return_value=cli),
                patch.object(savings_e2e, "is_repo_target_cli", return_value=True),
                patch.object(savings_e2e, "cli_version", return_value="0.0.0"),
                patch.object(savings_e2e, "git_commit", return_value="a" * 40),
                patch.object(
                    savings_e2e, "working_tree_status_digest", return_value="status"
                ),
                patch.object(
                    savings_e2e, "tracked_working_tree_dirty", return_value=False
                ),
                patch.object(
                    savings_e2e.tempfile, "mkdtemp", return_value=str(fixture_root)
                ),
                patch.object(savings_e2e, "initialize_git_fixture"),
                patch.object(
                    savings_e2e,
                    "run_cli",
                    side_effect=CliRunError(
                        "cli_process_finalization_failed",
                        THREAD_ID,
                        process_cleanup_confirmed=False,
                    ),
                ),
                patch.object(savings_e2e, "delete_thread") as delete,
                patch.object(savings_e2e, "_remove_fixture") as remove_fixture,
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                self.assertEqual(savings_e2e.main(), 1)
            report = json.loads(stdout.getvalue())
            self.assertEqual(report["failure_rule"], "process_cleanup_unverified")
            self.assertEqual(
                report["cleanup_failure_rule"], "process_cleanup_unverified"
            )
            self.assertEqual(
                report["cleanup"]["thread"], "retained_process_cleanup_unverified"
            )
            receipt = fixture_root / "cleanup-receipt.json"
            self.assertEqual(receipt.stat().st_mode & 0o777, 0o600)
            delete.assert_not_called()
            remove_fixture.assert_not_called()
        report = json.loads(stdout.getvalue())
        self.assertTrue(report["binary_built_in_run"])
        self.assertFalse(report["binary_physically_rebuilt_in_run"])
        self.assertEqual(report["observed_source_head"], "a" * 40)
        self.assertEqual(report["repository_commit"], "not_attributed")
        self.assertEqual(
            report["repository_commit_attribution"],
            "not_claimed_external_build_inputs_unverified",
        )
        self.assertEqual(
            report["binary_reproducibility"], "external_build_inputs_unverified"
        )

    @unittest.skipUnless(sys.platform == "darwin", "live scope is macOS-only")
    def test_launch_failure_removes_fixture_without_thread_cleanup(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            cli = root / "codex"
            cli.write_bytes(b"release binary")
            cli.chmod(0o700)
            argv = [
                "e2e",
                "--live",
                "--build",
                "--model",
                "gpt-5.5",
                "--codex-home",
                str(root / "codex-home"),
            ]
            with (
                patch.object(savings_e2e.sys, "argv", argv),
                patch.object(savings_e2e, "build_release", return_value=cli),
                patch.object(savings_e2e, "resolve_cli", return_value=cli),
                patch.object(savings_e2e, "is_repo_target_cli", return_value=True),
                patch.object(savings_e2e, "cli_version", return_value="0.0.0"),
                patch.object(savings_e2e, "git_commit", return_value="a" * 40),
                patch.object(
                    savings_e2e, "working_tree_status_digest", return_value="status"
                ),
                patch.object(
                    savings_e2e, "tracked_working_tree_dirty", return_value=False
                ),
                patch.object(
                    savings_e2e.tempfile, "mkdtemp", return_value=str(fixture_root)
                ),
                patch.object(savings_e2e, "initialize_git_fixture"),
                patch.object(
                    savings_e2e,
                    "run_cli",
                    side_effect=CliRunError(
                        "cli_failed",
                        None,
                        process_cleanup_confirmed=True,
                        process_started=False,
                    ),
                ),
                patch.object(savings_e2e, "delete_thread") as delete,
                patch.object(savings_e2e, "_write_cleanup_receipt") as receipt,
                patch.object(savings_e2e, "_remove_fixture") as remove_fixture,
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                self.assertEqual(savings_e2e.main(), 1)
            report = json.loads(stdout.getvalue())
            self.assertEqual(report["cleanup"]["thread"], "not_started")
            self.assertEqual(report["cleanup"]["fixture"], "removed_exact_fixture")
            delete.assert_not_called()
            receipt.assert_not_called()
            remove_fixture.assert_called_once_with(fixture_root)

    def test_keep_rewrites_the_final_report(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            report, cleanup_ok = _cleanup_report(
                {"status": "pass"},
                args=cleanup_args(keep=True),
                cli=Path("codex"),
                home=Path("effective-codex-home"),
                fixture=fixture,
                database=root / "state_5.sqlite",
                thread_id=THREAD_ID,
                sqlite_home=None,
            )
            self.assertTrue(cleanup_ok)
            self.assertEqual(report["cleanup"]["fixture"], "retained_by_keep")
            persisted = json.loads((fixture.root / "report.json").read_text())
            self.assertEqual(persisted["cleanup"]["thread"], "retained_by_keep")

    def test_cleanup_failure_retains_permission_restricted_exact_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            database = root / "state_5.sqlite"
            with patch(
                "tool_output_savings_lifecycle.delete_thread",
                side_effect=HarnessError("forced_cleanup_failure"),
            ):
                report, cleanup_ok = _cleanup_report(
                    {"status": "pass"},
                    args=cleanup_args(keep=False),
                    cli=Path("codex"),
                    home=Path("effective-codex-home"),
                    fixture=fixture,
                    database=database,
                    thread_id=THREAD_ID,
                    sqlite_home=None,
                )
            self.assertFalse(cleanup_ok)
            self.assertEqual(report["cleanup"]["fixture"], "retained_cleanup_receipt")
            receipt = fixture.root / "cleanup-receipt.json"
            self.assertEqual(receipt.stat().st_mode & 0o777, 0o600)
            receipt_value = json.loads(receipt.read_text())
            self.assertEqual(receipt_value["thread_id"], THREAD_ID)
            self.assertIn(THREAD_ID, receipt_value["cleanup_command"])
            self.assertEqual(
                receipt_value["cleanup_environment"]["CODEX_HOME"],
                "effective-codex-home",
            )
            self.assertTrue(fixture.root.exists())

    @unittest.skipUnless(
        sys.platform == "darwin", "diagnostic descriptor checks are macOS-only"
    )
    def test_diagnostic_replacement_does_not_follow_report_or_receipt_symlinks(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            outside = root / "outside.json"
            outside.write_text("keep", encoding="utf-8")
            (fixture_root / "report.json").symlink_to(outside)
            _write_content_free_report(fixture_root, {"status": "pass"})
            self.assertEqual(outside.read_text(encoding="utf-8"), "keep")
            self.assertFalse((fixture_root / "report.json").is_symlink())

            (fixture_root / "cleanup-receipt.json").symlink_to(outside)
            _write_cleanup_receipt(
                fixture_root,
                THREAD_ID,
                Path("effective-codex-home"),
                root / "sqlite" / "state_5.sqlite",
                Path("codex"),
            )
            self.assertEqual(outside.read_text(encoding="utf-8"), "keep")
            self.assertFalse((fixture_root / "cleanup-receipt.json").is_symlink())

    def test_cleanup_failure_preserves_evaluation_failure_rule(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            with patch(
                "tool_output_savings_lifecycle.delete_thread",
                side_effect=HarnessError("forced_cleanup_failure"),
            ):
                report, cleanup_ok = _cleanup_report(
                    {"status": "fail", "failure_rule": "evaluation_failed"},
                    args=cleanup_args(keep=False),
                    cli=Path("codex"),
                    home=Path("effective-codex-home"),
                    fixture=fixture,
                    database=root / "state_5.sqlite",
                    thread_id=THREAD_ID,
                    sqlite_home=None,
                )
            self.assertFalse(cleanup_ok)
            self.assertEqual(report["failure_rule"], "evaluation_failed")
            self.assertEqual(report["cleanup_failure_rule"], "thread_cleanup_failed")

    def test_success_cleanup_verifies_all_captured_deletion_targets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            targets = ThreadDeletionTargets(
                database=root / "state_5.sqlite",
                thread_id=THREAD_ID,
                rollout_path=root / "rollout.jsonl",
                artifact_directory=root / "tool_outputs" / THREAD_ID,
            )
            with (
                patch(
                    "tool_output_savings_lifecycle.delete_thread",
                    return_value=targets,
                ) as delete,
                patch("tool_output_savings_lifecycle.verify_thread_deleted") as verify,
                patch("tool_output_savings_lifecycle._remove_fixture"),
            ):
                report, cleanup_ok = _cleanup_report(
                    {"status": "pass"},
                    args=cleanup_args(keep=False),
                    cli=Path("codex"),
                    home=Path("effective-codex-home"),
                    fixture=fixture,
                    database=targets.database,
                    thread_id=THREAD_ID,
                    sqlite_home=None,
                    rollout_path=targets.rollout_path,
                )
            self.assertTrue(cleanup_ok)
            self.assertEqual(report["cleanup"]["thread"], "deleted_exact_thread")
            verify.assert_called_once_with(targets)
            self.assertEqual(
                delete.call_args.kwargs["rollout_path"], targets.rollout_path
            )


if __name__ == "__main__":
    unittest.main()
