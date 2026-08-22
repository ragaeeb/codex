import json
import os
import sys
import sqlite3
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import ThreadRow
from tool_output_savings_evidence import assert_usage_agreement
from tool_output_savings_evidence import latest_token_count
from tool_output_savings_evidence import measure_thread
from tool_output_savings_evidence import parse_json_lines
from tool_output_savings_evidence import percentage_reduction
from tool_output_savings_evidence import poll_thread_row
from tool_output_savings_evidence import read_thread_row
from tool_output_savings_evidence import validate_exec_events
from tool_output_savings_evidence import recover_thread_id
from tool_output_savings_evidence import selected_rollout
from tool_output_savings_cleanup import resolve_database
from tool_output_savings_artifacts import read_artifact
from tool_output_savings_artifacts import list_thread_artifacts
from tool_output_savings_artifacts import validate_artifact_id
from tool_output_savings_fixture import REPO_ROOT
from tool_output_savings_fixture import model_tool_mode
from tool_output_savings_test_support import THREAD_ID
from tool_output_savings_test_support import make_db
from tool_output_savings_test_support import token_info
from tool_output_savings_transcript import collect_tool_calls
from tool_output_savings_transcript import collect_tool_outputs


def response_item(payload: dict[str, object]) -> dict[str, object]:
    return {"type": "response_item", "payload": payload}


