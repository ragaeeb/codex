import hashlib
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tool_output_savings_code_mode_build import V8Assets
from tool_output_savings_code_mode_build import build_code_mode_release
from tool_output_savings_code_mode_build import rusty_v8_asset_names
from tool_output_savings_code_mode_build import rusty_v8_version
from tool_output_savings_code_mode_build import verify_rusty_v8_assets


class CodeModeBuildTests(unittest.TestCase):
    def test_pinned_v8_version_and_target_derive_official_asset_names(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "Cargo.toml"
            cargo.write_text('v8 = "=150.4.0"\n', encoding="utf-8")
            self.assertEqual(rusty_v8_version(cargo), "150.4.0")
        self.assertEqual(
            rusty_v8_asset_names("aarch64-apple-darwin"),
            (
                "librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz",
                "src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs",
                "rusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.sha256",
            ),
        )

    def test_checksum_manifest_must_cover_the_exact_archive_and_bindings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            names = rusty_v8_asset_names("aarch64-apple-darwin")
            archive, bindings, manifest = (root / name for name in names)
            archive.write_bytes(b"archive")
            bindings.write_bytes(b"bindings")
            manifest.write_text(
                f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n"
                f"{hashlib.sha256(bindings.read_bytes()).hexdigest()}  {bindings.name}\n",
                encoding="utf-8",
            )
            assets = verify_rusty_v8_assets(root, "150.4.0", "aarch64-apple-darwin")
            self.assertEqual(
                assets,
                V8Assets(
                    version="150.4.0",
                    target="aarch64-apple-darwin",
                    archive=archive,
                    bindings=bindings,
                    archive_sha256=hashlib.sha256(b"archive").hexdigest(),
                    bindings_sha256=hashlib.sha256(b"bindings").hexdigest(),
                ),
            )
            manifest.write_text(
                f"{'0' * 64}  {archive.name}\n"
                f"{hashlib.sha256(bindings.read_bytes()).hexdigest()}  {bindings.name}\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(Exception, "rusty_v8_checksum_mismatch"):
                verify_rusty_v8_assets(root, "150.4.0", "aarch64-apple-darwin")

    def test_code_mode_build_uses_verified_assets_and_builds_sibling_binaries(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "archive.gz"
            bindings = root / "bindings.rs"
            archive.write_bytes(b"archive")
            bindings.write_bytes(b"bindings")
            assets = V8Assets(
                version="150.4.0",
                target="aarch64-apple-darwin",
                archive=archive,
                bindings=bindings,
                archive_sha256=hashlib.sha256(b"archive").hexdigest(),
                bindings_sha256=hashlib.sha256(b"bindings").hexdigest(),
            )
            cli = root / "release" / "codex"
            host = root / "release" / "codex-code-mode-host"
            cli.parent.mkdir()
            cli.write_bytes(b"cli")
            host.write_bytes(b"host")
            cli.chmod(0o700)
            host.chmod(0o700)
            with (
                patch(
                    "tool_output_savings_code_mode_build.prepare_rusty_v8_assets",
                    return_value=assets,
                ),
                patch(
                    "tool_output_savings_code_mode_build.resolve_built_cli",
                    return_value=cli,
                ),
                patch(
                    "tool_output_savings_code_mode_build.resolve_built_code_mode_host",
                    return_value=host,
                ),
                patch("tool_output_savings_code_mode_build.run_bounded_command") as run,
            ):
                built = build_code_mode_release()
            self.assertEqual(built.cli, cli)
            self.assertEqual(built.code_mode_host, host)
            self.assertEqual(
                built.code_mode_host_sha256, hashlib.sha256(b"host").hexdigest()
            )
            command = run.call_args.args[0]
            self.assertEqual(command[:4], ["cargo", "build", "--locked", "--release"])
            self.assertIn("codex", command)
            self.assertIn("codex-code-mode-host", command)
            environment = run.call_args.kwargs["environment"]
            self.assertEqual(environment["RUSTY_V8_ARCHIVE"], str(archive))
            self.assertEqual(environment["RUSTY_V8_SRC_BINDING_PATH"], str(bindings))
            self.assertEqual(environment.get("PATH"), os.environ.get("PATH"))


if __name__ == "__main__":
    unittest.main()
