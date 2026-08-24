"""macOS-only bounded subprocess capture for the external evaluation."""

import json
import math
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import parse_json_lines
from tool_output_savings_evidence import recover_thread_id
from tool_output_savings_evidence import require
from tool_output_savings_evidence import validate_exec_events
from tool_output_savings_fixture import (
    MAX_CAPTURE_BYTES,
    MAX_TIMEOUT_SECONDS,
    Fixture,
)
from tool_output_savings_subprocess import terminate_process


SAFE_CLI_ENVIRONMENT_VARIABLES = frozenset(
    {
        "ALL_PROXY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_API_KEY",
        "COLORTERM",
        "CURL_CA_BUNDLE",
        "HOME",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LOGNAME",
        "NO_PROXY",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "PATH",
        "REQUESTS_CA_BUNDLE",
        "SHELL",
        "SSL_CERT_DIR",
        "SSL_CERT_FILE",
        "TERM",
        "TMPDIR",
        "USER",
        "all_proxy",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    }
)


class CliRunError(HarnessError):
    """A failed CLI run that still carries any recoverable thread identity."""

    def __init__(
        self,
        rule: str,
        thread_id: str | None,
        *,
        process_cleanup_confirmed: bool,
        process_started: bool = True,
    ) -> None:
        super().__init__(rule)
        self.thread_id = thread_id
        self.process_cleanup_confirmed = process_cleanup_confirmed
        self.process_started = process_started


def _safe_recover_thread_id(raw: bytes) -> str | None:
    try:
        return recover_thread_id(_events_from_bytes(raw))
    except HarnessError:
        return None


def _capture_pipe(
    stream: Any,
    data: bytearray,
    overflow: threading.Event,
    errors: list[BaseException],
) -> None:
    try:
        while True:
            chunk = stream.read(64 * 1024)
            if not chunk:
                return
            remaining = MAX_CAPTURE_BYTES - len(data)
            if remaining > 0:
                data.extend(chunk[:remaining])
            if len(chunk) > remaining:
                overflow.set()
    except BaseException as error:
        errors.append(error)
    finally:
        try:
            stream.close()
        except BaseException as error:
            errors.append(error)


