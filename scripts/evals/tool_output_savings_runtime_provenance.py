"""Stable source and evaluator provenance helpers."""

import hashlib
from pathlib import Path

from tool_output_savings_fixture import sha256_file


def harness_digest(directory: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(directory.glob("*.py")):
        digest.update(path.name.encode())
        digest.update(sha256_file(path).encode())
    return digest.hexdigest()


def stable_worktree_snapshot(
    expected_status_digest: str,
    expected_tracked_clean: bool,
    actual_status_digest: str,
    actual_tracked_clean: bool,
) -> bool:
    return (
        actual_status_digest == expected_status_digest
        and actual_tracked_clean == expected_tracked_clean
    )
