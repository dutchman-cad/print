#!/usr/bin/env python3
"""Check, preview, and release changelog fragments using Python's standard library."""

import argparse
from datetime import date
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
HEADINGS = (
    "Formats",
    "History",
    "Assemblies",
    "Desktop app",
    "Command line and install",
    "Reports",
    "FreeCAD output (experimental)",
    "cadmpeg",
    "Docs",
)


def fragments(root: Path) -> list[tuple[Path, str, str]]:
    """Read fragments in filename order, leaving their bullet wrapping intact."""
    result = []
    for path in sorted((root / "changelog.d").glob("*.md")):
        if path.name == "README.md":
            continue
        lines = path.read_text(encoding="utf-8").splitlines()
        first = lines[0] if lines else ""
        heading = first.removeprefix("### ")
        if not first.startswith("### ") or heading not in HEADINGS:
            raise ValueError(f"{path}: missing or unknown heading {first!r}; use ### "
                             + ", ### ".join(HEADINGS) + ".")
        content = "\n".join(lines[1:]).strip()
        if re.search(r"^- \S", content, re.MULTILINE) is None:
            raise ValueError(f"{path}: heading ### {heading} has no bullet; add a - item.")
        if re.search(r"^#{1,6}\s", content, re.MULTILINE):
            raise ValueError(f"{path}: heading ### {heading} must be the only heading.")
        if not content.startswith("- ") or any(
            line and not line.startswith(("- ", "  ")) for line in content.splitlines()
        ):
            raise ValueError(f"{path}: heading ### {heading} needs bullets with "
                             "two-space continuation indents.")
        result.append((path, heading, content))
    return result


def check(root: Path = ROOT) -> None:
    """Reject repeated topic headings in each version and malformed fragments."""
    version = "intro"
    seen = set()
    for line in (root / "CHANGELOG.md").read_text(encoding="utf-8").splitlines():
        if line.startswith("## "):
            version = line[3:].strip()
            seen.clear()
        elif line.startswith("### "):
            heading = line[4:].strip()
            if heading in seen:
                raise ValueError(f"CHANGELOG.md: {version}: duplicate heading ### {heading}.")
            seen.add(heading)
    fragments(root)


def assembled(entries: list[tuple[Path, str, str]]) -> str:
    return "\n\n".join(
        f"### {heading}\n\n" + "\n".join(
            content for _, topic, content in entries if topic == heading
        )
        for heading in HEADINGS if any(topic == heading for _, topic, _ in entries)
    )


def preview(root: Path = ROOT) -> str:
    content = assembled(fragments(root))
    return "## Unreleased\n" + (f"\n{content}\n" if content else "")


def release(version: str, release_date: str | None = None, root: Path = ROOT,
            write: bool = False) -> str:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("Use a release version in X.Y.Z form.")
    stamp = release_date if release_date is not None else date.today().isoformat()
    if not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", stamp):
        raise ValueError("Use a date in YYYY-MM-DD form.")
    date.fromisoformat(stamp)
    check(root)
    path = root / "CHANGELOG.md"
    changelog = path.read_text(encoding="utf-8")
    existing = re.search(rf"^## {re.escape(version)}(?:[ \t][^\n]*)?$", changelog, re.MULTILINE)
    if existing:
        if write:
            raise ValueError(f"CHANGELOG.md: version {version} already has an entry.")
        following = re.search(r"^## ", changelog[existing.end():], re.MULTILINE)
        end = existing.end() + following.start() if following else len(changelog)
        return changelog[existing.start():end].rstrip() + "\n"
    entries = fragments(root)
    if not entries:
        raise ValueError("changelog.d/: add a fragment before releasing.")
    newest = re.search(r"^## ", changelog, re.MULTILINE)
    if newest is None:
        raise ValueError("CHANGELOG.md: no version heading found for release insertion.")
    entry = f"## {version} ({stamp})\n\n{assembled(entries)}\n\n"
    if write:
        path.write_text(changelog[:newest.start()] + entry + changelog[newest.start():],
                        encoding="utf-8")
        for fragment, _, _ in entries:
            fragment.unlink()
    return entry.rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="Validate fragments and version headings")
    commands.add_parser("preview", help="Print the assembled Unreleased section")
    command = commands.add_parser("release", help="Print a version section without changing files")
    command.add_argument("version", metavar="X.Y.Z")
    command.add_argument("--date", dest="release_date", metavar="YYYY-MM-DD")
    command.add_argument("--write", action="store_true",
                         help="Also insert into CHANGELOG.md and delete included fragments")
    args = parser.parse_args()
    try:
        if args.command == "check":
            check()
        elif args.command == "preview":
            sys.stdout.write(preview())
        else:
            sys.stdout.write(release(args.version, args.release_date, write=args.write))
    except (OSError, ValueError) as error:
        print(f"Changelog: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
