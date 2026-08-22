"""Bounded process-group lifecycle for non-model evaluation commands."""

import os
import signal
import subprocess
import threading
import time
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require


MAX_AUXILIARY_CAPTURE_BYTES = 64 * 1024


@dataclass(frozen=True)
class BoundedCommandOutput:
    returncode: int
    stdout: bytes
    stderr: bytes


def terminate_process(process: subprocess.Popen[bytes]) -> None:
    """Terminate a process group and fail unless death is confirmed."""

    def group_alive() -> bool:
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            return False
        except OSError:
            return True
        return True

    if process.poll() is not None and not group_alive():
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except OSError as error:
        if process.poll() is None:
            raise HarnessError("cli_process_termination_failed") from error
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except OSError as error:
            if process.poll() is None:
                raise HarnessError("cli_process_termination_failed") from error
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired as error:
            raise HarnessError("cli_process_not_terminated") from error
    if group_alive():
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            raise HarnessError("cli_process_termination_failed") from error
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired as error:
            raise HarnessError("cli_process_not_terminated") from error
    deadline = time.monotonic() + 2
    while group_alive() and time.monotonic() < deadline:
        time.sleep(0.05)
    require(
        process.poll() is not None and not group_alive(), "cli_process_not_terminated"
    )


def run_bounded_command(
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
    rule: str,
    environment: Mapping[str, str] | None = None,
) -> None:
    """Run without captured output and always confirm process-group termination."""
    process: subprocess.Popen[bytes] | None = None
    returncode: int | None = None
    pending_error: BaseException | None = None
    finalization_error: BaseException | None = None
    try:
        process = subprocess.Popen(
            args,
            cwd=cwd,
            env=dict(environment) if environment is not None else None,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        returncode = process.wait(timeout=timeout)
    except BaseException as error:
        pending_error = error
    finally:
        if process is not None:
            try:
                terminate_process(process)
            except BaseException as error:
                finalization_error = error
    if pending_error is not None or finalization_error is not None or returncode != 0:
        raise HarnessError(rule) from (finalization_error or pending_error)


def _capture_stream(
    stream: object,
    destination: bytearray,
    overflow: threading.Event,
    errors: list[BaseException],
) -> None:
    try:
        while True:
            chunk = stream.read(16 * 1024)
            if not chunk:
                return
            remaining = MAX_AUXILIARY_CAPTURE_BYTES - len(destination)
            if remaining > 0:
                destination.extend(chunk[:remaining])
            if len(chunk) > remaining:
                overflow.set()
    except BaseException as error:
        errors.append(error)
    finally:
        try:
            stream.close()
        except BaseException as error:
            errors.append(error)


def run_bounded_capture(
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
    rule: str,
    environment: Mapping[str, str] | None = None,
) -> BoundedCommandOutput:
    """Capture a small result while bounding output and the full process group."""
    process: subprocess.Popen[bytes] | None = None
    stdout = bytearray()
    stderr = bytearray()
    overflow = threading.Event()
    errors: list[BaseException] = []
    readers: list[threading.Thread] = []
    returncode: int | None = None
    pending_error: BaseException | None = None
    finalization_error: BaseException | None = None
    try:
        process = subprocess.Popen(
            args,
            cwd=cwd,
            env=dict(environment) if environment is not None else None,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        require(
            process.stdout is not None and process.stderr is not None,
            "bounded_command_pipe_missing",
        )
        readers = [
            threading.Thread(
                target=_capture_stream,
                args=(process.stdout, stdout, overflow, errors),
                daemon=True,
            ),
            threading.Thread(
                target=_capture_stream,
                args=(process.stderr, stderr, overflow, errors),
                daemon=True,
            ),
        ]
        for reader in readers:
            reader.start()
        returncode = process.wait(timeout=timeout)
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
                finalization_error = HarnessError(
                    "bounded_command_reader_not_terminated"
                )
    if (
        pending_error is not None
        or finalization_error is not None
        or errors
        or overflow.is_set()
        or returncode is None
    ):
        raise HarnessError(rule) from (finalization_error or pending_error)
    return BoundedCommandOutput(returncode, bytes(stdout), bytes(stderr))