def _write_capture(path: Path, data: bytes) -> None:
    require(
        sys.platform == "darwin"
        and hasattr(os, "O_DIRECTORY")
        and hasattr(os, "O_NOFOLLOW"),
        "diagnostic_platform_unsupported",
    )
    require(
        os.open in getattr(os, "supports_dir_fd", set()),
        "diagnostic_platform_unsupported",
    )
    require(
        os.rename in getattr(os, "supports_dir_fd", set()),
        "diagnostic_platform_unsupported",
    )
    require(
        os.unlink in getattr(os, "supports_dir_fd", set()),
        "diagnostic_platform_unsupported",
    )
    directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    temporary_name = f".{path.name}.tmp-{os.urandom(8).hex()}"
    temporary_fd = -1
    try:
        temporary_fd = os.open(
            temporary_name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o600,
            dir_fd=directory_fd,
        )
        view = memoryview(data)
        while view:
            view = view[os.write(temporary_fd, view) :]
        os.fsync(temporary_fd)
        os.close(temporary_fd)
        temporary_fd = -1
        os.rename(
            temporary_name,
            path.name,
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


def _events_from_bytes(raw: bytes) -> list[dict[str, Any]]:
    try:
        text = raw.decode("utf-8")
    except UnicodeError:
        return []
    try:
        return parse_json_lines(text)
    except HarnessError:
        recovered: list[dict[str, Any]] = []
        for line in text.splitlines():
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                recovered.append(value)
        return recovered


def run_cli(
    cli: Path,
    fixture: Fixture,
    *,
    model: str,
    reasoning_effort: str,
    model_tool_mode_value: str,
    codex_home: Path | None,
    sqlite_home: Path | None,
    timeout: float,
    tool_argument_repair: bool | None = None,
    stage3: bool = False,
) -> tuple[list[dict[str, Any]], str, dict[str, int], Path, Path]:
    if sys.platform != "darwin":
        raise HarnessError("live_eval_scope_macos_only")
    require(
        math.isfinite(timeout) and 0 < timeout <= MAX_TIMEOUT_SECONDS,
        "timeout_invalid",
    )
    from tool_output_savings_fixture import build_cli_command

    stdout_path = fixture.root / "exec.jsonl"
    stderr_path = fixture.root / "exec.stderr"
    command = build_cli_command(
        cli,
        fixture,
        model=model,
        reasoning_effort=reasoning_effort,
        model_tool_mode_value=model_tool_mode_value,
        sqlite_home=sqlite_home,
        tool_argument_repair=tool_argument_repair,
        stage3=stage3,
    )
    environment = {
        key: value
        for key, value in os.environ.items()
        if key in SAFE_CLI_ENVIRONMENT_VARIABLES or key.startswith("LC_")
    }
    if codex_home is not None:
        environment["CODEX_HOME"] = str(codex_home)
    process: subprocess.Popen[bytes] | None = None
    stdout_data = bytearray()
    stderr_data = bytearray()
    overflow = threading.Event()
    readers: list[threading.Thread] = []
    reader_errors: list[BaseException] = []
    returncode: int | None = None
    pending_error: BaseException | None = None
    finalization_error: BaseException | None = None
    try:
        process = subprocess.Popen(
            command,
            cwd=fixture.root,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        require(
            process.stdout is not None and process.stderr is not None,
            "cli_pipe_missing",
        )
        readers = [
            threading.Thread(
                target=_capture_pipe,
                args=(process.stdout, stdout_data, overflow, reader_errors),
                daemon=True,
            ),
            threading.Thread(
                target=_capture_pipe,
                args=(process.stderr, stderr_data, overflow, reader_errors),
                daemon=True,
            ),
        ]
        for reader in readers:
            reader.start()
        deadline = time.monotonic() + timeout
        while process.poll() is None:
            if overflow.is_set():
                raise HarnessError("cli_capture_limit_exceeded")
            if time.monotonic() >= deadline:
                raise HarnessError("cli_timeout")
            time.sleep(0.02)
        returncode = process.returncode
    except BaseException as error:
        pending_error = error
    finally:
        if process is not None:
            try:
                terminate_process(process)
            except BaseException as error:
                finalization_error = error
        for reader in readers:
            reader.join(timeout=2)
            if reader.is_alive() and finalization_error is None:
                finalization_error = HarnessError("cli_capture_reader_not_terminated")
        if reader_errors and finalization_error is None:
            finalization_error = HarnessError("cli_capture_failed")
        try:
            _write_capture(stdout_path, bytes(stdout_data))
            _write_capture(stderr_path, bytes(stderr_data))
        except BaseException as error:
            if finalization_error is None:
                finalization_error = error

    def recovered_id() -> str | None:
        try:
            return recover_thread_id(_events_from_bytes(bytes(stdout_data)))
        except HarnessError:
            return None

    if finalization_error is not None:
        raise CliRunError(
            "cli_process_finalization_failed",
            recovered_id(),
            process_cleanup_confirmed=False,
        ) from finalization_error
    if pending_error is not None:
        rule = (
            str(pending_error)
            if isinstance(pending_error, HarnessError)
            else "cli_failed"
        )
        raise CliRunError(
            rule,
            recovered_id(),
            process_cleanup_confirmed=True,
            process_started=process is not None,
        ) from pending_error
    require(returncode is not None, "cli_returncode_missing")
    if overflow.is_set():
        raise CliRunError(
            "cli_capture_limit_exceeded",
            _safe_recover_thread_id(bytes(stdout_data)),
            process_cleanup_confirmed=True,
        )
    if returncode != 0:
        raise CliRunError(
            "cli_failed",
            _safe_recover_thread_id(bytes(stdout_data)),
            process_cleanup_confirmed=True,
        )
    try:
        events = _read_partial_events(stdout_path, strict=True)
    except (HarnessError, OSError, UnicodeError) as error:
        partial = _read_partial_events(stdout_path)
        rule = (
            str(error) if isinstance(error, HarnessError) else "cli_output_unreadable"
        )
        try:
            partial_thread_id = recover_thread_id(partial)
        except HarnessError:
            partial_thread_id = None
        raise CliRunError(
            rule, partial_thread_id, process_cleanup_confirmed=True
        ) from error
    thread_id = _safe_recover_thread_id(bytes(stdout_data))
    require(thread_id is not None, "thread_id_invalid")
    try:
        validated_thread_id, terminal = validate_exec_events(events)
    except HarnessError as error:
        raise CliRunError(
            str(error), thread_id, process_cleanup_confirmed=True
        ) from error
    require(validated_thread_id == thread_id, "thread_id_invalid")
    return events, thread_id, terminal, stdout_path, stderr_path


def _read_partial_events(path: Path, *, strict: bool = False) -> list[dict[str, Any]]:
    try:
        with path.open("rb") as stream:
            raw = stream.read(MAX_CAPTURE_BYTES + 1)
        if len(raw) > MAX_CAPTURE_BYTES:
            if strict:
                raise HarnessError("cli_capture_limit_exceeded")
            raw = raw[:MAX_CAPTURE_BYTES]
        text = raw.decode("utf-8")
    except (OSError, UnicodeError):
        if strict:
            raise
        return []
    try:
        return parse_json_lines(text)
    except HarnessError:
        if strict:
            raise
        return _events_from_bytes(raw)
