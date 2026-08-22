"""Exact generated-thread cleanup with the measured state database pinned."""

import json
import os
import re
import time
from dataclasses import dataclass
from pathlib import Path

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import read_thread_row
from tool_output_savings_evidence import require
from tool_output_savings_subprocess import run_bounded_command


def resolve_database(
    explicit: Path | None,
    codex_home: Path,
    home_overridden: bool,
    *,
    cwd: Path,
    sqlite_home_override: Path | None = None,
    ignore_user_config: bool = True,
    require_existing: bool = True,
) -> Path:
    """Resolve the active state DB without scanning unrelated rows or paths."""
    configured_environment = _configured_sqlite_home(
        codex_home, cwd, ignore_user_config=ignore_user_config
    )
    if sqlite_home_override is not None and configured_environment is not None:
        require(
            sqlite_home_override.expanduser().resolve() == configured_environment,
            "state_db_configuration_conflict",
        )
    if explicit is not None:
        path = explicit.expanduser().resolve()
        require(path.name == "state_5.sqlite", "state_db_path_invalid")
        configured_home = sqlite_home_override or configured_environment
        if configured_home is not None:
            require(
                path.parent == configured_home.expanduser().resolve(),
                "state_db_configuration_conflict",
            )
        if require_existing:
            require(path.is_file(), "state_db_missing")
        return path
    state_home = (
        sqlite_home_override.expanduser().resolve()
        if sqlite_home_override
        else configured_environment
    )
    if state_home is not None:
        candidates = [state_home / "state_5.sqlite"]
    else:
        candidates = [
            codex_home / "state_5.sqlite",
            codex_home / "sqlite" / "state_5.sqlite",
        ]
    if state_home is None and not home_overridden:
        default_home = (Path.home() / ".codex").resolve()
        candidates.extend(
            [
                default_home / "state_5.sqlite",
                default_home / "sqlite" / "state_5.sqlite",
            ]
        )
    for candidate in dict.fromkeys(path.resolve() for path in candidates):
        require(candidate.name == "state_5.sqlite", "state_db_path_invalid")
        if candidate.is_file() or not require_existing:
            return candidate
    raise HarnessError("state_db_missing")


def _configured_sqlite_home(
    codex_home: Path, cwd: Path, *, ignore_user_config: bool
) -> Path | None:
    environment_home = os.environ.get("CODEX_SQLITE_HOME")
    if environment_home and environment_home.strip():
        path = Path(environment_home).expanduser()
        return (path if path.is_absolute() else cwd / path).resolve()
    if ignore_user_config:
        return None
    try:
        config_text = (codex_home / "config.toml").read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return None
    try:
        import tomllib
    except ModuleNotFoundError:
        match = re.search(r"(?m)^\s*sqlite_home\s*=\s*([\"'])(.*?)\1\s*$", config_text)
        configured = match.group(2) if match else None
    else:
        try:
            configured = tomllib.loads(config_text).get("sqlite_home")
        except tomllib.TOMLDecodeError:
            return None
    if not isinstance(configured, str) or not configured.strip():
        return None
    path = Path(configured).expanduser()
    return (path if path.is_absolute() else cwd / path).resolve()


REPO_ROOT = Path(__file__).resolve().parents[2]
SAFE_CLEANUP_ENVIRONMENT_VARIABLES = frozenset({"LANG", "LC_ALL", "LC_CTYPE", "TMPDIR"})


@dataclass(frozen=True)
class ThreadDeletionTargets:
    """Exact persisted resources that must disappear after thread deletion."""

    database: Path
    thread_id: str
    rollout_path: Path | None
    artifact_directory: Path


def _rollout_path_without_following(raw: str | Path, database: Path) -> Path:
    path = Path(raw).expanduser()
    if not path.is_absolute():
        path = database.parent / path
    return Path(os.path.abspath(path))


def delete_thread(
    cli: Path,
    thread_id: str,
    codex_home: Path,
    sqlite_home: Path | None,
    timeout: float,
    *,
    database: Path,
    rollout_path: Path | None = None,
) -> ThreadDeletionTargets:
    command = [str(cli)]
    # SqliteConfig stores state_5.sqlite directly under sqlite_home.  Keep the
    # measured database's parent as the fallback; a legitimate sqlite_home
    # directory may itself be named "sqlite".
    effective_sqlite_home = sqlite_home or database.parent
    effective_sqlite_home = effective_sqlite_home.expanduser().resolve()
    require(
        database.expanduser().resolve().parent == effective_sqlite_home,
        "state_db_configuration_conflict",
    )
    database = database.expanduser().resolve()
    row = read_thread_row(database, thread_id)
    row_rollout_path = (
        _rollout_path_without_following(row.rollout_path, database)
        if row is not None
        else None
    )
    measured_rollout_path = (
        _rollout_path_without_following(rollout_path, database)
        if rollout_path is not None
        else None
    )
    if row_rollout_path is not None and measured_rollout_path is not None:
        require(
            row_rollout_path == measured_rollout_path,
            "thread_cleanup_rollout_mismatch",
        )
    selected_rollout_path = measured_rollout_path or row_rollout_path
    targets = ThreadDeletionTargets(
        database=database,
        thread_id=thread_id,
        rollout_path=selected_rollout_path,
        artifact_directory=Path(
            os.path.abspath(codex_home.expanduser() / "tool_outputs" / thread_id)
        ),
    )
    command.extend(["-c", f"sqlite_home={json.dumps(str(effective_sqlite_home))}"])
    command.extend(["delete", thread_id, "--force"])
    environment = {
        key: value
        for key, value in os.environ.items()
        if key in SAFE_CLEANUP_ENVIRONMENT_VARIABLES or key.startswith("LC_")
    }
    environment["CODEX_HOME"] = str(codex_home)
    run_bounded_command(
        command,
        cwd=REPO_ROOT,
        environment=environment,
        timeout=min(timeout, 30.0),
        rule="thread_cleanup_failed",
    )
    return targets


def verify_thread_deleted(targets: ThreadDeletionTargets, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            row_absent = read_thread_row(targets.database, targets.thread_id) is None
            rollout_absent = targets.rollout_path is None or not os.path.lexists(
                targets.rollout_path
            )
            artifact_absent = not os.path.lexists(targets.artifact_directory)
            if row_absent and rollout_absent and artifact_absent:
                return
        except HarnessError as error:
            if str(error) != "state_db_busy":
                raise
        time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))
    raise HarnessError("thread_cleanup_not_verified")
