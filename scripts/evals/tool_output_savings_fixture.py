"""Generated fixtures and command/provenance helpers for the savings harness."""

import hashlib
import json
import os
import re
import secrets
import shlex
import sys
from dataclasses import dataclass
from pathlib import Path

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_subprocess import run_bounded_command
from tool_output_savings_subprocess import run_bounded_capture


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_MODEL = "gpt-5.6-luna"
DEFAULT_REASONING_EFFORT = "medium"
EXEC_MAX_OUTPUT_TOKENS = 1_000
MAX_CAPTURE_BYTES = 8 * 1024 * 1024
MAX_TIMEOUT_SECONDS = 900.0
POSIX_ONLY_SCOPE = "live evaluation is macOS-only; it makes no cross-platform E2E claim"
VERSION = re.compile(r"(?<!\d)(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)")


@dataclass(frozen=True)
class Fixture:
    root: Path
    emitter_name: str
    window_name: str
    emitter_output_bytes: bytes
    window_bytes: bytes
    shell_head: str
    shell_middle: str
    shell_tail: str
    window_middle: str
    window_middle_offset: int
    window_modified_at_ms: int
    python_executable: str


def build_fixture(root: Path, *, python_executable: str | None = None) -> Fixture:
    suffix = secrets.token_hex(8)
    shell_head = f"SAVINGS_SHELL_HEAD_{suffix}"
    shell_middle = f"SAVINGS_SHELL_MIDDLE_{suffix}"
    shell_tail = f"SAVINGS_SHELL_TAIL_{suffix}"
    window_middle = f"SAVINGS_FILE_MIDDLE_{suffix}"
    emitter_name = "emit_large.py"
    window_name = "window_utf8.txt"
    emitter_lines = [
        "import sys",
        f"head = {shell_head!r}",
        f"middle = {shell_middle!r}",
        f"tail = {shell_tail!r}",
        "for index in range(7000):",
        "    if index == 0:",
        "        line = head",
        "    elif index == 3500:",
        "        line = middle",
        "    elif index == 6999:",
        "        line = tail",
        "    else:",
        "        line = f'emitter-line-{index:05d}-' + 'x' * 52",
        "    sys.stdout.write(line + '\\n')",
    ]
    (root / emitter_name).write_text("\n".join(emitter_lines) + "\n", encoding="utf-8")
    emitted_lines = []
    for index in range(7000):
        if index == 0:
            line = shell_head
        elif index == 3500:
            line = shell_middle
        elif index == 6999:
            line = shell_tail
        else:
            line = f"emitter-line-{index:05d}-" + "x" * 52
        emitted_lines.append(line)
    emitter_output_bytes = ("\n".join(emitted_lines) + "\n").encode()
    lines: list[str] = []
    size = 0
    marker_offset = -1
    index = 0
    while size < 96 * 1024:
        if marker_offset < 0 and size >= 4 * 1024:
            line = f"{window_middle} middle payload café 東京\n"
            marker_offset = size
        else:
            line = f"line-{index:05d} payload café 東京 {'z' * 74}\n"
        lines.append(line)
        size += len(line.encode())
        index += 1
    window_bytes = "".join(lines).encode()
    require(3_500 <= marker_offset <= 5_000, "fixture_marker_offset")
    (root / window_name).write_bytes(window_bytes)
    for path in (root / emitter_name, root / window_name):
        path.chmod(0o600)
    window_modified_at_ms = (root / window_name).stat().st_mtime_ns // 1_000_000
    return Fixture(
        root=root,
        emitter_name=emitter_name,
        window_name=window_name,
        emitter_output_bytes=emitter_output_bytes,
        window_bytes=window_bytes,
        shell_head=shell_head,
        shell_middle=shell_middle,
        shell_tail=shell_tail,
        window_middle=window_middle,
        window_middle_offset=marker_offset,
        window_modified_at_ms=window_modified_at_ms,
        python_executable=python_executable or sys.executable,
    )


def run_silent(
    args: list[str],
    cwd: Path,
    *,
    environment: dict[str, str],
    timeout: float = 30.0,
) -> None:
    run_bounded_command(
        args,
        cwd=cwd,
        timeout=timeout,
        rule="fixture_command_failed",
        environment=environment,
    )


