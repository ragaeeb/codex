import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path

from tool_output_savings_code_mode_lane import CODE_MODE_EVALUATION_LANE
from tool_output_savings_code_mode_lane import analyze_code_mode
from tool_output_savings_code_mode_lane import code_mode_program
from tool_output_savings_code_mode_lane import code_mode_prompt
from tool_output_savings_code_mode_test_support import synthetic_code_mode_case
from tool_output_savings_evidence import HarnessError
from tool_output_savings_fixture import build_cli_command


class CodeModeLaneTests(unittest.TestCase):
    def test_code_mode_allows_bootstrap_user_context_but_rejects_duplicate_prompt(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            user_index = next(
                index
                for index, record in enumerate(records)
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "message"
                and record.get("payload", {}).get("role") == "user"
            )
            records.insert(
                user_index,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "CLI bootstrap context"}
                        ],
                    },
                },
            )
            report = analyze_code_mode(
                events,
                "".join(json.dumps(record) + "\n" for record in records),
                records,
                fixture,
                home,
                row.id,
                row,
                usage,
                cli_version="1.2.3",
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                binary_sha256="a" * 64,
                binary_built_in_run=True,
                repository_commit=None,
                working_tree_dirty=True,
                model_tool_mode="code_mode_only",
            )
            self.assertEqual(report["status"], "pass")

            duplicate = copy.deepcopy(records)
            duplicate.insert(
                user_index,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": code_mode_prompt(fixture)}
                        ],
                    },
                },
            )
            with self.assertRaisesRegex(HarnessError, "evaluation_prompt_count"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in duplicate),
                    duplicate,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )

    def test_luna_command_enables_host_and_pins_exact_program(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, *_ = synthetic_code_mode_case(Path(directory))
            command = build_cli_command(
                Path("/repo/target/release/codex"),
                fixture,
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                model_tool_mode_value="code_mode_only",
            )
            self.assertIn("code_mode_host", command)
            self.assertNotIn("--disable", command)
            self.assertEqual(command[-1], code_mode_prompt(fixture))
            self.assertIn(code_mode_program(fixture), command[-1])
            self.assertNotIn("tools.read_tool_output", code_mode_program(fixture))
            self.assertIn("top-level read_tool_output", command[-1])

    def test_code_mode_analysis_proves_stage1_and_labels_stage2_inline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            report = analyze_code_mode(
                events,
                "".join(json.dumps(record) + "\n" for record in records),
                records,
                fixture,
                home,
                row.id,
                row,
                usage,
                cli_version="1.2.3",
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                binary_sha256="a" * 64,
                binary_built_in_run=True,
                repository_commit=None,
                working_tree_dirty=True,
                model_tool_mode="code_mode_only",
            )
            self.assertEqual(report["evaluation_lane"], CODE_MODE_EVALUATION_LANE)
            self.assertTrue(report["invariants"]["stage1_artifact_recoverable"])
            self.assertEqual(
                report["stage2_code_mode_scope"],
                "duplicate reads inline by design; no Stage 2 artifact projection claim",
            )
            self.assertNotIn("stage2_duplicate_envelope_bytes", report)

    def test_code_mode_analysis_rejects_non_exact_program(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            call = next(
                record["payload"]
                for record in records
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call"
            )
            call["input"] += "\ntext('forged');"
            with self.assertRaisesRegex(HarnessError, "code_mode_program_mismatch"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in records),
                    records,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )

    def test_code_mode_analysis_accepts_one_trailing_line_feed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            call = next(
                record["payload"]
                for record in records
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call"
            )
            self.assertFalse(call["input"].endswith("\n"))
            call["input"] += "\n"
            report = analyze_code_mode(
                events,
                "".join(json.dumps(record) + "\n" for record in records),
                records,
                fixture,
                home,
                row.id,
                row,
                usage,
                cli_version="1.2.3",
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                binary_sha256="a" * 64,
                binary_built_in_run=True,
                repository_commit=None,
                working_tree_dirty=True,
                model_tool_mode="code_mode_only",
            )
            self.assertTrue(report["invariants"]["outer_code_mode_program_exact"])

    def test_code_mode_analysis_rejects_other_trailing_newlines(self) -> None:
        for suffix in ("\r\n", "\n\n"):
            with (
                self.subTest(suffix=repr(suffix)),
                tempfile.TemporaryDirectory() as directory,
            ):
                fixture, home, events, records, row, usage = synthetic_code_mode_case(
                    Path(directory)
                )
                call = next(
                    record["payload"]
                    for record in records
                    if record.get("type") == "response_item"
                    and record.get("payload", {}).get("type") == "custom_tool_call"
                )
                call["input"] += suffix
                with self.assertRaisesRegex(HarnessError, "code_mode_program_mismatch"):
                    analyze_code_mode(
                        events,
                        "".join(json.dumps(record) + "\n" for record in records),
                        records,
                        fixture,
                        home,
                        row.id,
                        row,
                        usage,
                        cli_version="1.2.3",
                        model="gpt-5.6-luna",
                        reasoning_effort="medium",
                        binary_sha256="a" * 64,
                        binary_built_in_run=True,
                        repository_commit=None,
                        working_tree_dirty=True,
                        model_tool_mode="code_mode_only",
                    )

    def test_code_mode_analysis_rejects_extra_outer_output_blocks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            output = next(
                record["payload"]
                for record in records
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call_output"
            )
            output["output"].append({"type": "input_text", "text": "forged"})
            with self.assertRaisesRegex(HarnessError, "code_mode_output_shape"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in records),
                    records,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )

    def test_code_mode_analysis_rejects_forged_runner_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            output = next(
                record["payload"]
                for record in records
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call_output"
            )
            output["output"][0]["text"] = (
                "Script failed\nWall time 0.1 seconds\nOutput:\n"
            )
            with self.assertRaisesRegex(HarnessError, "code_mode_status_invalid"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in records),
                    records,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )

    def test_code_mode_analysis_rejects_retrieval_before_exec_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            exec_output = next(
                index
                for index, record in enumerate(records)
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call_output"
            )
            search_call = next(
                index
                for index, record in enumerate(records)
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("call_id") == "stage1-search"
                and record.get("payload", {}).get("type") == "function_call"
            )
            records[exec_output], records[search_call] = (
                records[search_call],
                records[exec_output],
            )
            with self.assertRaisesRegex(HarnessError, "code_mode_retrieval_order"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in records),
                    records,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )

    @unittest.skipUnless(sys.platform == "darwin", "macOS symlink-prefix contract")
    def test_code_mode_analysis_rejects_resolved_macos_path_alias(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, usage = synthetic_code_mode_case(
                Path(directory)
            )
            output = next(
                record["payload"]
                for record in records
                if record.get("type") == "response_item"
                and record.get("payload", {}).get("type") == "custom_tool_call_output"
            )
            evidence = json.loads(output["output"][1]["text"])
            lexical_path = str(fixture.root / fixture.window_name)
            resolved_path = str((fixture.root / fixture.window_name).resolve())
            self.assertNotEqual(lexical_path, resolved_path)
            evidence["stage2"]["path"] = resolved_path
            output["output"][1]["text"] = json.dumps(
                evidence, ensure_ascii=False, separators=(",", ":")
            )
            with self.assertRaisesRegex(HarnessError, "file_read_path_invalid"):
                analyze_code_mode(
                    events,
                    "".join(json.dumps(record) + "\n" for record in records),
                    records,
                    fixture,
                    home,
                    row.id,
                    row,
                    usage,
                    cli_version="1.2.3",
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    binary_sha256="a" * 64,
                    binary_built_in_run=True,
                    repository_commit=None,
                    working_tree_dirty=True,
                    model_tool_mode="code_mode_only",
                )


if __name__ == "__main__":
    unittest.main()
