"""Command-line options and state-home selection for the live evaluator."""

import argparse
import os
from pathlib import Path

from tool_output_savings_fixture import DEFAULT_MODEL
from tool_output_savings_fixture import DEFAULT_REASONING_EFFORT


def resolve_codex_home(value: Path | None) -> tuple[Path, bool]:
    if value is not None:
        return value.expanduser().resolve(), True
    environment_home = os.environ.get("CODEX_HOME")
    if environment_home:
        return Path(environment_home).expanduser().resolve(), True
    return (Path.home() / ".codex").resolve(), False


def evaluation_lane(model_tool_mode: str) -> str:
    if model_tool_mode == "code_mode_only":
        return "luna_code_mode_stage1_and_usage"
    return "direct_stage1_stage2_projection"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Opt-in external E2E evaluation for bounded tool-output projections."
    )
    parser.add_argument(
        "--live",
        action="store_true",
        help="run the live external evaluation (macOS-only; --build required)",
    )
    parser.add_argument(
        "--stage3",
        action="store_true",
        help="run the one paired Stage 3 Luna Code Mode argument-repair lane",
    )
    parser.add_argument(
        "--codex-cli",
        type=Path,
        help="repo release executable; use --build for run-built binary evidence",
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="build the release CLI used by the live evaluation",
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL,
        help=f"catalog model (default: {DEFAULT_MODEL}); direct models use the separate Stage 2 projection lane",
    )
    parser.add_argument("--reasoning-effort", default=DEFAULT_REASONING_EFFORT)
    parser.add_argument("--codex-home", type=Path)
    parser.add_argument("--codex-db", type=Path)
    parser.add_argument(
        "--sqlite-home", type=Path, help="explicit sqlite_home config override"
    )
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument(
        "--keep", action="store_true", help="keep generated fixture and live traffic"
    )
    return parser
