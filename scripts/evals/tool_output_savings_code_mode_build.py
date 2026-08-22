"""Verified macOS arm64 Code Mode host build for the live savings lane."""

import hashlib
import os
import re
import tempfile
from dataclasses import dataclass
from pathlib import Path

from tool_output_savings_evidence import HarnessError
from tool_output_savings_evidence import require
from tool_output_savings_fixture import REPO_ROOT
from tool_output_savings_fixture import resolve_built_cli
from tool_output_savings_subprocess import run_bounded_capture
from tool_output_savings_subprocess import run_bounded_command


RUSTY_V8_REPOSITORY = "openai/codex"
RUSTY_V8_PROFILE = "ptrcomp_sandbox_release"
SUPPORTED_CODE_MODE_TARGET = "aarch64-apple-darwin"
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_BINDINGS_BYTES = 16 * 1024 * 1024
MAX_MANIFEST_BYTES = 64 * 1024
_V8_DEPENDENCY = re.compile(r'^v8\s*=\s*"=([^"\s]+)"\s*$', re.MULTILINE)
_CHECKSUM_LINE = re.compile(r"^([0-9a-f]{64})\s+[ *]?([^/\\]+)$")


@dataclass(frozen=True)
class V8Assets:
    version: str
    target: str
    archive: Path
    bindings: Path
    archive_sha256: str
    bindings_sha256: str


@dataclass(frozen=True)
class CodeModeBuild:
    cli: Path
    code_mode_host: Path
    code_mode_host_sha256: str
    rusty_v8_version: str
    rust_target: str
    rusty_v8_archive_sha256: str
    rusty_v8_bindings_sha256: str


def _sha256(path: Path, limit: int, rule: str) -> str:
    require(not path.is_symlink() and path.is_file(), rule)
    try:
        require(path.stat().st_size <= limit, rule)
        digest = hashlib.sha256()
        total = 0
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                total += len(chunk)
                require(total <= limit, rule)
                digest.update(chunk)
        return digest.hexdigest()
    except OSError as error:
        raise HarnessError(rule) from error


def rusty_v8_version(cargo_toml: Path) -> str:
    try:
        source = cargo_toml.read_text(encoding="utf-8")
    except OSError as error:
        raise HarnessError("rusty_v8_version_unavailable") from error
    matches = _V8_DEPENDENCY.findall(source)
    require(len(matches) == 1, "rusty_v8_version_unavailable")
    return matches[0]


def rust_build_target() -> str:
    configured = os.environ.get("CARGO_BUILD_TARGET")
    if configured:
        return configured
    result = run_bounded_capture(
        ["rustc", "-vV"],
        cwd=REPO_ROOT,
        timeout=30,
        rule="rust_target_unavailable",
    )
    require(result.returncode == 0, "rust_target_unavailable")
    try:
        hosts = [
            line.removeprefix("host: ")
            for line in result.stdout.decode("utf-8").splitlines()
            if line.startswith("host: ")
        ]
    except UnicodeError as error:
        raise HarnessError("rust_target_unavailable") from error
    require(len(hosts) == 1 and bool(hosts[0]), "rust_target_unavailable")
    return hosts[0]


def rusty_v8_asset_names(target: str) -> tuple[str, str, str]:
    require(target == SUPPORTED_CODE_MODE_TARGET, "code_mode_target_unsupported")
    return (
        f"librusty_v8_{RUSTY_V8_PROFILE}_{target}.a.gz",
        f"src_binding_{RUSTY_V8_PROFILE}_{target}.rs",
        f"rusty_v8_{RUSTY_V8_PROFILE}_{target}.sha256",
    )


def verify_rusty_v8_assets(root: Path, version: str, target: str) -> V8Assets:
    archive_name, bindings_name, manifest_name = rusty_v8_asset_names(target)
    archive = root / archive_name
    bindings = root / bindings_name
    manifest = root / manifest_name
    manifest_digest = _sha256(manifest, MAX_MANIFEST_BYTES, "rusty_v8_manifest_invalid")
    require(bool(manifest_digest), "rusty_v8_manifest_invalid")
    try:
        lines = manifest.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise HarnessError("rusty_v8_manifest_invalid") from error
    entries: dict[str, str] = {}
    for line in lines:
        match = _CHECKSUM_LINE.fullmatch(line.rstrip("\r"))
        require(match is not None, "rusty_v8_manifest_invalid")
        digest, name = match.groups()
        require(name not in entries, "rusty_v8_manifest_invalid")
        entries[name] = digest
    require(
        set(entries) == {archive_name, bindings_name},
        "rusty_v8_manifest_invalid",
    )
    archive_digest = _sha256(archive, MAX_ARCHIVE_BYTES, "rusty_v8_archive_invalid")
    bindings_digest = _sha256(bindings, MAX_BINDINGS_BYTES, "rusty_v8_bindings_invalid")
    require(
        entries[archive_name] == archive_digest
        and entries[bindings_name] == bindings_digest,
        "rusty_v8_checksum_mismatch",
    )
    return V8Assets(
        version=version,
        target=target,
        archive=archive,
        bindings=bindings,
        archive_sha256=archive_digest,
        bindings_sha256=bindings_digest,
    )


def prepare_rusty_v8_assets(root: Path) -> V8Assets:
    version = rusty_v8_version(REPO_ROOT / "codex-rs" / "Cargo.toml")
    target = rust_build_target()
    names = rusty_v8_asset_names(target)
    root.chmod(0o700)
    run_bounded_command(
        [
            "gh",
            "release",
            "download",
            f"rusty-v8-v{version}",
            "--repo",
            RUSTY_V8_REPOSITORY,
            "--dir",
            str(root),
            *(part for name in names for part in ("--pattern", name)),
        ],
        cwd=REPO_ROOT,
        timeout=300,
        rule="rusty_v8_asset_download_failed",
    )
    return verify_rusty_v8_assets(root, version, target)


def resolve_built_code_mode_host(cli: Path) -> Path:
    candidate = cli.parent / "codex-code-mode-host"
    require(
        not candidate.is_symlink()
        and candidate.is_file()
        and os.access(candidate, os.X_OK),
        "built_code_mode_host_missing",
    )
    return candidate.resolve()


def build_code_mode_release() -> CodeModeBuild:
    with tempfile.TemporaryDirectory(prefix="codex-rusty-v8-") as directory:
        assets = prepare_rusty_v8_assets(Path(directory))
        environment = os.environ.copy()
        environment.update(
            {
                "RUSTY_V8_ARCHIVE": str(assets.archive),
                "RUSTY_V8_SRC_BINDING_PATH": str(assets.bindings),
            }
        )
        run_bounded_command(
            [
                "cargo",
                "build",
                "--locked",
                "--release",
                "--bin",
                "codex",
                "--bin",
                "codex-code-mode-host",
            ],
            cwd=REPO_ROOT / "codex-rs",
            timeout=900,
            rule="release_code_mode_build_failed",
            environment=environment,
        )
        cli = resolve_built_cli()
        host = resolve_built_code_mode_host(cli)
        return CodeModeBuild(
            cli=cli,
            code_mode_host=host,
            code_mode_host_sha256=_sha256(
                host, MAX_ARCHIVE_BYTES, "code_mode_host_digest_failed"
            ),
            rusty_v8_version=assets.version,
            rust_target=assets.target,
            rusty_v8_archive_sha256=assets.archive_sha256,
            rusty_v8_bindings_sha256=assets.bindings_sha256,
        )
