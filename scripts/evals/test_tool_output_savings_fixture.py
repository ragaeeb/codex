import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock
from unittest.mock import patch

from tool_output_savings_evidence import HarnessError
from tool_output_savings_fixture import MAX_CAPTURE_BYTES
from tool_output_savings_fixture import build_release
from tool_output_savings_fixture import build_cli_command
from tool_output_savings_fixture import build_fixture
from tool_output_savings_fixture import initialize_git_fixture
from tool_output_savings_fixture import model_tool_mode
from tool_output_savings_fixture import prompt
from tool_output_savings_fixture import REPO_ROOT
from tool_output_savings_fixture import _git_output
from tool_output_savings_fixture import cli_version
from tool_output_savings_fixture import validate_reasoning_effort
from tool_output_savings_process import CliRunError
from tool_output_savings_process import run_cli
from tool_output_savings_cleanup import ThreadDeletionTargets
from tool_output_savings_cleanup import delete_thread
from tool_output_savings_cleanup import verify_thread_deleted
from tool_output_savings_subprocess import run_bounded_command
from tool_output_savings_subprocess import BoundedCommandOutput
from tool_output_savings_subprocess import run_bounded_capture


THREAD_ID = "00000000-0000-0000-0000-000000000001"


class SavingsFixtureTests(unittest.TestCase):
    def test_release_build_is_locked_bounded_and_does_not_capture_output(self) -> None:
        built_cli = Path("/repo/target/release/codex")
        with (
            patch("tool_output_savings_fixture.run_bounded_command") as run,
            patch(
                "tool_output_savings_fixture.resolve_built_cli",
                return_value=built_cli,
            ),
        ):
            self.assertEqual(build_release(), built_cli)
        self.assertEqual(
            run.call_args.args[0],
            [
                "cargo",
                "build",
                "--locked",
                "--release",
                "-p",
                "codex-cli",
                "--bin",
                "codex",
            ],
        )
        self.assertEqual(run.call_args.kwargs["rule"], "release_build_failed")
        self.assertEqual(run.call_args.kwargs["timeout"], 900)

    def test_fixture_git_commands_ignore_external_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory))
            with patch("tool_output_savings_fixture.run_bounded_command") as run:
                initialize_git_fixture(fixture)
        self.assertEqual(run.call_count, 3)
        commands = [call.args[0] for call in run.call_args_list]
        for call in run.call_args_list:
            environment = call.kwargs["environment"]
            self.assertEqual(environment["GIT_CONFIG_NOSYSTEM"], "1")
            self.assertEqual(environment["GIT_CONFIG_GLOBAL"], os.devnull)
            self.assertEqual(environment["GIT_TERMINAL_PROMPT"], "0")
            self.assertNotIn("GIT_CONFIG_COUNT", environment)
        for command in commands:
            self.assertIn("core.hooksPath=/dev/null", command)
        self.assertIn("init.templateDir=", commands[0])
        self.assertIn("commit.gpgsign=false", commands[2])
        self.assertIn("--no-verify", commands[2])
        self.assertIn("--no-gpg-sign", commands[2])

    def test_bounded_command_discards_output_and_confirms_termination(self) -> None:
        process = Mock()
        process.wait.return_value = 0
        with (
            patch(
                "tool_output_savings_subprocess.subprocess.Popen",
                return_value=process,
            ) as popen,
            patch("tool_output_savings_subprocess.terminate_process") as terminate,
        ):
            run_bounded_command(
                ["command"],
                cwd=Path("/tmp"),
                timeout=1.0,
                rule="command_failed",
            )
        self.assertIs(popen.call_args.kwargs["stdout"], subprocess.DEVNULL)
        self.assertIs(popen.call_args.kwargs["stderr"], subprocess.DEVNULL)
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        terminate.assert_called_once_with(process)

    def test_bounded_capture_limits_output_and_returns_small_results(self) -> None:
        result = run_bounded_capture(
            [sys.executable, "-c", "print('captured')"],
            cwd=Path.cwd(),
            timeout=2.0,
            rule="bounded_capture_failed",
        )
        self.assertEqual(result.stdout, b"captured\n")
        self.assertEqual(result.stderr, b"")
        with self.assertRaisesRegex(HarnessError, "bounded_capture_failed"):
            run_bounded_capture(
                [sys.executable, "-c", "print('x' * 70000)"],
                cwd=Path.cwd(),
                timeout=2.0,
                rule="bounded_capture_failed",
            )

    def test_git_probe_is_config_isolated_and_bounded(self) -> None:
        with patch(
            "tool_output_savings_fixture.run_bounded_capture",
            return_value=BoundedCommandOutput(0, b"value\n", b""),
        ) as run:
            self.assertEqual(_git_output(["status", "--porcelain"]), "value")
        command = run.call_args.args[0]
        self.assertIn("core.fsmonitor=false", command)
        self.assertIn("core.hooksPath=/dev/null", command)
        environment = run.call_args.kwargs["environment"]
        self.assertEqual(environment["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(environment["GIT_CONFIG_GLOBAL"], os.devnull)
        self.assertEqual(environment["GIT_OPTIONAL_LOCKS"], "0")
        self.assertEqual(environment["GIT_TERMINAL_PROMPT"], "0")
        self.assertNotIn("OPENAI_API_KEY", environment)

    def test_version_probe_uses_a_minimal_environment(self) -> None:
        with patch(
            "tool_output_savings_fixture.run_bounded_capture",
            return_value=BoundedCommandOutput(0, b"codex-cli 1.2.3\n", b""),
        ) as run:
            self.assertEqual(cli_version(Path("/repo/codex"), 1.0), "1.2.3")
        environment = run.call_args.kwargs["environment"]
        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertNotIn("CODEX_HOME", environment)
        self.assertNotIn("DYLD_INSERT_LIBRARIES", environment)

    def test_direct_command_is_isolated_and_pins_spill_limit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory), python_executable=sys.executable)
            command = build_cli_command(
                Path("/bin/codex"),
                fixture,
                model="direct-model",
                reasoning_effort="medium",
                model_tool_mode_value="direct",
            )
            self.assertIn("--ignore-user-config", command)
            self.assertIn("--ignore-rules", command)
            self.assertIn("--disable", command)
            self.assertIn('shell_environment_policy.inherit="none"', command)
            self.assertIn('model_reasoning_effort="medium"', command)
            self.assertIn("tool_output_token_limit=10000", command)
            self.assertIn("max_output_tokens=1000", command[-1])
            self.assertIn(
                json.dumps([sys.executable, "emit_large.py"])[1:-1]
                .split(",")[0]
                .strip('"'),
                command[-1],
            )

    def test_reasoning_effort_is_catalog_validated_and_toml_encoded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            catalog = Path(directory) / "models.json"
            catalog.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "direct-model",
                                "supported_reasoning_levels": [
                                    {"effort": "low"},
                                    {"effort": "medium"},
                                ],
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            validate_reasoning_effort(catalog, "direct-model", "medium")
            with self.assertRaisesRegex(HarnessError, "reasoning_effort_unsupported"):
                validate_reasoning_effort(
                    catalog, "direct-model", 'medium"\ninjected=true'
                )

            fixture = build_fixture(Path(directory))
            command = build_cli_command(
                Path("/bin/codex"),
                fixture,
                model="direct-model",
                reasoning_effort='medium"\ninjected=true',
                model_tool_mode_value="direct",
            )
            self.assertIn('model_reasoning_effort="medium\\"\\ninjected=true"', command)

    def test_prompt_requires_exact_retrieval_budget_and_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory))
            value = prompt(fixture)
            self.assertIn("exactly two read_tool_output calls", value)
            self.assertIn("Do not use lines mode", value)
            self.assertIn("TOOL_OUTPUT_SAVINGS_E2E_SUCCESS", value)

    def test_code_mode_only_lane_enables_the_validated_host(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory))
            command = build_cli_command(
                Path("codex"),
                fixture,
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                model_tool_mode_value="code_mode_only",
            )
        self.assertIn("code_mode_host", command)
        self.assertNotIn("--disable", command)

    def test_direct_catalog_mode_is_accepted(self) -> None:
        self.assertEqual(
            model_tool_mode(
                REPO_ROOT / "codex-rs/models-manager/models.json", "gpt-5.5"
            ),
            "direct",
        )

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_failed_cli_preserves_partial_thread_id_for_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, sys\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n"
                "print('{partial-json', flush=True)\n"
                "raise SystemExit(7)\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            with self.assertRaises(CliRunError) as raised:
                run_cli(
                    fake,
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=2.0,
                )
            self.assertEqual(
                raised.exception.thread_id, "00000000-0000-0000-0000-000000000001"
            )
            self.assertTrue(raised.exception.process_cleanup_confirmed)

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_cli_launch_failure_reports_that_no_process_started(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = build_fixture(root)
            with (
                patch(
                    "tool_output_savings_process.subprocess.Popen",
                    side_effect=OSError("forced launch failure"),
                ),
                self.assertRaises(CliRunError) as raised,
            ):
                run_cli(
                    Path("/bin/codex"),
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=2.0,
                )
            self.assertIsNone(raised.exception.thread_id)
            self.assertFalse(raised.exception.process_started)
            self.assertTrue(raised.exception.process_cleanup_confirmed)

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_cli_process_environment_does_not_expose_unlisted_secrets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n"
                "print(os.environ.get('E2E_TEST_SECRET', 'not-inherited'), file=sys.stderr, flush=True)\n"
                "raise SystemExit(7)\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            codex_home = root / "codex-home"
            with patch.dict(os.environ, {"E2E_TEST_SECRET": "do-not-inherit"}):
                with self.assertRaises(CliRunError):
                    run_cli(
                        fake,
                        fixture,
                        model="gpt-5.5",
                        reasoning_effort="medium",
                        model_tool_mode_value="direct",
                        codex_home=codex_home,
                        sqlite_home=None,
                        timeout=2.0,
                    )
            self.assertEqual(
                (fixture.root / "exec.stderr").read_text(encoding="utf-8").strip(),
                "not-inherited",
            )

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_timed_out_cli_preserves_partial_thread_id(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, time\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n"
                "time.sleep(2)\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            with self.assertRaises(CliRunError) as raised:
                run_cli(
                    fake,
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=0.5,
                )
            self.assertEqual(
                raised.exception.thread_id, "00000000-0000-0000-0000-000000000001"
            )
            self.assertTrue(raised.exception.process_cleanup_confirmed)

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_cli_process_finalization_failure_is_not_cleanup_confirmed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            with (
                patch(
                    "tool_output_savings_process.terminate_process",
                    side_effect=HarnessError("forced_process_termination_failure"),
                ),
                self.assertRaises(CliRunError) as raised,
            ):
                run_cli(
                    fake,
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=2.0,
                )
            self.assertEqual(
                raised.exception.thread_id, "00000000-0000-0000-0000-000000000001"
            )
            self.assertFalse(raised.exception.process_cleanup_confirmed)

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_zero_exit_malformed_trailing_json_preserves_partial_thread_id(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n"
                "print('{trailing', flush=True)\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            with self.assertRaises(CliRunError) as raised:
                run_cli(
                    fake,
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=2.0,
                )
            self.assertEqual(
                raised.exception.thread_id, "00000000-0000-0000-0000-000000000001"
            )

    @unittest.skipUnless(
        sys.platform == "darwin", "live subprocess scope is macOS-only"
    )
    def test_capture_limit_failure_preserves_partial_thread_id(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            fixture = build_fixture(fixture_root)
            fake = root / "fake.py"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, sys\n"
                "print(json.dumps({'type':'thread.started','thread_id':'00000000-0000-0000-0000-000000000001'}), flush=True)\n"
                f"sys.stdout.write('x' * {MAX_CAPTURE_BYTES + 1})\n"
                "sys.stdout.flush()\n",
                encoding="utf-8",
            )
            fake.chmod(0o700)
            with self.assertRaises(CliRunError) as raised:
                run_cli(
                    fake,
                    fixture,
                    model="gpt-5.5",
                    reasoning_effort="medium",
                    model_tool_mode_value="direct",
                    codex_home=None,
                    sqlite_home=None,
                    timeout=2.0,
                )
            self.assertEqual(
                raised.exception.thread_id, "00000000-0000-0000-0000-000000000001"
            )

    def test_delete_pins_the_measured_database_state_home(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "sqlite" / "state_5.sqlite"
            database.parent.mkdir()
            rollout = root / "rollouts" / "thread.jsonl"
            row = Mock(rollout_path=str(rollout))
            with (
                patch("tool_output_savings_cleanup.read_thread_row", return_value=row),
                patch("tool_output_savings_cleanup.run_bounded_command") as run,
            ):
                targets = delete_thread(
                    Path("/bin/codex"),
                    "00000000-0000-0000-0000-000000000001",
                    root / "codex-home",
                    None,
                    1.0,
                    database=database,
                )
            command = run.call_args.args[0]
            self.assertIn(
                f"sqlite_home={json.dumps(str((root / 'sqlite').resolve()))}", command
            )
            self.assertEqual(
                run.call_args.kwargs["environment"]["CODEX_HOME"],
                str(root / "codex-home"),
            )
            self.assertNotIn("OPENAI_API_KEY", run.call_args.kwargs["environment"])
            self.assertEqual(targets.rollout_path, rollout.absolute())
            self.assertEqual(
                targets.artifact_directory,
                (root / "codex-home" / "tool_outputs" / THREAD_ID).absolute(),
            )

    def test_delete_rejects_a_state_home_that_does_not_own_the_database(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "measured" / "state_5.sqlite"
            database.parent.mkdir()
            with patch("tool_output_savings_cleanup.run_bounded_command") as run:
                with self.assertRaisesRegex(
                    HarnessError, "state_db_configuration_conflict"
                ):
                    delete_thread(
                        Path("/bin/codex"),
                        "00000000-0000-0000-0000-000000000001",
                        root / "codex-home",
                        root / "different-state-home",
                        1.0,
                        database=database,
                    )
            run.assert_not_called()

    def test_delete_rejects_a_rollout_that_differs_from_the_measured_target(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "sqlite" / "state_5.sqlite"
            database.parent.mkdir()
            row = Mock(rollout_path=str(root / "rollouts" / "current.jsonl"))
            with (
                patch("tool_output_savings_cleanup.read_thread_row", return_value=row),
                patch("tool_output_savings_cleanup.run_bounded_command") as run,
                self.assertRaisesRegex(HarnessError, "thread_cleanup_rollout_mismatch"),
            ):
                delete_thread(
                    Path("/bin/codex"),
                    THREAD_ID,
                    root / "codex-home",
                    None,
                    1.0,
                    database=database,
                    rollout_path=root / "rollouts" / "measured.jsonl",
                )
            run.assert_not_called()

    def test_deletion_proof_requires_database_rollout_and_artifact_absence(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rollout_target = root / "missing-rollout-target"
            rollout = root / "rollout.jsonl"
            rollout.symlink_to(rollout_target)
            artifact_target = root / "missing-artifact-target"
            artifact_directory = root / "tool-output-thread"
            artifact_directory.symlink_to(artifact_target)
            targets = ThreadDeletionTargets(
                database=root / "state_5.sqlite",
                thread_id=THREAD_ID,
                rollout_path=rollout,
                artifact_directory=artifact_directory,
            )
            with (
                patch("tool_output_savings_cleanup.read_thread_row", return_value=None),
                self.assertRaisesRegex(HarnessError, "thread_cleanup_not_verified"),
            ):
                verify_thread_deleted(targets, timeout=0.01)
            rollout.unlink()
            artifact_directory.unlink()
            with patch(
                "tool_output_savings_cleanup.read_thread_row", return_value=None
            ):
                verify_thread_deleted(targets, timeout=0.01)


if __name__ == "__main__":
    unittest.main()