class SavingsEvidenceTests(unittest.TestCase):
    def test_exec_lifecycle_and_usage_reject_invalid_order_or_negative_values(
        self,
    ) -> None:
        usage = {
            "input_tokens": 1,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 1,
            "reasoning_output_tokens": 0,
        }
        started = {"type": "thread.started", "thread_id": THREAD_ID}
        completed = {"type": "turn.completed", "usage": usage}
        with self.assertRaisesRegex(HarnessError, "terminal_event_order"):
            validate_exec_events([completed, started])
        with self.assertRaisesRegex(HarnessError, "terminal_event_not_final"):
            validate_exec_events([started, completed, {"type": "trailing"}])
        negative = dict(usage, output_tokens=-1)
        with self.assertRaisesRegex(HarnessError, "terminal_usage_missing"):
            validate_exec_events(
                [started, {"type": "turn.completed", "usage": negative}]
            )

    def test_repeated_identical_thread_ids_are_recoverable_but_conflicts_fail(
        self,
    ) -> None:
        events = [
            {"type": "thread.started", "thread_id": THREAD_ID},
            {"type": "thread.started", "thread_id": THREAD_ID},
        ]
        self.assertEqual(recover_thread_id(events), THREAD_ID)
        with self.assertRaisesRegex(HarnessError, "thread_id_ambiguous"):
            recover_thread_id(
                events
                + [
                    {
                        "type": "thread.started",
                        "thread_id": "00000000-0000-0000-0000-000000000002",
                    }
                ]
            )

    def test_non_tool_response_items_are_not_mistaken_for_tool_outputs(self) -> None:
        collect_tool_calls(
            [response_item({"type": "reasoning", "summary": "x" * 20_000})]
        )

    @unittest.skipUnless(
        sys.platform == "darwin", "artifact descriptor checks are macOS-only"
    )
    def test_descriptor_enumeration_rejects_unreferenced_or_invalid_entries(
        self,
    ) -> None:
        data = b"artifact body"
        artifact_id = "out_" + __import__("hashlib").sha256(data).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            scope = home / "tool_outputs" / THREAD_ID
            scope.mkdir(parents=True)
            (home / "tool_outputs").chmod(0o700)
            scope.chmod(0o700)
            artifact = scope / f"{artifact_id}.txt"
            artifact.write_bytes(data)
            artifact.chmod(0o600)
            self.assertEqual(
                list_thread_artifacts(home, THREAD_ID), {artifact_id: data}
            )
            (scope / "unexpected.txt").write_bytes(b"unexpected")
            with self.assertRaisesRegex(
                HarnessError, "artifact_scope_unexpected_entry"
            ):
                list_thread_artifacts(home, THREAD_ID)

    @unittest.skipUnless(
        sys.platform == "darwin", "artifact descriptor checks are macOS-only"
    )
    def test_artifact_enumeration_stops_at_the_entry_ceiling(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            scope = home / "tool_outputs" / THREAD_ID
            scope.mkdir(parents=True)
            (home / "tool_outputs").chmod(0o700)
            scope.chmod(0o700)
            for index in range(3):
                data = f"artifact-{index}".encode()
                artifact_id = "out_" + __import__("hashlib").sha256(data).hexdigest()
                artifact = scope / f"{artifact_id}.txt"
                artifact.write_bytes(data)
                artifact.chmod(0o600)
            with (
                patch("tool_output_savings_artifacts.MAX_THREAD_ARTIFACTS", 2),
                patch(
                    "tool_output_savings_artifacts.os.listdir",
                    side_effect=AssertionError("directory entries must stream"),
                ),
                self.assertRaisesRegex(HarnessError, "artifact_scope_over_cap"),
            ):
                list_thread_artifacts(home, THREAD_ID)

    def test_db_selects_target_and_latest_valid_token_count(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "state_5.sqlite"
            rollout_path = root / "target.jsonl"
            rollout_path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "type": "event_msg",
                                "payload": {"type": "token_count", "info": None},
                            }
                        ),
                        json.dumps(
                            {
                                "type": "event_msg",
                                "payload": {
                                    "type": "token_count",
                                    "info": token_info(100),
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            make_db(db, THREAD_ID, str(rollout_path))
            row = read_thread_row(db, THREAD_ID)
            self.assertEqual(row.id, THREAD_ID)
            self.assertEqual(
                latest_token_count(parse_json_lines(rollout_path.read_text())).total[
                    "total_tokens"
                ],
                100,
            )

    def test_measure_thread_retries_a_late_row(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "state_5.sqlite"
            rollout_path = root / "late.jsonl"
            rollout_path.write_text(
                json.dumps({"type": "session_meta", "payload": {"id": THREAD_ID}})
                + "\n"
                + json.dumps(
                    {
                        "type": "event_msg",
                        "payload": {"type": "token_count", "info": token_info(100)},
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            connection = sqlite3.connect(db)
            connection.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, tokens_used INTEGER NOT NULL, model TEXT, reasoning_effort TEXT, cli_version TEXT, updated_at INTEGER, updated_at_ms INTEGER)"
            )
            connection.commit()
            connection.close()

            def insert() -> None:
                time.sleep(1.05)
                connection = sqlite3.connect(db)
                try:
                    connection.execute(
                        "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        (
                            THREAD_ID,
                            str(rollout_path),
                            100,
                            "gpt-5.6-luna",
                            "medium",
                            "1.2.3",
                            1,
                            1,
                        ),
                    )
                    connection.commit()
                finally:
                    connection.close()

            thread = threading.Thread(target=insert)
            thread.start()
            row, _, _, _, usage = measure_thread(db, THREAD_ID, timeout=2.4)
            thread.join()
            self.assertEqual(row.tokens_used, usage.total["total_tokens"])

    def test_read_only_busy_retry_and_state_home_resolution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "state_5.sqlite"
            make_db(db, THREAD_ID, "/missing.jsonl")
            lock = sqlite3.connect(db, check_same_thread=False)
            lock.execute("BEGIN EXCLUSIVE")

            def release() -> None:
                time.sleep(0.08)
                lock.rollback()
                lock.close()

            thread = threading.Thread(target=release)
            thread.start()
            row = poll_thread_row(db, THREAD_ID, timeout=1.0)
            thread.join()
            self.assertEqual(row.id, THREAD_ID)
            sqlite_home = root / "configured"
            sqlite_home.mkdir()
            configured_db = sqlite_home / "state_5.sqlite"
            make_db(configured_db, THREAD_ID, "/missing.jsonl")
            with patch.dict(os.environ, {"CODEX_SQLITE_HOME": str(sqlite_home)}):
                resolved = resolve_database(
                    None, root / "codex", True, cwd=root, ignore_user_config=True
                )
            self.assertEqual(resolved, configured_db.resolve())

            named_sqlite_home = root / "sqlite"
            named_sqlite_home.mkdir()
            named_database = named_sqlite_home / "state_5.sqlite"
            make_db(named_database, THREAD_ID, "/missing.jsonl")
            self.assertEqual(
                resolve_database(
                    None,
                    root / "other-codex-home",
                    True,
                    cwd=root,
                    sqlite_home_override=named_sqlite_home,
                    ignore_user_config=True,
                ),
                named_database.resolve(),
            )

            with self.assertRaisesRegex(HarnessError, "state_db_path_invalid"):
                resolve_database(
                    root / "wrong-name.sqlite",
                    root / "other-codex-home",
                    True,
                    cwd=root,
                    require_existing=False,
                )

    def test_missing_row_and_rollout_are_bounded_failures(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "state_5.sqlite"
            make_db(db, THREAD_ID, str(root / "does-not-exist.jsonl"))
            with self.assertRaisesRegex(HarnessError, "state_db_row_missing"):
                poll_thread_row(
                    db, "00000000-0000-0000-0000-000000000002", timeout=0.05
                )
            with self.assertRaisesRegex(HarnessError, "rollout_missing"):
                measure_thread(db, THREAD_ID, timeout=0.15)

    @unittest.skipUnless(sys.platform == "darwin", "live scope is macOS-only")
    def test_selected_rollout_rejects_ancestor_and_final_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            real = root / "real"
            real.mkdir()
            rollout = real / "thread.jsonl"
            rollout.write_text(
                json.dumps({"type": "session_meta", "payload": {"id": THREAD_ID}})
                + "\n",
                encoding="utf-8",
            )
            ancestor_link = root / "linked"
            ancestor_link.symlink_to(real)
            row = ThreadRow(
                THREAD_ID,
                str(ancestor_link / rollout.name),
                1,
                "gpt-5.5",
                "medium",
                "0.0.0",
                1,
                1,
            )
            with self.assertRaisesRegex(HarnessError, "rollout_scope_invalid"):
                selected_rollout(row, root / "state_5.sqlite", timeout=0.01)

            final_link = root / "thread.jsonl"
            final_link.symlink_to(rollout)
            row = ThreadRow(
                THREAD_ID,
                str(final_link),
                1,
                "gpt-5.5",
                "medium",
                "0.0.0",
                1,
                1,
            )
            with self.assertRaisesRegex(HarnessError, "rollout_scope_invalid"):
                selected_rollout(row, root / "state_5.sqlite", timeout=0.01)

    def test_malformed_token_count_after_valid_fails_closed(self) -> None:
        records = [
            {
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": token_info(100),
                },
            },
            {
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": "bad"},
                },
            },
            {"type": "event_msg", "payload": {"type": "token_count", "info": None}},
        ]
        with self.assertRaisesRegex(HarnessError, "rollout_token_count_invalid"):
            latest_token_count(records)

    def test_token_counts_require_consistent_subsets_and_monotonic_totals(self) -> None:
        invalid_cached = token_info(100)
        invalid_cached["total_token_usage"]["cached_input_tokens"] = 61
        with self.assertRaisesRegex(HarnessError, "rollout_token_count_invalid"):
            latest_token_count(
                [
                    {
                        "type": "event_msg",
                        "payload": {"type": "token_count", "info": invalid_cached},
                    }
                ]
            )

        invalid_last = token_info(100)
        invalid_last["last_token_usage"]["input_tokens"] = 61
        invalid_last["last_token_usage"]["total_tokens"] = 62
        with self.assertRaisesRegex(HarnessError, "rollout_last_token_usage_invalid"):
            latest_token_count(
                [
                    {
                        "type": "event_msg",
                        "payload": {"type": "token_count", "info": invalid_last},
                    }
                ]
            )

        later = token_info(100)
        earlier = token_info(110)
        with self.assertRaisesRegex(HarnessError, "rollout_token_count_non_monotonic"):
            latest_token_count(
                [
                    {
                        "type": "event_msg",
                        "payload": {"type": "token_count", "info": earlier},
                    },
                    {
                        "type": "event_msg",
                        "payload": {"type": "token_count", "info": later},
                    },
                ]
            )

    def test_usage_mismatch_fails_closed(self) -> None:
        evidence = latest_token_count(
            [
                {
                    "type": "event_msg",
                    "payload": {"type": "token_count", "info": token_info(100)},
                }
            ]
        )
        row = ThreadRow(
            THREAD_ID, "/rollout", 100, "gpt-5.6-luna", "medium", "1.2.3", 1, 1
        )
        terminal = {
            field: evidence.total[field]
            for field in evidence.total
            if field != "total_tokens"
        }
        assert_usage_agreement(
            row,
            evidence,
            terminal,
            model="gpt-5.6-luna",
            reasoning_effort="medium",
            cli_version="1.2.3",
        )
        terminal["output_tokens"] += 1
        with self.assertRaisesRegex(
            HarnessError, "terminal_rollout_output_tokens_mismatch"
        ):
            assert_usage_agreement(
                row,
                evidence,
                terminal,
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                cli_version="1.2.3",
            )
        with self.assertRaisesRegex(HarnessError, "db_rollout_total_mismatch"):
            assert_usage_agreement(
                ThreadRow(
                    THREAD_ID,
                    "/rollout",
                    101,
                    "gpt-5.6-luna",
                    "medium",
                    "1.2.3",
                    1,
                    1,
                ),
                evidence,
                {
                    field: evidence.total[field]
                    for field in evidence.total
                    if field != "total_tokens"
                },
                model="gpt-5.6-luna",
                reasoning_effort="medium",
                cli_version="1.2.3",
            )

    def test_collectors_reject_malformed_duplicate_orphan_and_oversized_outputs(
        self,
    ) -> None:
        with self.assertRaisesRegex(HarnessError, "malformed_tool_call_arguments"):
            collect_tool_calls(
                [
                    response_item(
                        {
                            "type": "function_call",
                            "name": "read_file",
                            "call_id": "bad",
                            "arguments": "not-json",
                        }
                    )
                ]
            )
        with self.assertRaisesRegex(HarnessError, "tool_response_item_over_cap"):
            collect_tool_calls(
                [
                    response_item(
                        {
                            "type": "message",
                            "role": "tool",
                            "content": "x" * 10_000,
                        }
                    )
                ]
            )
        call = response_item(
            {
                "type": "function_call",
                "name": "read_file",
                "call_id": "call",
                "arguments": "{}",
            }
        )
        with self.assertRaisesRegex(HarnessError, "orphan_tool_output"):
            collect_tool_outputs(
                [
                    call,
                    response_item(
                        {
                            "type": "function_call_output",
                            "call_id": "orphan",
                            "output": "small",
                        }
                    ),
                ]
            )
        with self.assertRaisesRegex(HarnessError, "function_output_over_cap"):
            collect_tool_outputs(
                [
                    call,
                    response_item(
                        {
                            "type": "function_call_output",
                            "call_id": "orphan",
                            "output": "x" * (8 * 1024 + 1),
                        }
                    ),
                ]
            )
        with self.assertRaisesRegex(HarnessError, "duplicate_tool_output_id"):
            collect_tool_outputs(
                [
                    call,
                    response_item(
                        {
                            "type": "function_call_output",
                            "call_id": "call",
                            "output": "first",
                        }
                    ),
                    response_item(
                        {
                            "type": "function_call_output",
                            "call_id": "call",
                            "output": "second",
                        }
                    ),
                ]
            )

    @unittest.skipUnless(
        sys.platform == "darwin", "artifact descriptor checks are macOS-only"
    )
    def test_artifact_scope_rejects_ancestor_and_final_symlinks(self) -> None:
        data = b"artifact body"
        artifact_id = "out_" + __import__("hashlib").sha256(data).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            tool_outputs = home / "tool_outputs"
            scope = tool_outputs / THREAD_ID
            scope.mkdir(parents=True)
            tool_outputs.chmod(0o700)
            scope.chmod(0o700)
            artifact = scope / f"{artifact_id}.txt"
            artifact.write_bytes(data)
            artifact.chmod(0o600)
            self.assertEqual(read_artifact(home, THREAD_ID, artifact_id), data)
            scope_file = home / "outside"
            scope_file.write_bytes(data)
            final = scope / f"{artifact_id}.txt"
            final.unlink()
            final.symlink_to(scope_file)
            with self.assertRaisesRegex(HarnessError, "artifact_scope_invalid"):
                read_artifact(home, THREAD_ID, artifact_id)

            real_parent = home / "real-parent"
            real_home = real_parent / "nested-home"
            real_scope = real_home / "tool_outputs" / THREAD_ID
            real_scope.mkdir(parents=True)
            (real_home / "tool_outputs").chmod(0o700)
            real_scope.chmod(0o700)
            real_artifact = real_scope / f"{artifact_id}.txt"
            real_artifact.write_bytes(data)
            real_artifact.chmod(0o600)
            linked_parent = home / "linked-parent"
            linked_parent.symlink_to(real_parent)
            with self.assertRaisesRegex(HarnessError, "artifact_scope_invalid"):
                read_artifact(linked_parent / "nested-home", THREAD_ID, artifact_id)

            final.unlink()
            final.write_bytes(data)
            final.chmod(0o600)
            replacement = home / "real-tool-outputs"
            replacement.mkdir()
            (replacement / THREAD_ID).mkdir()
            (replacement / THREAD_ID / f"{artifact_id}.txt").write_bytes(data)
            tool_outputs.rename(home / "tool-outputs-real")
            (home / "tool_outputs").symlink_to(home / "tool-outputs-real")
            with self.assertRaisesRegex(HarnessError, "artifact_scope_invalid"):
                read_artifact(home, THREAD_ID, artifact_id)

    def test_catalog_null_tool_mode_is_direct_and_unknown_is_rejected(self) -> None:
        self.assertEqual(
            model_tool_mode(
                REPO_ROOT / "codex-rs/models-manager/models.json", "gpt-5.5"
            ),
            "direct",
        )
        self.assertEqual(
            model_tool_mode(
                REPO_ROOT / "codex-rs/models-manager/models.json", "gpt-5.6-luna"
            ),
            "code_mode_only",
        )
        with tempfile.TemporaryDirectory() as directory:
            catalog = Path(directory) / "models.json"
            catalog.write_text(
                json.dumps({"models": [{"slug": "x", "tool_mode": "future"}]})
            )
            with self.assertRaisesRegex(HarnessError, "model_tool_mode_unknown"):
                model_tool_mode(catalog, "x")

    def test_report_helpers_are_content_free_and_zero_safe(self) -> None:
        self.assertEqual(percentage_reduction(0, 0), 0.0)
        with self.assertRaisesRegex(HarnessError, "invalid_artifact_id"):
            validate_artifact_id("../out_" + "a" * 64)

    def test_unexpected_tool_like_response_item_is_rejected_before_filtering(
        self,
    ) -> None:
        with self.assertRaisesRegex(HarnessError, "unexpected_tool_like_response_item"):
            collect_tool_calls(
                [
                    response_item(
                        {
                            "type": "local_shell_call",
                            "call_id": "shell",
                            "command": "echo unexpected",
                        }
                    )
                ]
            )


if __name__ == "__main__":
    unittest.main()
