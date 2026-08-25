import copy
import base64
import json
import sys
import tempfile
import unittest
from pathlib import Path

from test_tool_output_savings_analysis import run_analysis
from tool_output_savings_evidence import HarnessError
from tool_output_savings_fixture import prompt
from tool_output_savings_test_support import synthetic_case


def message_record(role: str, content: object) -> dict[str, object]:
    return {
        "type": "response_item",
        "payload": {"type": "message", "role": role, "content": content},
    }


def insert_before_assistant(
    records: list[dict[str, object]], record: dict[str, object]
) -> None:
    index = next(
        index
        for index, item in enumerate(records)
        if isinstance(item.get("payload"), dict)
        and item["payload"].get("type") == "message"
        and item["payload"].get("role") == "assistant"
    )
    records.insert(index, record)


@unittest.skipUnless(sys.platform == "darwin", "artifact checks are macOS-only")
class ContextBudgetTests(unittest.TestCase):
    def test_effective_context_uses_explicit_ten_thousand_token_byte_estimate(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            accepted = copy.deepcopy(records)
            insert_before_assistant(
                accepted,
                message_record("developer", "D" * 25_000),
            )
            report = run_analysis(fixture, home, events, accepted, row, terminal)
            self.assertEqual(report["tool_output_model_visible_cap_bytes"], 8_192)
            self.assertEqual(report["effective_context_item_approx_token_cap"], 10_000)
            self.assertEqual(report["context_token_estimator_bytes_per_token"], 4)
            self.assertEqual(report["effective_context_item_approx_byte_cap"], 40_000)

            rejected = copy.deepcopy(records)
            insert_before_assistant(
                rejected,
                message_record("developer", "D" * 40_000),
            )
            with self.assertRaisesRegex(HarnessError, "context_item_over_cap"):
                run_analysis(fixture, home, events, rejected, row, terminal)

    def test_exact_single_user_prompt_and_assistant_sentinel_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            report = run_analysis(fixture, home, events, records, row, terminal)
            self.assertEqual(report["manual_review_status"], "external_review_required")

            changed = copy.deepcopy(records)
            user = next(
                item["payload"]
                for item in changed
                if isinstance(item.get("payload"), dict)
                and item["payload"].get("type") == "message"
                and item["payload"].get("role") == "user"
            )
            user["content"] = prompt(fixture) + " altered"
            with self.assertRaisesRegex(HarnessError, "user_prompt_mismatch"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            insert_before_assistant(
                changed,
                message_record(
                    "user",
                    "Bootstrap context supplied by the CLI before the evaluation prompt.",
                ),
            )
            report = run_analysis(fixture, home, events, changed, row, terminal)
            self.assertGreaterEqual(report["bounded_effective_context_item_count"], 2)

            changed = copy.deepcopy(records)
            insert_before_assistant(changed, message_record("user", prompt(fixture)))
            with self.assertRaisesRegex(HarnessError, "evaluation_prompt_count"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            assistant = next(
                item["payload"]
                for item in changed
                if isinstance(item.get("payload"), dict)
                and item["payload"].get("type") == "message"
                and item["payload"].get("role") == "assistant"
            )
            assistant["content"] = [
                {"type": "output_text", "text": " TOOL_OUTPUT_SAVINGS_E2E_SUCCESS "}
            ]
            with self.assertRaisesRegex(HarnessError, "assistant_sentinel_mismatch"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_effective_non_reasoning_items_have_a_hard_serialized_cap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            mutants = []
            oversized_user = copy.deepcopy(records)
            user = next(
                item["payload"]
                for item in oversized_user
                if isinstance(item.get("payload"), dict)
                and item["payload"].get("type") == "message"
                and item["payload"].get("role") == "user"
            )
            user["content"] = "Z" * 50_000
            mutants.append(oversized_user)
            mutants.append(
                [
                    *copy.deepcopy(records),
                    {"type": "compacted", "payload": {"message": "Z" * 50_000}},
                ]
            )
            mutants.append(
                [
                    *copy.deepcopy(records),
                    {
                        "type": "replacement_history",
                        "payload": {"items": [{"content": "Z" * 50_000}]},
                    },
                ]
            )
            for changed in mutants:
                with self.subTest(record_type=changed[-1].get("type")):
                    with self.assertRaisesRegex(HarnessError, "context_item_over_cap"):
                        run_analysis(fixture, home, events, changed, row, terminal)

    def test_reasoning_and_encrypted_items_are_measured_but_not_claimed_bounded(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            insert_before_assistant(
                changed,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "reasoning",
                        "summary": [],
                        "encrypted_content": "R" * 50_000,
                    },
                },
            )
            insert_before_assistant(
                changed,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "context_compaction",
                        "encrypted_content": "E" * 50_000,
                    },
                },
            )
            changed.append(
                {
                    "type": "compacted",
                    "payload": {
                        "replacement_history": [
                            {
                                "type": "compaction_summary",
                                "encrypted_content": "C" * 50_000,
                            }
                        ]
                    },
                }
            )
            report = run_analysis(fixture, home, events, changed, row, terminal)
            self.assertEqual(report["exempt_reasoning_encrypted_item_count"], 3)
            self.assertGreater(report["exempt_reasoning_encrypted_item_bytes"], 150_000)
            self.assertTrue(
                report["invariants"]["reasoning_encrypted_items_measured_not_bounded"]
            )

    def test_plaintext_inside_provider_managed_items_remains_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            insert_before_assistant(
                changed,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "reasoning",
                        "summary": "P" * 50_000,
                        "encrypted_content": "E" * 50_000,
                    },
                },
            )
            with self.assertRaisesRegex(HarnessError, "context_item_over_cap"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            insert_before_assistant(
                changed,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "reasoning",
                        "encrypted_content": {"plaintext": "P" * 50_000},
                    },
                },
            )
            with self.assertRaisesRegex(HarnessError, "encrypted_content_invalid"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            changed.append(
                {
                    "type": "compacted",
                    "payload": {
                        "metadata": {
                            "type": "reasoning",
                            "encrypted_content": "Z" * 50_000,
                        }
                    },
                }
            )
            with self.assertRaisesRegex(HarnessError, "context_item_over_cap"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_recursive_json_decoding_has_a_hard_depth_limit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            nested = "fixture-leaf"
            for _ in range(20):
                nested = json.dumps(nested)
            for metadata in ({"value": nested}, {nested: "value"}):
                changed = copy.deepcopy(records)
                changed.append({"type": "event_msg", "payload": {"metadata": metadata}})
                with self.subTest(metadata_kind=next(iter(metadata)) == "value"):
                    with self.assertRaisesRegex(
                        HarnessError, "persisted_content_decode_depth"
                    ):
                        run_analysis(fixture, home, events, changed, row, terminal)

    def test_partial_stage1_copies_that_erase_savings_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            body = fixture.emitter_output_bytes.decode("utf-8")
            copies = {
                "one_byte_short": body[:-1],
                "ninety_five_percent": body[: int(len(body) * 0.95)],
                "one_character_changed_per_line": "\n".join(
                    line[:-1] + "y" if line.startswith("emitter-line-") else line
                    for line in body.splitlines()
                )
                + "\n",
            }
            for name, leaked in copies.items():
                changed = copy.deepcopy(records)
                changed.append(
                    {"type": "event_msg", "payload": {"metadata": {"leak": leaked}}}
                )
                with self.subTest(name=name):
                    with self.assertRaisesRegex(
                        HarnessError, "stage1_fixture_content_over_budget"
                    ):
                        run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            leaked = copies["ninety_five_percent"]
            changed.extend(
                {
                    "type": "event_msg",
                    "payload": {"metadata": {"part": leaked[index : index + 4_000]}},
                }
                for index in range(0, len(leaked), 4_000)
            )
            with self.assertRaisesRegex(
                HarnessError, "stage1_fixture_content_over_budget"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_split_base64_stage1_copy_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            encoded = base64.b64encode(fixture.emitter_output_bytes).decode("ascii")
            changed = copy.deepcopy(records)
            changed.extend(
                {
                    "type": "event_msg",
                    "payload": {
                        "type": "notice",
                        "metadata": {"part": encoded[index : index + 4_000]},
                    },
                }
                for index in range(0, len(encoded), 4_000)
            )
            with self.assertRaisesRegex(
                HarnessError, "encoded_stage1_payload_in_rollout"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            for name, wrapped in {
                "data_uri": f"data:text/plain;base64,{encoded}",
                "prefixed": f"base64:{encoded}",
                "alphabetic_wrapper": f"BEGIN{encoded}END",
            }.items():
                changed = copy.deepcopy(records)
                changed.append(
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "notice",
                            "metadata": {"encoded": wrapped},
                        },
                    }
                )
                with self.subTest(name=name):
                    with self.assertRaisesRegex(
                        HarnessError, "encoded_stage1_payload_in_rollout"
                    ):
                        run_analysis(fixture, home, events, changed, row, terminal)

            variants = {
                "ninety_five_percent": (
                    encoded[: int(len(encoded) * 0.95)],
                    "stage1_fixture_content_over_budget",
                ),
                "one_byte_short": (
                    encoded[:-1],
                    "encoded_stage1_payload_in_rollout",
                ),
                "one_quartet_fragment_short": (
                    encoded[:-3],
                    "stage1_fixture_content_over_budget",
                ),
            }
            for name, (partial, failure_rule) in variants.items():
                changed = copy.deepcopy(records)
                changed.extend(
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "notice",
                            "metadata": {"part": partial[index : index + 4_000]},
                        },
                    }
                    for index in range(0, len(partial), 4_000)
                )
                with self.subTest(name=name):
                    with self.assertRaisesRegex(HarnessError, failure_rule):
                        run_analysis(fixture, home, events, changed, row, terminal)

            mime_wrapped = "\n".join(
                encoded[index : index + 76] for index in range(0, len(encoded), 76)
            )
            changed = copy.deepcopy(records)
            changed.append(
                {
                    "type": "event_msg",
                    "payload": {
                        "type": "notice",
                        "metadata": {"encoded": mime_wrapped},
                    },
                }
            )
            with self.assertRaisesRegex(
                HarnessError, "encoded_stage1_payload_in_rollout"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_base64_stage2_window_copy_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            inline = next(
                json.loads(item["payload"]["output"])
                for item in records
                if isinstance(item.get("payload"), dict)
                and item["payload"].get("call_id") == "read-a"
                and item["payload"].get("type") == "function_call_output"
            )
            encoded = base64.b64encode(inline["window"]["text"].encode()).decode()
            for wrapped in (encoded, f"data:text/plain;base64,{encoded}"):
                changed = copy.deepcopy(records)
                changed.append(
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "notice",
                            "metadata": {"encoded": wrapped},
                        },
                    }
                )
                with self.assertRaisesRegex(
                    HarnessError, "duplicated_file_read_window_in_rollout"
                ):
                    run_analysis(fixture, home, events, changed, row, terminal)

    def test_escaped_chunks_and_mapping_keys_are_included_in_content_accounting(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            body = fixture.emitter_output_bytes.decode("utf-8")
            escaped = json.dumps(body, ensure_ascii=False)[1:-1]
            changed = copy.deepcopy(records)
            midpoint = len(escaped) // 2
            changed.extend(
                [
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"part": escaped[:midpoint]}},
                    },
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"part": escaped[midpoint:]}},
                    },
                ]
            )
            with self.assertRaisesRegex(
                HarnessError, "stage1_fixture_content_over_budget"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            changed.append(
                {"type": "event_msg", "payload": {"metadata": {body: "value"}}}
            )
            with self.assertRaisesRegex(
                HarnessError, "stage1_fixture_content_over_budget"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_partial_stage2_window_copy_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            inline = next(
                json.loads(item["payload"]["output"])
                for item in records
                if isinstance(item.get("payload"), dict)
                and item["payload"].get("call_id") == "read-a"
                and item["payload"].get("type") == "function_call_output"
            )
            window = inline["window"]["text"]
            changed = copy.deepcopy(records)
            changed.append(
                {
                    "type": "event_msg",
                    "payload": {
                        "metadata": {"copy": window[: int(len(window) * 0.95)]}
                    },
                }
            )
            with self.assertRaisesRegex(
                HarnessError, "stage2_fixture_content_over_budget"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            escaped = json.dumps(window, ensure_ascii=True)[1:-1]
            midpoint = len(escaped) // 2
            changed = copy.deepcopy(records)
            changed.extend(
                [
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"part": escaped[:midpoint]}},
                    },
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"part": escaped[midpoint:]}},
                    },
                ]
            )
            with self.assertRaisesRegex(
                HarnessError, "stage2_fixture_content_over_budget"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)


if __name__ == "__main__":
    unittest.main()