def initialize_git_fixture(fixture: Fixture) -> None:
    environment = {
        key: value
        for key, value in os.environ.items()
        if key in {"LANG", "LC_ALL", "LC_CTYPE", "PATH", "TMPDIR"}
        or key.startswith("LC_")
    }
    environment.update(
        {
            "GCM_INTERACTIVE": "never",
            "GIT_ATTR_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    git = [
        "git",
        "-c",
        f"core.hooksPath={os.devnull}",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "tag.gpgsign=false",
    ]
    run_silent(
        [*git, "-c", "init.templateDir=", "init", "-q"],
        fixture.root,
        environment=environment,
    )
    run_silent(
        [
            *git,
            "-c",
            "user.name=Codex E2E",
            "-c",
            "user.email=e2e@example.invalid",
            "add",
            ".",
        ],
        fixture.root,
        environment=environment,
    )
    run_silent(
        [
            *git,
            "-c",
            "user.name=Codex E2E",
            "-c",
            "user.email=e2e@example.invalid",
            "commit",
            "--no-verify",
            "--no-gpg-sign",
            "-qm",
            "fixture",
        ],
        fixture.root,
        environment=environment,
    )


def model_tool_mode(catalog: Path, model: str) -> str:
    try:
        payload = json.loads(catalog.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise HarnessError("model_catalog_unreadable") from error
    for item in payload.get("models", []):
        if isinstance(item, dict) and item.get("slug") == model:
            mode = item.get("tool_mode")
            if mode is None:
                return "direct"
            require(mode in {"direct", "code_mode_only"}, "model_tool_mode_unknown")
            return mode
    raise HarnessError("model_not_in_catalog")


def model_catalog_entry_digest(catalog: Path, model: str) -> str:
    try:
        payload = json.loads(catalog.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise HarnessError("model_catalog_unreadable") from error
    entries = [
        item
        for item in payload.get("models", [])
        if isinstance(item, dict) and item.get("slug") == model
    ]
    require(len(entries) == 1, "model_catalog_entry_missing_or_ambiguous")
    encoded = json.dumps(
        entries[0], ensure_ascii=False, sort_keys=True, separators=(",", ":")
    )
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def validate_reasoning_effort(catalog: Path, model: str, reasoning_effort: str) -> None:
    try:
        payload = json.loads(catalog.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise HarnessError("model_catalog_unreadable") from error
    entries = [
        item
        for item in payload.get("models", [])
        if isinstance(item, dict) and item.get("slug") == model
    ]
    require(len(entries) == 1, "model_catalog_entry_missing_or_ambiguous")
    levels = entries[0].get("supported_reasoning_levels")
    supported = (
        {
            level.get("effort")
            for level in levels
            if isinstance(level, dict) and isinstance(level.get("effort"), str)
        }
        if isinstance(levels, list)
        else set()
    )
    require(reasoning_effort in supported, "reasoning_effort_unsupported")


def _target_roots() -> list[Path]:
    configured = os.environ.get("CARGO_TARGET_DIR")
    if configured:
        root = Path(configured).expanduser()
        if not root.is_absolute():
            root = REPO_ROOT / "codex-rs" / root
    else:
        root = REPO_ROOT / "codex-rs" / "target"
    roots = [root.resolve()]
    target = os.environ.get("CARGO_BUILD_TARGET")
    if target:
        roots.insert(0, (root / target).resolve())
    return roots


def resolve_built_cli() -> Path:
    candidates = [
        root / "release" / name
        for root in _target_roots()
        for name in ("codex", "codex.exe")
    ]
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve()
    raise HarnessError("built_cli_missing")


def is_repo_target_cli(path: Path) -> bool:
    resolved = path.expanduser().resolve()
    return any(
        resolved == root / "release" / name
        for root in _target_roots()
        for name in ("codex", "codex.exe")
    )


def resolve_cli(value: Path | None) -> Path:
    path = value.expanduser().resolve() if value is not None else resolve_built_cli()
    require(
        path.is_file() and os.access(path, os.X_OK), "cli_missing_or_not_executable"
    )
    return path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise HarnessError("cli_digest_failed") from error
    return digest.hexdigest()


def cli_version(cli: Path, timeout: float) -> str:
    try:
        result = run_bounded_capture(
            [str(cli), "--version"],
            cwd=REPO_ROOT,
            timeout=timeout,
            rule="cli_version_failed",
            environment=_minimal_command_environment(),
        )
        stdout = result.stdout.decode("utf-8")
    except (HarnessError, OSError, UnicodeError) as error:
        raise HarnessError("cli_version_failed") from error
    require(result.returncode == 0, "cli_version_failed")
    match = VERSION.search(stdout)
    require(match is not None, "cli_version_unparseable")
    return match.group(1)


def _minimal_command_environment() -> dict[str, str]:
    return {
        key: value
        for key, value in os.environ.items()
        if key in {"LANG", "LC_ALL", "LC_CTYPE", "TMPDIR"} or key.startswith("LC_")
    }


def _git_output(arguments: list[str]) -> str:
    environment = _minimal_command_environment()
    environment.update(
        {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    try:
        result = run_bounded_capture(
            [
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                *arguments,
            ],
            cwd=REPO_ROOT,
            timeout=10,
            rule="repository_probe_failed",
            environment=environment,
        )
        require(result.returncode == 0, "repository_probe_failed")
        return result.stdout.decode("utf-8").strip()
    except (HarnessError, OSError, UnicodeError) as error:
        raise HarnessError("repository_probe_failed") from error


def git_commit() -> str:
    value = _git_output(["rev-parse", "HEAD"])
    require(
        re.fullmatch(r"[0-9a-f]{40}", value) is not None,
        "repository_commit_unavailable",
    )
    return value


def working_tree_dirty() -> bool:
    try:
        output = _git_output(["status", "--porcelain"])
    except HarnessError as error:
        raise HarnessError("repository_status_unavailable") from error
    return bool(output)


def working_tree_status_digest() -> str:
    try:
        output = _git_output(["status", "--porcelain=v1"])
    except HarnessError as error:
        raise HarnessError("repository_status_unavailable") from error
    return hashlib.sha256(output.encode("utf-8")).hexdigest()


def tracked_working_tree_dirty() -> bool:
    try:
        output = _git_output(["status", "--porcelain", "--untracked-files=no"])
    except HarnessError as error:
        raise HarnessError("repository_status_unavailable") from error
    return bool(output)


def build_release() -> Path:
    run_bounded_command(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "-p",
            "codex-cli",
            "--bin",
            "codex",
        ],
        cwd=REPO_ROOT / "codex-rs",
        timeout=900,
        rule="release_build_failed",
    )
    return resolve_built_cli()


def build_cli_command(
    cli: Path,
    fixture: Fixture,
    *,
    model: str,
    reasoning_effort: str,
    model_tool_mode_value: str,
    sqlite_home: Path | None = None,
) -> list[str]:
    command = [
        str(cli),
        "exec",
        "--json",
        "--ignore-user-config",
        "--ignore-rules",
        "--enable",
        "native_read_file",
    ]
    if model_tool_mode_value == "code_mode_only":
        command.extend(["--enable", "code_mode_host"])
        from tool_output_savings_code_mode_lane import code_mode_prompt

        selected_prompt = code_mode_prompt(fixture)
    else:
        command.extend(["--disable", "code_mode", "--disable", "code_mode_host"])
        selected_prompt = prompt(fixture)
    command.extend(
        [
            "-m",
            model,
            "-c",
            f"model_reasoning_effort={json.dumps(reasoning_effort)}",
            "-c",
            "tool_output_token_limit=10000",
            "-c",
            'shell_environment_policy.inherit="none"',
        ]
    )
    if sqlite_home is not None:
        command.extend(["-c", f"sqlite_home={json.dumps(str(sqlite_home))}"])
    command.extend(
        ["--sandbox", "workspace-write", "--cd", str(fixture.root), selected_prompt]
    )
    return command


def prompt(fixture: Fixture) -> str:
    command = shlex.join([fixture.python_executable, fixture.emitter_name])
    return (
        "Perform exactly this single-turn checklist. Do not use shell, cat, sed, head, or tail to read either fixture file. Do not call any other tools.\n"
        f"1. Call exec_command exactly once with cmd={command!r} and max_output_tokens={EXEC_MAX_OUTPUT_TOKENS}; do not redirect output. Then make exactly two read_tool_output calls for this artifact: first mode=search query={fixture.shell_middle!r}, then mode=bytes using the returned match offset and limit=256.\n"
        f'2. Call native read_file exactly twice with exactly these identical arguments: {{"path":"{fixture.window_name}","offset":0,"max_bytes":32768,"max_lines":2000}}.\n'
        f"3. Make exactly two read_tool_output calls for the duplicate read_file artifact: first mode=search query={fixture.window_middle!r}, then mode=bytes using the returned match offset and limit=256. Do not use lines mode.\n"
        "4. Finish with exactly this success sentinel: TOOL_OUTPUT_SAVINGS_E2E_SUCCESS."
    )
