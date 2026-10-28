#!/usr/bin/env python3
"""Package a built Print binary and release notes; write an adjacent SHA-256 file."""

import argparse
import hashlib
from pathlib import Path
import re
import tarfile
import zipfile

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def package(target: str, version: str, root: Path = Path(".")) -> Path:
    if target not in TARGETS or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("Use a supported target and an X.Y.Z version.")
    name = f"dutchman-print-{version}-{target}"
    binary = "dutchman-print" + (".exe" if "windows" in target else "")
    files = [(root / "target" / target / "release" / binary, binary)]
    files += [(root / item, item) for item in ("README.md", "LICENSE", "NOTICE")]
    files.append((root / "release-notes.md", "CHANGELOG.md"))
    for path, _ in files:
        if not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"Missing or empty package input: {path}")
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    archive = dist / (name + (".zip" if "windows" in target else ".tar.gz"))
    if "windows" in target:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for path, member in files:
                output.write(path, f"{name}/{member}")
    else:
        with tarfile.open(archive, "w:gz") as output:
            for path, member in files:
                output.add(path, arcname=f"{name}/{member}")
    checksum = hashlib.sha256()
    with archive.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    digest = checksum.hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8")
    return archive


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=TARGETS)
    parser.add_argument("version")
    args = parser.parse_args()
    try:
        print(package(args.target, args.version))
    except (OSError, ValueError) as error:
        parser.exit(1, f"Packaging: {error}\n")
