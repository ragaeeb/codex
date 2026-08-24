import tempfile
import unittest
import json
from pathlib import Path

from tool_output_savings_fixture import build_cli_command
from tool_output_savings_fixture import build_fixture
from tool_output_savings_context_budget import ContextBudgetMetrics
from tool_output_savings_stage3 import STAGE3_RULE
from tool_output_savings_stage3 import STAGE3_RESULT_TYPE
from tool_output_savings_stage3 import STAGE3_RETRY_SENTINEL
from tool_output_savings_stage3 import STAGE3_RETRY_TYPE
from tool_output_savings_stage3 import STAGE3_SUCCESS_SENTINEL
from tool_output_savings_stage3 import _outer_result
from tool_output_savings_stage3 import _model_request_count
from tool_output_savings_stage3 import _context_budget_fields
from tool_output_savings_stage3 import stage3_correction_program
from tool_output_savings_stage3 import stage3_code_mode_program
from tool_output_savings_stage3 import stage3_code_mode_prompt
from tool_output_savings_stage3 import _repair_receipt
from tool_output_savings_stage3_runner import failure_rule
from tool_output_savings_stage3_runner import paired_deltas


class Stage3LaneTests(unittest.TestCase):
    def test_context_budget_report_uses_the_metrics_public_field_names(self) -> None:
        metrics = ContextBudgetMetrics(
            bounded_item_count=2,
            bounded_item_bytes=2048,
            max_bounded_item_bytes=1024,
            exempt_item_count=1,
            exempt_item_bytes=512,
            max_exempt_item_bytes=512,
        )
        self.assertEqual(
            _context_budget_fields(metrics),
            {
                "context_item_approx_bytes": 2048,
                "context_item_approx_count": 2,
                "context_item_max_bytes": 1024,
                "context_items_below_1k_tokens": True,
            },
        )

    def test_unexpected_failure_rule_discloses_only_the_exception_class(self) -> None:
        self.assertEqual(
            failure_rule(ValueError("secret detail")),
            "stage3_harness_unexpected_failure_value_error",
        )

    @staticmethod
    def _records(programs: list[str], values: list[object]) -> list[dict[str, object]]:
        records = []
        for index, (program, value) in enumerate(zip(programs, values, strict=True)):
            call_id = f"call-{index}"
            records.extend(
                [
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "custom_tool_call",
                            "name": "exec",
                            "call_id": call_id,
                            "input": program,
                        },
                    },
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "custom_tool_call_output",
                            "call_id": call_id,
                            "output": [
                                {
                                    "type": "input_text",
                                    "text": "Script completed\nWall time 0.1 seconds\nOutput:\n",
                                },
                                {
                                    "type": "input_text",
                                    "text": value
                                    if isinstance(value, str)
                                    else json.dumps(value),
                                },
                            ],
                        },
                    },
                ]
            )
        return records

    @staticmethod
    def _success_value(*, repaired: bool) -> dict[str, object]:
        return {
            "type": STAGE3_RESULT_TYPE,
            "version": 1,
            "sentinel": STAGE3_SUCCESS_SENTINEL,
            "first_attempt": {"outcome": "success" if repaired else "handler_error"},
            "correction": {
                "performed": not repaired,
                "outcome": "not_needed" if repaired else "success",
            },
            "nested_dispatches": 1 if repaired else 2,
            "handler_attempts": 1 if repaired else 2,
            "handler_successes": 1,
            "invalid_argument_outputs": 0 if repaired else 1,
            "repair_activated": repaired,
            "repair_rules": [STAGE3_RULE] if repaired else [],
            "marker_present": True,
            "validated": True,
        }

    def test_pair_uses_identical_fixture_program_and_explicit_feature_switches(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as first,
            tempfile.TemporaryDirectory() as second,
        ):
            left = build_fixture(Path(first), suffix="stage3-test-fixture")
            right = build_fixture(Path(second), suffix="stage3-test-fixture")
            self.assertEqual(left.window_bytes, right.window_bytes)
            self.assertEqual(left.window_middle, right.window_middle)
            self.assertEqual(
                stage3_code_mode_program(left), stage3_code_mode_program(right)
            )
            program = stage3_code_mode_program(left)
            self.assertIn(
                f"offset: {left.window_middle_offset}",
                program,
            )
            self.assertIn('max_bytes: "1024"', program)
            self.assertIn("max_bytes: 1024", stage3_correction_program(left))
            self.assertLessEqual(
                left.window_middle_offset + len(left.window_middle.encode("utf-8")),
                left.window_middle_offset + 1024,
            )
            self.assertIn(
                'if (first?.type !== "file_read") firstError = true;',
                stage3_code_mode_program(left),
            )
            self.assertIn("numeric_string_typed", stage3_code_mode_program(left))
            self.assertIn(
                "--disable",
                build_cli_command(
                    Path("/repo/codex"),
                    left,
                    model="gpt-5.6-luna",
                    reasoning_effort="medium",
                    model_tool_mode_value="code_mode_only",
                    tool_argument_repair=False,
                    stage3=True,
                ),
            )
            enabled = build_cli_command(
                Path("/repo/codex"),
                left,
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                model_tool_mode_value="code_mode_only",
                tool_argument_repair=True,
                stage3=True,
            )
            self.assertEqual(enabled[enabled.index("--enable") + 1], "native_read_file")
            self.assertIn("tool_argument_repair", enabled)
            self.assertIn(stage3_code_mode_prompt(left), enabled)
            self.assertIn(stage3_correction_program(left), enabled[-1])

    def test_receipt_is_stable_and_absent_when_feature_is_off(self) -> None:
        records = [
            {
                "type": "response_item",
                "metadata": {
                    "tool_argument_repair": {
                        "tool_family": "read_file",
                        "outcome": "repaired",
                        "rules": [STAGE3_RULE],
                        "input_bytes": 72,
                        "effective_bytes": 70,
                        "candidate_work": 1,
                        "repair_duration_micros": 12,
                    }
                },
            }
        ]
        self.assertEqual(_repair_receipt([], False), None)
        receipt = _repair_receipt(records, True)
        self.assertEqual(receipt["rules"], [STAGE3_RULE])
        self.assertNotIn("max_bytes", receipt)

    def test_pair_deltas_report_raw_tokens_and_net_new_input(self) -> None:
        base = {
            "task_success": True,
            "model": "gpt-5.6-luna",
            "reasoning_effort": "medium",
            "model_request_count": 2,
            "first_attempt_dispatch_success": False,
            "repair_rule_count": 0,
            "net_new_input_tokens": 80,
            "token_totals": {
                "input_tokens": 100,
                "cached_input_tokens": 20,
                "cache_write_input_tokens": 0,
                "output_tokens": 30,
                "reasoning_output_tokens": 10,
                "total_tokens": 130,
            },
        }
        enabled = {
            **base,
            "model_request_count": 1,
            "first_attempt_dispatch_success": True,
            "repair_rule_count": 1,
            "net_new_input_tokens": 70,
            "token_totals": {**base["token_totals"], "total_tokens": 120},
        }
        deltas = paired_deltas(base, enabled)
        self.assertEqual(deltas["model_request_count"], {"off": 2, "on": 1})
        self.assertEqual(deltas["token_deltas"]["total_tokens"]["absolute_delta"], -10)
        self.assertEqual(deltas["net_new_input_tokens"]["absolute_delta"], -10)

    def test_pair_deltas_require_one_fewer_model_request_when_repair_is_enabled(
        self,
    ) -> None:
        run = {
            "task_success": True,
            "model": "gpt-5.6-luna",
            "reasoning_effort": "medium",
            "model_request_count": 2,
            "first_attempt_dispatch_success": False,
            "repair_rule_count": 0,
            "net_new_input_tokens": 80,
            "token_totals": {
                "input_tokens": 100,
                "cached_input_tokens": 20,
                "cache_write_input_tokens": 0,
                "output_tokens": 30,
                "reasoning_output_tokens": 10,
                "total_tokens": 130,
            },
        }
        enabled = {
            **run,
            "first_attempt_dispatch_success": True,
            "repair_rule_count": 1,
        }
        with self.assertRaisesRegex(Exception, "stage3_pair_request_reduction"):
            paired_deltas(run, enabled)

    def test_outer_result_requires_model_visible_retry_only_when_feature_is_off(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory), suffix="stage3-outer-result")
            first = stage3_code_mode_program(fixture)
            correction = stage3_correction_program(fixture)
            retry = {
                "type": STAGE3_RETRY_TYPE,
                "version": 1,
                "sentinel": STAGE3_RETRY_SENTINEL,
            }
            repaired = self._success_value(repaired=True)
            corrected = self._success_value(repaired=False)
            self.assertEqual(
                _outer_result(self._records([first], [repaired]), fixture, True),
                repaired,
            )
            self.assertEqual(
                _model_request_count(self._records([first], [repaired])),
                2,
            )
            off_records = self._records([first, correction], [retry, corrected])
            self.assertEqual(
                _outer_result(off_records, fixture, False),
                corrected,
            )
            self.assertEqual(_model_request_count(off_records), 3)

    def test_outer_result_classifies_completed_non_json_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory), suffix="stage3-non-json")
            records = self._records(
                [stage3_code_mode_program(fixture)], ["stage3 marker proof failed"]
            )
            with self.assertRaisesRegex(Exception, "stage3_output_not_json"):
                _outer_result(records, fixture, True)

    def test_outer_result_rejects_output_before_call(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = build_fixture(Path(directory), suffix="stage3-output-order")
            records = self._records(
                [stage3_code_mode_program(fixture)],
                [self._success_value(repaired=True)],
            )
            records.reverse()
            with self.assertRaisesRegex(Exception, "stage3_outer_call_output_order"):
                _outer_result(records, fixture, True)


if __name__ == "__main__":
    unittest.main()
