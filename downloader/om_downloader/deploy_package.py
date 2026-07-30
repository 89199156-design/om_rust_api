from __future__ import annotations

import argparse
from pathlib import Path
from typing import Iterator
import zipfile


EXCLUDED_DIRS = {".git", ".venv", "__pycache__", "data", "build", "dist", "tests"}
EXCLUDED_SUFFIXES = {".pyc", ".pyo"}


def iter_deploy_files(root: Path) -> Iterator[Path]:
    root = root.resolve()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if path.is_dir():
            continue
        if any(part in EXCLUDED_DIRS for part in relative.parts):
            continue
        if path.suffix in EXCLUDED_SUFFIXES:
            continue
        yield relative


def create_deploy_zip(root: Path, output: Path) -> Path:
    root = root.resolve()
    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for relative in iter_deploy_files(root):
            archive.write(root / relative, relative.as_posix())
    return output


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=".")
    parser.add_argument("--output", required=True)
    args = parser.parse_args(argv)
    path = create_deploy_zip(Path(args.root), Path(args.output))
    print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
