"""Symlink-safe managed-artifact validation and exact-byte access."""

import hashlib
import errno
import os
import re
import stat
import sys
import uuid
from pathlib import Path
from typing import Any

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require


ARTIFACT_ID = re.compile(r"^out_[0-9a-f]{64}$")
MAX_ARTIFACT_READ_BYTES = 2 * 1024 * 1024
MAX_THREAD_ARTIFACTS = 4_096
MAX_THREAD_ARTIFACT_BYTES = 64 * 1024 * 1024


def validate_artifact_id(value: Any) -> str:
    require(
        isinstance(value, str) and ARTIFACT_ID.fullmatch(value) is not None,
        "invalid_artifact_id",
    )
    return value


def _require_descriptor_capabilities() -> int:
    require(
        sys.platform == "darwin"
        and hasattr(os, "O_DIRECTORY")
        and hasattr(os, "O_NOFOLLOW")
        and os.open in getattr(os, "supports_dir_fd", set()),
        # scandir(fd) is the enumeration primitive that keeps the managed
        # scope descriptor-relative after it has been opened.
        "artifact_scope_platform_unsupported",
    )
    require(
        os.scandir in getattr(os, "supports_fd", set()),
        "artifact_scope_platform_unsupported",
    )
    return os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW


def _open_no_follow_path(path: Path, flags: int) -> int:
    """Open every component of an absolute path without following symlinks."""
    expanded = Path(os.path.abspath(os.path.expanduser(str(path))))
    require(expanded.is_absolute(), "artifact_scope_invalid")
    # macOS exposes /var as a system compatibility symlink. Rewrite that one
    # documented system alias without resolving any user-controlled ancestor.
    if len(expanded.parts) >= 2 and expanded.parts[1] == "var":
        expanded = Path("/private/var").joinpath(*expanded.parts[2:])
    descriptor = os.open(os.sep, flags)
    try:
        for component in expanded.parts[1:]:
            next_descriptor = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = next_descriptor
        return descriptor
    except OSError:
        os.close(descriptor)
        raise


def _open_scope(codex_home: Path, thread_id: str) -> tuple[int, int, int]:
    try:
        uuid.UUID(thread_id)
    except ValueError as error:
        raise HarnessError("thread_id_invalid") from error
    flags = _require_descriptor_capabilities()
    try:
        root_fd = _open_no_follow_path(codex_home, flags)
    except OSError as error:
        raise HarnessError("artifact_scope_invalid") from error
    tool_outputs_fd = -1
    thread_fd = -1
    try:
        tool_outputs_fd = os.open("tool_outputs", flags, dir_fd=root_fd)
        thread_fd = os.open(thread_id, flags, dir_fd=tool_outputs_fd)
        require(
            all(
                stat.S_IMODE(os.fstat(descriptor).st_mode) == 0o700
                for descriptor in (tool_outputs_fd, thread_fd)
            ),
            "artifact_scope_permissions_invalid",
        )
    except (HarnessError, OSError):
        for descriptor in (thread_fd, tool_outputs_fd, root_fd):
            if descriptor >= 0:
                os.close(descriptor)
        raise
    return root_fd, tool_outputs_fd, thread_fd


def _read_descriptor(descriptor: int) -> bytes:
    metadata = os.fstat(descriptor)
    require(stat.S_ISREG(metadata.st_mode), "artifact_scope_invalid")
    require(
        stat.S_IMODE(metadata.st_mode) == 0o600,
        "artifact_scope_permissions_invalid",
    )
    require(metadata.st_size <= MAX_ARTIFACT_READ_BYTES, "artifact_over_cap")
    data = bytearray()
    while len(data) <= MAX_ARTIFACT_READ_BYTES:
        chunk = os.read(
            descriptor, min(64 * 1024, MAX_ARTIFACT_READ_BYTES + 1 - len(data))
        )
        if not chunk:
            break
        data.extend(chunk)
    require(len(data) <= MAX_ARTIFACT_READ_BYTES, "artifact_over_cap")
    return bytes(data)


def read_artifact(codex_home: Path, thread_id: str, artifact_id: str) -> bytes:
    try:
        validate_artifact_id(artifact_id)
        uuid.UUID(thread_id)
        nofollow = _require_descriptor_capabilities() & os.O_NOFOLLOW
        root_fd, tool_outputs_fd, thread_fd = _open_scope(codex_home, thread_id)
        artifact_fd = -1
        try:
            artifact_name = f"{artifact_id}.txt"
            artifact_fd = os.open(
                artifact_name, os.O_RDONLY | nofollow, dir_fd=thread_fd
            )
            data = _read_descriptor(artifact_fd)
        finally:
            for descriptor in (artifact_fd, thread_fd, tool_outputs_fd, root_fd):
                if descriptor >= 0:
                    os.close(descriptor)
    except HarnessError:
        raise
    except OSError as error:
        if error.errno in {errno.ELOOP, errno.ENOTDIR}:
            raise HarnessError("artifact_scope_invalid") from error
        raise HarnessError("artifact_missing") from error
    except UnicodeError as error:
        raise HarnessError("artifact_missing") from error
    require(
        hashlib.sha256(data).hexdigest() == artifact_id[4:], "artifact_digest_mismatch"
    )
    return data


def list_thread_artifacts(codex_home: Path, thread_id: str) -> dict[str, bytes]:
    """Enumerate exactly the managed artifact files for one thread."""
    root_fd, tool_outputs_fd, thread_fd = _open_scope(codex_home, thread_id)
    artifacts: dict[str, bytes] = {}
    try:
        total_bytes = 0
        entry_count = 0
        with os.scandir(thread_fd) as entries:
            for entry in entries:
                entry_count += 1
                require(
                    entry_count <= MAX_THREAD_ARTIFACTS,
                    "artifact_scope_over_cap",
                )
                name = entry.name
                if name == ".last_access":
                    descriptor = os.open(
                        name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=thread_fd
                    )
                    try:
                        _read_descriptor(descriptor)
                    finally:
                        os.close(descriptor)
                    continue
                match = re.fullmatch(r"(out_[0-9a-f]{64})\.txt", name)
                require(match is not None, "artifact_scope_unexpected_entry")
                artifact_id = validate_artifact_id(match.group(1))
                descriptor = os.open(
                    name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=thread_fd
                )
                try:
                    data = _read_descriptor(descriptor)
                finally:
                    os.close(descriptor)
                total_bytes += len(data)
                require(
                    total_bytes <= MAX_THREAD_ARTIFACT_BYTES,
                    "artifact_scope_over_cap",
                )
                require(
                    hashlib.sha256(data).hexdigest() == artifact_id[4:],
                    "artifact_digest_mismatch",
                )
                artifacts[artifact_id] = data
    except OSError as error:
        if error.errno in {errno.ELOOP, errno.ENOTDIR}:
            raise HarnessError("artifact_scope_invalid") from error
        raise HarnessError("artifact_scope_enumeration_failed") from error
    finally:
        for descriptor in (thread_fd, tool_outputs_fd, root_fd):
            os.close(descriptor)
    return artifacts


def artifact_ids_in(value: Any) -> set[str]:
    if isinstance(value, dict):
        found = (
            {validate_artifact_id(value.get("artifact_id"))}
            if value.get("type")
            in {
                "tool_output_artifact",
                "tool_output_artifact_window",
                "tool_output_artifact_search",
            }
            else set()
        )
        for child in value.values():
            found.update(artifact_ids_in(child))
        return found
    if isinstance(value, list):
        return set().union(*(artifact_ids_in(child) for child in value))
    return set()
