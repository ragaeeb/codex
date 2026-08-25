import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path

from tool_output_savings_analysis import _require_file_read_window
from tool_output_savings_analysis import analyze
from tool_output_savings_content import contains_across_strings
from tool_output_savings_content import count_across_strings
from tool_output_savings_report import redacted_report
from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import MAX_MODEL_VISIBLE_BYTES
from tool_output_savings_test_support import THREAD_ID
from tool_output_savings_test_support import synthetic_case
from tool_output_savings_transcript import tool_response_item_metrics


def transcript(records: list[dict[str, object]]) -> str:
    return "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records)


def run_analysis(
    fixture: object,
    home: Path,
    events: list[dict[str, object]],
    records: list[dict[str, object]],
    row: object,
    terminal: dict[str, int],
) -> dict[str, object]:
    return analyze(
        events,
        transcript(records),
        records,
        fixture,
        home,
        THREAD_ID,
        row,
        terminal,
        cli_version="1.2.3",
        model="gpt-5.6-luna",
        reasoning_effort="medium",
        binary_sha256="a" * 64,
        binary_built_in_run=False,
        repository_commit=None,
        working_tree_dirty=False,
        model_tool_mode="direct",
    )


@unittest.skipUnless(
    sys.platform == "darwin", "artifact descriptor checks are macOS-only"
)
class SavingsAnalysisTests(unittest.TestCase):
    def test_split_content_matching_handles_many_tiny_chunks(self) -> None:
        needle = "a" * 16_384 + "b"
        chunks = [*needle, "unrelated"]
        self.assertTrue(contains_across_strings(iter(chunks), needle))
        self.assertEqual(count_across_strings(iter(chunks), needle), 1)

    def test_complete_tool_item_metric_does_not_reapply_the_output_body_cap(
        self,
    ) -> None:
        raw = "x" * MAX_MODEL_VISIBLE_BYTES
        metrics = tool_response_item_metrics(
            [
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "near-cap",
                        "output": raw,
                    },
                }
            ]
        )
        self.assertGreater(metrics["max_item_bytes"], MAX_MODEL_VISIBLE_BYTES)

    def test_synthetic_transcript_passes_exact_checks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            report = run_analysis(fixture, home, events, records, row, terminal)
            self.assertEqual(report["status"], "pass")
            self.assertEqual(report["retrieval_count"], 4)
            self.assertGreater(report["model_visible_complete_response_item_bytes"], 0)
            self.assertGreater(report["persisted_record_bytes"], 0)
            self.assertIn("external signoff required", report["manual_review_evidence"])
            self.assertTrue(
                report["invariants"]["retrievals_exact_bounded_and_structured"]
            )
            self.assertTrue(report["manual_review_required"])
            self.assertEqual(report["manual_review_threshold_tokens"], 1_000)
            self.assertIn("external signoff required", report["manual_review_evidence"])

    def test_retrieval_offset_and_search_match_must_be_exact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call"
                    and payload.get("call_id") == "s1-bytes"
                ):
                    arguments = json.loads(payload["arguments"])
                    arguments["offset"] += 1
                    payload["arguments"] = json.dumps(arguments)
            with self.assertRaisesRegex(
                HarnessError, "retrieval_window_offset_mismatch"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call_output"
                    and payload.get("call_id") == "s1-search"
                ):
                    value = json.loads(payload["output"])
                    value["byte_offsets"] = [0]
                    payload["output"] = json.dumps(value)
            with self.assertRaisesRegex(
                HarnessError, "retrieval_match_offsets_not_exact"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_read_arguments_digest_and_final_assistant_are_authoritative(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call"
                    and payload.get("name") == "read_file"
                ):
                    arguments = json.loads(payload["arguments"])
                    arguments["max_bytes"] = 31_000
                    payload["arguments"] = json.dumps(arguments)
                    break
            with self.assertRaisesRegex(HarnessError, "read_file_arguments_mismatch"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call_output"
                    and payload.get("call_id") == "read-a"
                ):
                    value = json.loads(payload["output"])
                    value["fingerprint"]["window_digest"] = "sha256:" + "0" * 64
                    payload["output"] = json.dumps(value)
                    break
            with self.assertRaisesRegex(HarnessError, "file_read_digest_mismatch"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "message"
                    and payload.get("role") == "assistant"
                ):
                    payload["role"] = "user"
            with self.assertRaisesRegex(HarnessError, "success_sentinel_missing"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_oversized_extra_output_fails_before_semantic_filtering(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            changed.insert(
                1,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "call_id": "extra",
                        "name": "exec_command",
                        "arguments": "{}",
                    },
                },
            )
            changed.insert(
                2,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "extra",
                        "output": "x" * 100_000,
                    },
                },
            )
            with self.assertRaisesRegex(HarnessError, "function_output_over_cap"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_redacted_report_excludes_sensitive_values(self) -> None:
        secret = "SECRET_CONTENT_PATH_ARGUMENT_ARTIFACT"
        report = redacted_report(
            {"status": "pass", "secret": secret, "model": "gpt-5.6-luna"}
        )
        self.assertNotIn(secret, json.dumps(report))
        self.assertNotIn("secret", report)

    def test_envelope_metadata_and_positive_economics_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            report = run_analysis(fixture, home, events, records, row, terminal)
            self.assertGreater(report["stage1_envelope_byte_reduction_percent"], 0)
            self.assertGreater(report["stage2_incremental_byte_reduction_percent"], 0)
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call_output"
                    and payload.get("call_id") == "read-b"
                ):
                    value = json.loads(payload["output"])
                    value.pop("version")
                    payload["output"] = json.dumps(value)
                    break
            with self.assertRaisesRegex(HarnessError, "artifact_envelope_version"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call_output"
                    and payload.get("call_id") == "read-b"
                ):
                    value = json.loads(payload["output"])
                    value["padding"] = "x" * 7_000
                    payload["output"] = json.dumps(value)
                    break
            with self.assertRaisesRegex(HarnessError, "function_output_over_cap"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_chunked_payload_and_display_wrapped_duplicate_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            emitter = fixture.emitter_output_bytes.decode()
            changed.extend(
                {
                    "type": "compacted",
                    "payload": {"message": emitter[index : index + 4_000]},
                }
                for index in range(0, len(emitter), 4_000)
            )
            with self.assertRaisesRegex(HarnessError, "full_stage1_payload_in_rollout"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            inline = next(
                json.loads(record["payload"]["output"])
                for record in changed
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("type") == "function_call_output"
                and record["payload"].get("call_id") == "read-a"
            )
            changed.append(
                {
                    "type": "compacted",
                    "payload": {"message": "display: " + json.dumps(inline)},
                }
            )
            with self.assertRaisesRegex(
                HarnessError, "duplicated_file_read_window_in_rollout"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_every_persisted_container_is_scanned_for_split_payloads(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            emitter = fixture.emitter_output_bytes.decode()
            changed = copy.deepcopy(records)
            half = len(emitter) // 2
            changed.extend(
                [
                    {
                        "type": "replacement_history",
                        "payload": {"metadata": {"part": emitter[:half]}},
                    },
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"part": emitter[half:]}},
                    },
                ]
            )
            with self.assertRaisesRegex(HarnessError, "full_stage1_payload_in_rollout"):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            inline = next(
                json.loads(record["payload"]["output"])
                for record in changed
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "read-a"
                and record["payload"].get("type") == "function_call_output"
            )
            text = inline["window"]["text"]
            half = len(text) // 2
            changed.extend(
                [
                    {"type": "compacted", "payload": {"message": text[:half]}},
                    {
                        "type": "event_msg",
                        "payload": {"metadata": {"message": text[half:]}},
                    },
                ]
            )
            with self.assertRaisesRegex(
                HarnessError, "duplicated_file_read_window_in_rollout"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            emitter = fixture.emitter_output_bytes.decode()
            changed.append(
                {
                    "type": "compacted",
                    "payload": {
                        "chunks": [
                            json.dumps(emitter[index : index + 4_000])
                            for index in range(0, len(emitter), 4_000)
                        ]
                    },
                }
            )
            with self.assertRaisesRegex(HarnessError, "full_stage1_payload_in_rollout"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_search_requires_single_complete_match_and_no_continuation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "function_call_output"
                    and payload.get("call_id") == "s1-search"
                ):
                    value = json.loads(payload["output"])
                    value["next_offset"] = 1
                    value["complete"] = False
                    payload["output"] = json.dumps(value)
                    break
            with self.assertRaisesRegex(
                HarnessError, "retrieval_search_continuation_invalid"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_retrieval_order_and_artifact_scope_are_causal_and_exhaustive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            search_output_index = next(
                index
                for index, record in enumerate(changed)
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "s1-search"
                and record["payload"].get("type") == "function_call_output"
            )
            search_call_index = next(
                index
                for index, record in enumerate(changed)
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "s1-search"
                and record["payload"].get("type") == "function_call"
            )
            changed[search_output_index], changed[search_call_index] = (
                changed[search_call_index],
                changed[search_output_index],
            )
            with self.assertRaisesRegex(HarnessError, "tool_call_output_order"):
                run_analysis(fixture, home, events, changed, row, terminal)

            extra = b"unreferenced artifact"
            extra_id = "out_" + __import__("hashlib").sha256(extra).hexdigest()
            extra_path = home / "tool_outputs" / THREAD_ID / f"{extra_id}.txt"
            extra_path.write_bytes(extra)
            extra_path.chmod(0o600)
            with self.assertRaisesRegex(
                HarnessError, "unexpected_or_missing_managed_artifact"
            ):
                run_analysis(fixture, home, events, records, row, terminal)

    def test_nested_tool_controls_and_out_of_checklist_order_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            changed.append(
                {
                    "type": "replacement_history",
                    "payload": {
                        "items": [
                            {
                                "type": "function_call",
                                "call_id": "hidden",
                                "name": "exec_command",
                                "arguments": "{}",
                            },
                            {
                                "type": "function_call_output",
                                "call_id": "hidden",
                                "output": "small",
                            },
                        ]
                    },
                }
            )
            with self.assertRaisesRegex(
                HarnessError, "tool_response_item_not_authoritative"
            ):
                run_analysis(fixture, home, events, changed, row, terminal)

            changed = copy.deepcopy(records)
            read_call = next(
                record
                for record in changed
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "read-a"
                and record["payload"].get("type") == "function_call"
            )
            read_output = next(
                record
                for record in changed
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "read-a"
                and record["payload"].get("type") == "function_call_output"
            )
            changed.remove(read_call)
            changed.remove(read_output)
            exec_index = next(
                index
                for index, record in enumerate(changed)
                if isinstance(record.get("payload"), dict)
                and record["payload"].get("call_id") == "exec"
                and record["payload"].get("type") == "function_call"
            )
            changed[exec_index:exec_index] = [read_call, read_output]
            with self.assertRaisesRegex(HarnessError, "tool_checklist_order"):
                run_analysis(fixture, home, events, changed, row, terminal)

    def test_exact_exec_args_mtime_permissions_and_maximal_retrieval_are_required(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture, home, events, records, row, terminal = synthetic_case(
                Path(directory)
            )
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("call_id") == "exec"
                    and payload.get("type") == "function_call"
                ):
                    arguments = json.loads(payload["arguments"])
                    arguments["yield_time_ms"] = 1
                    payload["arguments"] = json.dumps(arguments)
                    break
            with self.assertRaisesRegex(HarnessError, "exec_command_arguments_invalid"):
                run_analysis(fixture, home, events, changed, row, terminal)

            inline = None
            for record in records:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("call_id") == "read-a"
                    and payload.get("type") == "function_call_output"
                ):
                    inline = json.loads(payload["output"])
                    break
            self.assertIsNotNone(inline)
            inline["fingerprint"]["modified_at_ms"] += 1
            with self.assertRaisesRegex(HarnessError, "file_read_mtime_mismatch"):
                _require_file_read_window(inline, fixture)

            artifact = next((home / "tool_outputs" / THREAD_ID).glob("out_*.txt"))
            artifact.chmod(0o644)
            with self.assertRaisesRegex(
                HarnessError, "artifact_scope_permissions_invalid"
            ):
                run_analysis(fixture, home, events, records, row, terminal)

            artifact.chmod(0o600)
            changed = copy.deepcopy(records)
            for record in changed:
                payload = record.get("payload")
                if (
                    isinstance(payload, dict)
                    and payload.get("call_id") == "s1-bytes"
                    and payload.get("type") == "function_call_output"
                ):
                    value = json.loads(payload["output"])
                    marker = fixture.shell_middle
                    value["end_byte"] = value["start_byte"] + len(marker.encode())
                    value["text"] = marker
                    value["next_offset"] = value["end_byte"]
                    payload["output"] = json.dumps(value, separators=(",", ":"))
                    break
            with self.assertRaisesRegex(HarnessError, "retrieval_window_not_maximal"):
                run_analysis(fixture, home, events, changed, row, terminal)


if __name__ == "__main__":
    unittest.main()
